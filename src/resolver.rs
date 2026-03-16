use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::adapters::{ManagedFile, build_managed_files};
use crate::git::{
    current_rev, ensure_git_dependency, shared_checkout_path, shared_repository_path,
    validate_shared_checkout,
};
use crate::lockfile::{LOCKFILE_NAME, LockedPackage, LockedSource, Lockfile};
use crate::manifest::{
    DependencySourceKind, DependencySpec, LoadedManifest, PackageRole, load_dependency_from_dir,
    load_root_from_dir,
};
use crate::store::{snapshot_resolution, write_atomic};

#[derive(Debug, Clone)]
pub struct Resolution {
    pub project_root: PathBuf,
    pub packages: Vec<ResolvedPackage>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub alias: String,
    pub root: PathBuf,
    pub manifest: LoadedManifest,
    pub source: PackageSource,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    Root,
    Path {
        path: PathBuf,
        tag: Option<String>,
    },
    Git {
        url: String,
        tag: String,
        rev: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolveMode {
    Sync,
    Doctor,
}

#[derive(Debug, Default)]
struct ResolverState {
    stack: Vec<PathBuf>,
    resolved_by_path: HashMap<PathBuf, ResolvedPackage>,
}

#[derive(Debug, Clone, Copy)]
struct ResolveContext<'a> {
    cache_root: &'a Path,
    mode: ResolveMode,
}

pub fn sync(cache_root: &Path, locked: bool, allow_high_sensitivity: bool) -> Result<()> {
    let cwd = env::current_dir().context("failed to determine the current directory")?;
    sync_in_dir(&cwd, cache_root, locked, allow_high_sensitivity)
}

pub fn sync_in_dir(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
) -> Result<()> {
    let resolution = resolve_project(cwd, cache_root, ResolveMode::Sync)?;
    enforce_capabilities(&resolution, allow_high_sensitivity)?;
    let stored_packages = snapshot_resolution(&resolution)?;
    let lockfile_path = cwd.join(LOCKFILE_NAME);
    let existing_lockfile = if lockfile_path.exists() {
        Some(Lockfile::read(&lockfile_path)?)
    } else {
        None
    };

    let snapshot_by_digest = stored_packages
        .into_iter()
        .map(|stored| (stored.digest, stored.snapshot_root))
        .collect::<HashMap<_, _>>();
    let package_snapshots = resolution
        .packages
        .iter()
        .map(|package| {
            let snapshot_root = snapshot_by_digest
                .get(&package.digest)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing snapshot for {}", package.digest))?;
            Ok((package.clone(), snapshot_root))
        })
        .collect::<Result<Vec<_>>>()?;
    let planned_files = build_managed_files(cwd, &package_snapshots)?;
    let lockfile = resolution.to_lockfile(cwd, &planned_files)?;
    let owned_paths = load_owned_paths(cwd, existing_lockfile.as_ref())?;

    if locked {
        let Some(existing) = existing_lockfile.as_ref() else {
            bail!(
                "`--locked` requires an existing {} in {}",
                LOCKFILE_NAME,
                cwd.display()
            );
        };
        if *existing != lockfile {
            bail!(
                "{} is out of date; run `agen sync` without `--locked` to regenerate it",
                LOCKFILE_NAME
            );
        }
    }

    validate_collisions(&planned_files, &owned_paths)?;
    prune_stale_files(&owned_paths, &planned_files, cwd)?;
    write_managed_files(&planned_files)?;

    if !locked {
        lockfile.write(&lockfile_path)?;
    }

    for warning in &resolution.warnings {
        eprintln!("warning: {warning}");
    }

    Ok(())
}

pub fn doctor(cache_root: &Path) -> Result<()> {
    let cwd = env::current_dir().context("failed to determine the current directory")?;
    doctor_in_dir(&cwd, cache_root)
}

#[cfg(test)]
pub fn resolve_project_for_sync(root: &Path, cache_root: &Path) -> Result<Resolution> {
    resolve_project(root, cache_root, ResolveMode::Sync)
}

pub fn doctor_in_dir(cwd: &Path, cache_root: &Path) -> Result<()> {
    let resolution = resolve_project(cwd, cache_root, ResolveMode::Doctor)?;
    let lockfile_path = cwd.join(LOCKFILE_NAME);
    if !lockfile_path.exists() {
        bail!("missing {}", LOCKFILE_NAME);
    }

    let existing_lockfile = Lockfile::read(&lockfile_path)?;
    let package_roots = resolution
        .packages
        .iter()
        .map(|package| (package.clone(), package.root.clone()))
        .collect::<Vec<_>>();
    let planned_files = build_managed_files(cwd, &package_roots)?;
    let expected_lockfile = resolution.to_lockfile(cwd, &planned_files)?;
    if existing_lockfile != expected_lockfile {
        bail!("{LOCKFILE_NAME} is out of date");
    }
    let owned_paths = load_owned_paths(cwd, Some(&existing_lockfile))?;

    validate_collisions(&planned_files, &owned_paths)?;
    validate_state_consistency(&owned_paths, &planned_files)?;

    for package in &resolution.packages {
        if let PackageSource::Git { url, rev, .. } = &package.source {
            let checkout_path = shared_checkout_path(cache_root, url, rev)?;
            if package.root != checkout_path {
                bail!(
                    "git dependency `{}` resolved to {} instead of shared checkout {}",
                    package.alias,
                    package.root.display(),
                    checkout_path.display()
                );
            }
            let current = current_rev(&package.root)?;
            if current.trim() != rev {
                bail!(
                    "git dependency `{}` is checked out at {} instead of {}",
                    package.alias,
                    current.trim(),
                    rev
                );
            }

            let mirror_path = shared_repository_path(cache_root, url)?;
            validate_shared_checkout(&package.root, &mirror_path, url)?;
        }
    }

    for warning in &resolution.warnings {
        eprintln!("warning: {warning}");
    }

    Ok(())
}

fn resolve_project(root: &Path, cache_root: &Path, mode: ResolveMode) -> Result<Resolution> {
    let project_root = root
        .canonicalize()
        .with_context(|| format!("failed to access {}", root.display()))?;
    let context = ResolveContext { cache_root, mode };
    let mut state = ResolverState::default();
    resolve_package(
        &context,
        "root".to_string(),
        project_root.clone(),
        PackageSource::Root,
        PackageRole::Root,
        &mut state,
    )?;

    let mut packages: Vec<_> = state.resolved_by_path.into_values().collect();
    packages.sort_by(|left, right| {
        left.alias
            .cmp(&right.alias)
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
    context: &ResolveContext<'_>,
    alias: String,
    package_root: PathBuf,
    source: PackageSource,
    role: PackageRole,
    state: &mut ResolverState,
) -> Result<ResolvedPackage> {
    if let Some(existing) = state.resolved_by_path.get(&package_root) {
        return Ok(existing.clone());
    }

    if state.stack.iter().any(|path| path == &package_root) {
        let cycle = state
            .stack
            .iter()
            .chain(std::iter::once(&package_root))
            .map(|path| display_path(path))
            .collect::<Vec<_>>()
            .join(" -> ");
        bail!("dependency cycle detected: {cycle}");
    }

    state.stack.push(package_root.clone());

    let manifest = match role {
        PackageRole::Root => load_root_from_dir(&package_root)?,
        PackageRole::Dependency => load_dependency_from_dir(&package_root)?,
    };

    let dependencies = manifest
        .manifest
        .dependencies
        .iter()
        .map(|(dependency_alias, dependency)| {
            resolve_dependency(&manifest, dependency_alias, dependency, context, state)
        })
        .collect::<Result<Vec<_>>>()?;

    let digest = compute_package_digest(&manifest)?;
    let resolved = ResolvedPackage {
        alias,
        root: package_root.clone(),
        manifest,
        source,
        digest,
    };
    state
        .resolved_by_path
        .insert(package_root.clone(), resolved.clone());
    state.stack.pop();

    drop(dependencies);

    Ok(resolved)
}

fn resolve_dependency(
    parent: &LoadedManifest,
    alias: &str,
    dependency: &DependencySpec,
    context: &ResolveContext<'_>,
    state: &mut ResolverState,
) -> Result<ResolvedPackage> {
    match dependency.source_kind()? {
        DependencySourceKind::Path => {
            let path = dependency
                .path
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("dependency `{alias}` must declare `path`"))?;
            let dependency_root = parent
                .resolve_path(path)
                .with_context(|| format!("failed to resolve dependency `{alias}`"))?;
            let source = PackageSource::Path {
                path: dependency_root.clone(),
                tag: dependency.tag.clone(),
            };
            resolve_package(
                context,
                alias.to_string(),
                dependency_root,
                source,
                PackageRole::Dependency,
                state,
            )
        }
        DependencySourceKind::Git => {
            let url = dependency.url.as_deref().unwrap_or_default();
            let tag = dependency.tag.as_deref().unwrap_or_default();
            let checkout = ensure_git_dependency(
                context.cache_root,
                url,
                Some(tag),
                context.mode == ResolveMode::Sync,
            )?;
            let source = PackageSource::Git {
                url: checkout.url,
                tag: checkout.tag,
                rev: checkout.rev,
            };
            resolve_package(
                context,
                alias.to_string(),
                checkout.path,
                source,
                PackageRole::Dependency,
                state,
            )
        }
    }
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
    pub fn to_lockfile(
        &self,
        project_root: &Path,
        planned_files: &[ManagedFile],
    ) -> Result<Lockfile> {
        let mut packages = Vec::new();

        for package in &self.packages {
            let source = match &package.source {
                PackageSource::Root => LockedSource {
                    kind: "path".into(),
                    path: Some(".".into()),
                    url: None,
                    tag: None,
                    rev: None,
                },
                PackageSource::Path { path, tag } => LockedSource {
                    kind: "path".into(),
                    path: Some(display_path(
                        path.strip_prefix(&self.project_root).unwrap_or(path),
                    )),
                    url: None,
                    tag: tag.clone(),
                    rev: None,
                },
                PackageSource::Git { url, tag, rev } => LockedSource {
                    kind: "git".into(),
                    path: None,
                    url: Some(url.clone()),
                    tag: Some(tag.clone()),
                    rev: Some(rev.clone()),
                },
            };

            let mut dependencies: Vec<_> = package
                .manifest
                .manifest
                .dependencies
                .keys()
                .cloned()
                .collect();
            dependencies.sort();

            packages.push(LockedPackage {
                alias: package.alias.clone(),
                name: package.manifest.effective_name(),
                version_tag: match &package.source {
                    PackageSource::Git { tag, .. } => Some(tag.clone()),
                    PackageSource::Path { tag, .. } => tag.clone(),
                    PackageSource::Root => {
                        package.manifest.effective_version().map(|v| v.to_string())
                    }
                },
                source,
                digest: package.digest.clone(),
                skills: sorted_ids(
                    package
                        .manifest
                        .discovered
                        .skills
                        .iter()
                        .map(|item| &item.id),
                ),
                agents: sorted_ids(
                    package
                        .manifest
                        .discovered
                        .agents
                        .iter()
                        .map(|item| &item.id),
                ),
                rules: sorted_ids(
                    package
                        .manifest
                        .discovered
                        .rules
                        .iter()
                        .map(|item| &item.id),
                ),
                commands: sorted_ids(
                    package
                        .manifest
                        .discovered
                        .commands
                        .iter()
                        .map(|item| &item.id),
                ),
                dependencies,
                capabilities: package.manifest.manifest.capabilities.clone(),
            });
        }

        let managed_files = planned_files
            .iter()
            .map(|file| Lockfile::normalize_relative(project_root, &file.path))
            .collect::<Result<Vec<_>>>()?;

        Ok(Lockfile::new(packages, managed_files))
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

fn enforce_capabilities(resolution: &Resolution, allow_high_sensitivity: bool) -> Result<()> {
    let mut high_sensitivity = Vec::new();

    for package in &resolution.packages {
        for capability in &package.manifest.manifest.capabilities {
            eprintln!(
                "capability: {} {} ({})",
                package.alias, capability.id, capability.sensitivity
            );
            if capability.sensitivity.eq_ignore_ascii_case("high") {
                high_sensitivity.push(format!("{}:{}", package.alias, capability.id));
            }
        }
    }

    if !high_sensitivity.is_empty() && !allow_high_sensitivity {
        high_sensitivity.sort();
        bail!(
            "high-sensitivity capabilities require --allow-high-sensitivity: {}",
            high_sensitivity.join(", ")
        );
    }

    Ok(())
}

fn validate_collisions(
    planned_files: &[ManagedFile],
    owned_paths: &HashSet<PathBuf>,
) -> Result<()> {
    for file in planned_files {
        if file.path.exists() && !owned_paths.contains(&file.path) {
            bail!(
                "refusing to overwrite unmanaged file {}",
                file.path.display()
            );
        }
    }

    Ok(())
}

fn prune_stale_files(
    owned_paths: &HashSet<PathBuf>,
    planned_files: &[ManagedFile],
    project_root: &Path,
) -> Result<()> {
    let desired_paths = planned_files
        .iter()
        .map(|file| file.path.clone())
        .collect::<HashSet<_>>();

    for path in owned_paths.difference(&desired_paths) {
        if path.exists() {
            fs::remove_file(path).with_context(|| {
                format!("failed to remove stale managed file {}", path.display())
            })?;
            prune_empty_parent_dirs(path, project_root)?;
        }
    }

    Ok(())
}

fn write_managed_files(planned_files: &[ManagedFile]) -> Result<()> {
    for file in planned_files {
        write_atomic(&file.path, &file.contents)
            .with_context(|| format!("failed to write managed file {}", file.path.display()))?;
    }
    Ok(())
}

fn validate_state_consistency(
    owned_paths: &HashSet<PathBuf>,
    planned_files: &[ManagedFile],
) -> Result<()> {
    let desired_paths = planned_files
        .iter()
        .map(|file| file.path.clone())
        .collect::<HashSet<_>>();

    if let Some(path) = owned_paths.difference(&desired_paths).next() {
        bail!("stale managed state entry for {}", path.display());
    }

    for path in desired_paths.intersection(owned_paths) {
        if !path.exists() {
            bail!("managed file is missing from disk: {}", path.display());
        }
    }

    Ok(())
}

fn load_owned_paths(project_root: &Path, lockfile: Option<&Lockfile>) -> Result<HashSet<PathBuf>> {
    if let Some(lockfile) = lockfile {
        return lockfile.managed_paths(project_root);
    }

    Ok(HashSet::new())
}

fn prune_empty_parent_dirs(path: &Path, project_root: &Path) -> Result<()> {
    let stop_roots = [
        project_root.join(".claude"),
        project_root.join(".codex"),
        project_root.join(".opencode"),
        project_root.join(".agen"),
    ];
    let mut current = path.parent();

    while let Some(dir) = current {
        if stop_roots.iter().any(|root| dir == root) {
            break;
        }
        match fs::remove_dir(dir) {
            Ok(()) => {
                current = dir.parent();
            }
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to prune {}", dir.display()));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;
    use crate::adapters::namespaced_skill_id;
    use crate::git::{add_dependency_in_dir, shared_checkout_path, shared_repository_path};
    use crate::manifest::{MANIFEST_FILE, load_root_from_dir};

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

    fn init_git_repo(path: &Path) {
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };

        run(&["init"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test User"]);
        run(&["add", "."]);
        run(&["commit", "-m", "initial"]);
    }

    fn create_git_dependency() -> (TempDir, String) {
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        write_file(&repo.path().join("agents/security.md"), "# Security\n");
        init_git_repo(repo.path());

        let output = Command::new("git")
            .args(["tag", "v0.1.0"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let url = repo.path().to_string_lossy().to_string();
        (repo, url)
    }

    fn cache_dir() -> TempDir {
        TempDir::new().unwrap()
    }

    fn git_output(path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn resolves_local_path_dependencies_with_discovery() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
        );

        write_skill(&temp.path().join("vendor/shared/skills/checks"), "Checks");

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let planned_files = build_managed_files(
            temp.path(),
            &resolution
                .packages
                .iter()
                .map(|package| (package.clone(), package.root.clone()))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let lockfile = resolution.to_lockfile(temp.path(), &planned_files).unwrap();

        assert_eq!(lockfile.packages.len(), 2);
        assert_eq!(lockfile.packages[0].alias, "root");
        assert_eq!(lockfile.packages[1].alias, "shared");
    }

    #[test]
    fn add_dependency_clones_repo_and_updates_manifest() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        let (_repo, url) = create_git_dependency();

        add_dependency_in_dir(temp.path(), cache.path(), &url, Some("v0.1.0")).unwrap();

        let mirror_path = shared_repository_path(cache.path(), &url).unwrap();
        let rev = git_output(&mirror_path, &["rev-parse", "v0.1.0^{commit}"]);
        let checkout_path = shared_checkout_path(cache.path(), &url, &rev).unwrap();
        assert!(mirror_path.exists());
        assert!(checkout_path.exists());
        assert_eq!(
            git_output(&mirror_path, &["rev-parse", "--is-bare-repository"]),
            "true"
        );
        assert_eq!(
            git_output(
                &checkout_path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"]
            ),
            mirror_path.canonicalize().unwrap().to_string_lossy()
        );
        let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
        assert!(manifest.contains("[dependencies]"));
        assert!(manifest.contains("tag = \"v0.1.0\""));
        assert!(manifest.contains("url = "));
        let lockfile = Lockfile::read(&temp.path().join("agentpack.lock")).unwrap();
        assert!(!lockfile.managed_files.is_empty());

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let dependency = resolution
            .packages
            .iter()
            .find(|package| package.alias != "root")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(dependency, "review");
        assert!(
            temp.path()
                .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
    }

    #[test]
    fn add_dependency_uses_latest_tag_when_not_provided() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        init_git_repo(repo.path());

        for tag in ["v0.1.0", "v1.2.0", "v0.9.0"] {
            let output = Command::new("git")
                .args(["tag", tag])
                .current_dir(repo.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        add_dependency_in_dir(
            temp.path(),
            cache.path(),
            &repo.path().to_string_lossy(),
            None,
        )
        .unwrap();

        let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
        assert!(manifest.contains("tag = \"v1.2.0\""));
    }

    #[test]
    fn add_dependency_rejects_repo_without_supported_directories() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        let repo = TempDir::new().unwrap();
        write_file(&repo.path().join("README.md"), "hello\n");
        init_git_repo(repo.path());
        Command::new("git")
            .args(["tag", "v0.1.0"])
            .current_dir(repo.path())
            .output()
            .unwrap();

        let error = add_dependency_in_dir(
            temp.path(),
            cache.path(),
            &repo.path().to_string_lossy(),
            Some("v0.1.0"),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("does not match the Agen package layout"));
    }

    #[test]
    fn sync_writes_runtime_outputs_from_discovered_layout() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");
        write_file(&temp.path().join("agents/security.md"), "# Security\n");
        write_file(&temp.path().join("rules/default.rules"), "allow = []\n");
        write_file(&temp.path().join("AGENTS.md"), "user-owned instructions\n");
        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let root_package = resolution
            .packages
            .iter()
            .find(|package| package.alias == "root")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(root_package, "review");

        sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

        assert!(
            temp.path()
                .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".codex/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(temp.path().join(".codex/rules/default.rules").exists());
        assert!(
            temp.path()
                .join(".opencode/instructions/security.md")
                .exists()
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("AGENTS.md")).unwrap(),
            "user-owned instructions\n"
        );
    }

    #[test]
    fn sync_requires_opt_in_for_high_sensitivity_capabilities() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[[capabilities]]
id = "shell.exec"
sensitivity = "high"
"#,
        );
        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let root_package = resolution
            .packages
            .iter()
            .find(|package| package.alias == "root")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(root_package, "review");

        let error = sync_in_dir(temp.path(), cache.path(), false, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--allow-high-sensitivity"));

        sync_in_dir(temp.path(), cache.path(), false, true).unwrap();
        assert!(
            temp.path()
                .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
    }

    #[test]
    fn sync_uses_short_git_revision_suffix_for_dependency_skills() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        let (_repo, url) = create_git_dependency();

        add_dependency_in_dir(temp.path(), cache.path(), &url, Some("v0.1.0")).unwrap();
        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let dependency = resolution
            .packages
            .iter()
            .find(|package| matches!(package.source, PackageSource::Git { .. }))
            .unwrap();
        let managed_skill_id = namespaced_skill_id(dependency, "review");

        sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

        assert!(managed_skill_id.starts_with("review_"));
        assert_eq!(managed_skill_id.len(), "review_".len() + 6);
        assert!(
            temp.path()
                .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
    }

    #[test]
    fn sync_prunes_stale_managed_files() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();

        write_skill(&temp.path().join("skills/review"), "Review");
        write_file(&temp.path().join("agents/security.md"), "# Security\n");

        sync_in_dir(temp.path(), cache.path(), false, false).unwrap();
        assert!(
            temp.path()
                .join(".opencode/instructions/security.md")
                .exists()
        );

        fs::remove_file(temp.path().join("agents/security.md")).unwrap();
        fs::remove_dir(temp.path().join("agents")).unwrap();
        sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

        assert!(
            !temp
                .path()
                .join(".opencode/instructions/security.md")
                .exists()
        );
    }

    #[test]
    fn doctor_detects_lockfile_drift() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");
        sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

        write_skill(&temp.path().join("skills/renamed"), "Renamed");

        let error = doctor_in_dir(temp.path(), cache.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("out of date"));
    }

    #[test]
    fn shared_cache_is_reused_across_multiple_projects() {
        let cache = cache_dir();
        let project_one = TempDir::new().unwrap();
        let project_two = TempDir::new().unwrap();
        let (_repo, url) = create_git_dependency();

        add_dependency_in_dir(project_one.path(), cache.path(), &url, Some("v0.1.0")).unwrap();
        add_dependency_in_dir(project_two.path(), cache.path(), &url, Some("v0.1.0")).unwrap();

        let mirror_path = shared_repository_path(cache.path(), &url).unwrap();
        let rev = git_output(&mirror_path, &["rev-parse", "v0.1.0^{commit}"]);
        let checkout_path = shared_checkout_path(cache.path(), &url, &rev).unwrap();
        assert!(mirror_path.exists());
        assert!(checkout_path.exists());
        assert_eq!(
            git_output(
                &checkout_path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"]
            ),
            mirror_path.canonicalize().unwrap().to_string_lossy()
        );
        let resolution_one =
            resolve_project(project_one.path(), cache.path(), ResolveMode::Sync).unwrap();
        let resolution_two =
            resolve_project(project_two.path(), cache.path(), ResolveMode::Sync).unwrap();
        assert_eq!(
            resolution_one
                .packages
                .iter()
                .find(|package| matches!(package.source, PackageSource::Git { .. }))
                .unwrap()
                .root,
            checkout_path
        );
        assert_eq!(
            resolution_two
                .packages
                .iter()
                .find(|package| matches!(package.source, PackageSource::Git { .. }))
                .unwrap()
                .root,
            checkout_path
        );
    }

    #[test]
    fn custom_cache_root_routes_shared_repositories_into_the_override_directory() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        let (_repo, url) = create_git_dependency();

        add_dependency_in_dir(temp.path(), cache.path(), &url, Some("v0.1.0")).unwrap();

        assert!(shared_repository_path(cache.path(), &url).unwrap().exists());
    }

    #[test]
    fn doctor_accepts_shared_mirror_backed_checkouts() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        let (_repo, url) = create_git_dependency();

        add_dependency_in_dir(temp.path(), cache.path(), &url, Some("v0.1.0")).unwrap();

        doctor_in_dir(temp.path(), cache.path()).unwrap();
    }

    #[test]
    fn root_manifest_can_be_missing() {
        let temp = TempDir::new().unwrap();
        write_skill(&temp.path().join("skills/review"), "Review");

        let loaded = load_root_from_dir(temp.path()).unwrap();
        assert!(loaded.manifest.dependencies.is_empty());
        assert_eq!(loaded.discovered.skills[0].id, "review");
    }
}
