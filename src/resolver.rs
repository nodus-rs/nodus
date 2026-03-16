use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::lockfile::{LOCKFILE_NAME, LockedPackage, LockedSource, Lockfile};
use crate::manifest::{DependencySpec, LoadedManifest, load_from_dir};

#[derive(Debug, Clone)]
pub struct Resolution {
    pub project_root: PathBuf,
    pub packages: Vec<ResolvedPackage>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub root: PathBuf,
    pub manifest: LoadedManifest,
    pub digest: String,
}

#[derive(Debug, Clone)]
struct SeenPackage {
    version: semver::Version,
    digest: String,
    root: PathBuf,
}

#[derive(Debug, Default)]
struct ResolverState {
    stack: Vec<PathBuf>,
    resolved_by_path: HashMap<PathBuf, ResolvedPackage>,
    seen_by_name: BTreeMap<String, SeenPackage>,
}

pub fn sync(locked: bool, _allow_high_sensitivity: bool) -> Result<()> {
    let cwd = env::current_dir().context("failed to determine the current directory")?;
    let resolution = resolve_project(&cwd)?;
    let lockfile = resolution.to_lockfile()?;
    let lockfile_path = cwd.join(LOCKFILE_NAME);

    if locked {
        if !lockfile_path.exists() {
            bail!(
                "`--locked` requires an existing {} in {}",
                LOCKFILE_NAME,
                cwd.display()
            );
        }

        let existing = Lockfile::read(&lockfile_path)?;
        if existing != lockfile {
            bail!(
                "{} is out of date; run `agen sync` without `--locked` to regenerate it",
                LOCKFILE_NAME
            );
        }
    } else {
        lockfile.write(&lockfile_path)?;
    }

    for warning in &resolution.warnings {
        eprintln!("warning: {warning}");
    }

    Ok(())
}

pub fn doctor() -> Result<()> {
    bail!("doctor is not implemented yet")
}

pub fn resolve_project(root: &Path) -> Result<Resolution> {
    let project_root = root
        .canonicalize()
        .with_context(|| format!("failed to access {}", root.display()))?;
    let mut state = ResolverState::default();
    resolve_package(&project_root, &project_root, &mut state)?;

    let mut packages: Vec<_> = state.resolved_by_path.into_values().collect();
    packages.sort_by(|left, right| {
        left.manifest
            .manifest
            .name
            .cmp(&right.manifest.manifest.name)
            .then(
                left.manifest
                    .manifest
                    .version
                    .cmp(&right.manifest.manifest.version),
            )
            .then(left.root.cmp(&right.root))
    });

    let warnings = packages
        .iter()
        .flat_map(|package| package.manifest.warnings.iter().cloned())
        .collect();

    Ok(Resolution {
        project_root,
        packages,
        warnings,
    })
}

fn resolve_package(
    project_root: &Path,
    package_root: &Path,
    state: &mut ResolverState,
) -> Result<ResolvedPackage> {
    if let Some(existing) = state.resolved_by_path.get(package_root) {
        return Ok(existing.clone());
    }

    if state.stack.iter().any(|path| path == package_root) {
        let cycle = state
            .stack
            .iter()
            .chain(std::iter::once(&package_root.to_path_buf()))
            .map(|path| display_path(path))
            .collect::<Vec<_>>()
            .join(" -> ");
        bail!("dependency cycle detected: {cycle}");
    }

    state.stack.push(package_root.to_path_buf());

    let manifest = load_from_dir(package_root)?;
    let dependency_paths = manifest
        .manifest
        .dependencies
        .agentpacks
        .iter()
        .map(|(name, dependency)| resolve_dependency(&manifest, name, dependency))
        .collect::<Result<Vec<_>>>()?;

    let mut dependency_names = HashSet::new();
    for (name, dependency_root) in &dependency_paths {
        if !dependency_names.insert(name.clone()) {
            bail!(
                "duplicate dependency alias `{name}` in {}",
                manifest.root.display()
            );
        }
        let dependency = resolve_package(project_root, dependency_root, state)?;
        validate_dependency_requirement(
            &manifest,
            name,
            &dependency.manifest.manifest,
            project_root,
        )?;
    }

    let digest = compute_package_digest(&manifest)?;
    register_package_identity(
        &manifest.manifest.name,
        &manifest.manifest.version,
        &digest,
        &manifest.root,
        &mut state.seen_by_name,
    )?;

    let resolved = ResolvedPackage {
        root: package_root.to_path_buf(),
        manifest,
        digest,
    };
    state
        .resolved_by_path
        .insert(package_root.to_path_buf(), resolved.clone());
    state.stack.pop();

    Ok(resolved)
}

fn resolve_dependency(
    manifest: &LoadedManifest,
    name: &str,
    dependency: &DependencySpec,
) -> Result<(String, PathBuf)> {
    let dependency_root = manifest
        .resolve_path(&dependency.path)
        .with_context(|| format!("failed to resolve dependency `{name}`"))?;
    if !dependency_root.starts_with(&manifest.root) {
        bail!(
            "dependency `{name}` path `{}` escapes the package root {}",
            dependency.path.display(),
            manifest.root.display()
        );
    }
    Ok((name.to_string(), dependency_root))
}

fn validate_dependency_requirement(
    parent: &LoadedManifest,
    alias: &str,
    dependency_manifest: &crate::manifest::Manifest,
    project_root: &Path,
) -> Result<()> {
    let Some(spec) = parent.manifest.dependencies.agentpacks.get(alias) else {
        bail!("missing dependency metadata for `{alias}`");
    };

    if let Some(requirement) = &spec.requirement {
        let parsed = semver::VersionReq::parse(requirement).with_context(|| {
            format!(
                "dependency `{alias}` in {} has an invalid semver requirement `{requirement}`",
                display_path(
                    &parent
                        .root
                        .strip_prefix(project_root)
                        .unwrap_or(&parent.root)
                )
            )
        })?;
        if !parsed.matches(&dependency_manifest.version) {
            bail!(
                "dependency `{alias}` requires `{}` but resolved {} {}",
                requirement,
                dependency_manifest.name,
                dependency_manifest.version
            );
        }
    }

    Ok(())
}

fn register_package_identity(
    name: &str,
    version: &semver::Version,
    digest: &str,
    root: &Path,
    seen_by_name: &mut BTreeMap<String, SeenPackage>,
) -> Result<()> {
    if let Some(existing) = seen_by_name.get(name) {
        if existing.version != *version {
            bail!(
                "conflicting versions for package `{name}`: {} at {} and {} at {}",
                existing.version,
                display_path(&existing.root),
                version,
                display_path(root)
            );
        }
        if existing.digest != digest {
            bail!(
                "package `{name}` version {} resolves to different contents at {} and {}",
                version,
                display_path(&existing.root),
                display_path(root)
            );
        }
    } else {
        seen_by_name.insert(
            name.to_string(),
            SeenPackage {
                version: version.clone(),
                digest: digest.to_string(),
                root: root.to_path_buf(),
            },
        );
    }

    Ok(())
}

fn compute_package_digest(manifest: &LoadedManifest) -> Result<String> {
    let mut files = manifest.package_files()?;
    files.sort();

    let mut hasher = Sha256::new();
    for file in files {
        let relative = file
            .strip_prefix(&manifest.root)
            .with_context(|| format!("failed to make {} relative", file.display()))?;
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(
            fs::read(&file)
                .with_context(|| format!("failed to read {} for hashing", file.display()))?,
        );
        hasher.update([0xff]);
    }

    Ok(format!("sha256:{:x}", hasher.finalize()))
}

impl Resolution {
    pub fn to_lockfile(&self) -> Result<Lockfile> {
        let mut packages = Vec::new();

        for package in &self.packages {
            let relative_root = package
                .root
                .strip_prefix(&self.project_root)
                .unwrap_or(&package.root);
            let source_path = if relative_root.as_os_str().is_empty() {
                ".".to_string()
            } else {
                display_path(relative_root)
            };

            let manifest = &package.manifest.manifest;
            let mut dependencies: Vec<_> =
                manifest.dependencies.agentpacks.keys().cloned().collect();
            dependencies.sort();

            packages.push(LockedPackage {
                name: manifest.name.clone(),
                package_version: manifest.version.clone(),
                source: LockedSource {
                    kind: "path".into(),
                    path: source_path,
                },
                digest: package.digest.clone(),
                skills: sorted_ids(manifest.exports.skills.iter().map(|item| &item.id)),
                agents: sorted_ids(manifest.exports.agents.iter().map(|item| &item.id)),
                rules: sorted_ids(manifest.exports.rules.iter().map(|item| &item.id)),
                dependencies,
                capabilities: manifest.capabilities.clone(),
            });
        }

        Ok(Lockfile::new(packages))
    }
}

fn sorted_ids<'a>(ids: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut ids: Vec<_> = ids.cloned().collect();
    ids.sort();
    ids
}

fn display_path(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        ".".into()
    } else {
        path.to_string_lossy().replace('\\', "/")
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::TempDir;

    use super::*;
    use crate::manifest::MANIFEST_FILE;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::File::create(path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }

    fn write_skill(path: &Path, name: &str) {
        write_file(
            &path.join("SKILL.md"),
            &format!("---\nname: {name}\ndescription: Example skill.\n---\n# {name}\n"),
        );
    }

    #[test]
    fn resolves_local_dependencies_into_a_deterministic_lockfile() {
        let temp = TempDir::new().unwrap();

        write_skill(&temp.path().join("skills/review"), "Review");
        write_file(&temp.path().join("rules/default.rules"), "allow = []\n");
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
api_version = "agentpack/v0"
name = "root"
version = "0.1.0"

[[exports.skills]]
id = "review"
path = "skills/review"

[[exports.rules]]
id = "default"

[[exports.rules.sources]]
type = "codex.ruleset"
path = "rules/default.rules"

[dependencies.agentpacks.shared]
path = "vendor/shared"
requirement = "^1.0.0"
"#,
        );

        write_skill(&temp.path().join("vendor/shared/skills/checks"), "Checks");
        write_file(
            &temp.path().join("vendor/shared/agentpack.toml"),
            r#"
api_version = "agentpack/v0"
name = "shared"
version = "1.2.0"

[[exports.skills]]
id = "checks"
path = "skills/checks"
"#,
        );

        let resolution = resolve_project(temp.path()).unwrap();
        let lockfile = resolution.to_lockfile().unwrap();

        assert_eq!(lockfile.packages.len(), 2);
        assert_eq!(lockfile.packages[0].name, "root");
        assert_eq!(lockfile.packages[1].name, "shared");
    }

    #[test]
    fn rejects_dependency_cycles() {
        let temp = TempDir::new().unwrap();

        write_skill(&temp.path().join("skills/review"), "Review");
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
api_version = "agentpack/v0"
name = "root"
version = "0.1.0"

[[exports.skills]]
id = "review"
path = "skills/review"

[dependencies.agentpacks.self_ref]
path = "."
"#,
        );

        let error = resolve_project(temp.path()).unwrap_err().to_string();
        assert!(error.contains("dependency cycle detected"));
    }

    #[test]
    fn rejects_conflicting_versions_for_the_same_package_name() {
        let temp = TempDir::new().unwrap();

        write_skill(&temp.path().join("skills/review"), "Review");
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
api_version = "agentpack/v0"
name = "root"
version = "0.1.0"

[[exports.skills]]
id = "review"
path = "skills/review"

[dependencies.agentpacks.one]
path = "deps/one"

[dependencies.agentpacks.two]
path = "deps/two"
"#,
        );

        for (path, version) in [("deps/one", "1.0.0"), ("deps/two", "2.0.0")] {
            write_skill(&temp.path().join(path).join("skills/checks"), "Checks");
            write_file(
                &temp.path().join(path).join(MANIFEST_FILE),
                &format!(
                    r#"
api_version = "agentpack/v0"
name = "shared"
version = "{version}"

[[exports.skills]]
id = "checks"
path = "skills/checks"
"#
                ),
            );
        }

        let error = resolve_project(temp.path()).unwrap_err().to_string();
        assert!(error.contains("conflicting versions for package `shared`"));
    }
}
