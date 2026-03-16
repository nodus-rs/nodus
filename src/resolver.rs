use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::adapters::{Adapter, Adapters, ManagedFile, build_output_plan};
use crate::git::{
    current_rev, ensure_git_dependency, shared_checkout_path, shared_repository_path,
    validate_shared_checkout,
};
use crate::lockfile::{LOCKFILE_NAME, LockedPackage, LockedSource, Lockfile};
use crate::manifest::{
    DependencyComponent, DependencySourceKind, DependencySpec, LoadedManifest, PackageRole,
    load_dependency_from_dir, load_root_from_dir, write_manifest,
};
use crate::report::Reporter;
use crate::selection::{resolve_adapter_selection, should_prompt_for_adapter};
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
    pub selected_components: Option<Vec<DependencyComponent>>,
}

#[derive(Debug, Clone)]
pub struct SyncSummary {
    pub package_count: usize,
    pub adapters: Vec<Adapter>,
    pub managed_file_count: usize,
}

#[derive(Debug, Clone)]
pub struct DoctorSummary {
    pub package_count: usize,
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

#[derive(Clone, Copy)]
struct ResolveContext<'a> {
    cache_root: &'a Path,
    mode: ResolveMode,
    reporter: &'a Reporter,
}

#[allow(dead_code)]
pub fn sync_with_adapters(
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    adapters: &[crate::adapters::Adapter],
    reporter: &Reporter,
) -> Result<SyncSummary> {
    let cwd = env::current_dir().context("failed to determine the current directory")?;
    sync_in_dir_with_adapters(
        &cwd,
        cache_root,
        locked,
        allow_high_sensitivity,
        adapters,
        reporter,
    )
}

pub fn sync_in_dir(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters(
        cwd,
        cache_root,
        locked,
        allow_high_sensitivity,
        &[],
        reporter,
    )
}

pub fn sync_in_dir_with_adapters(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    adapters: &[Adapter],
    reporter: &Reporter,
) -> Result<SyncSummary> {
    let mut root = load_root_from_dir(cwd)?;
    let selection = resolve_adapter_selection(
        cwd,
        &root.manifest,
        adapters,
        !locked && should_prompt_for_adapter(),
    )?;
    if selection.should_persist {
        if locked {
            bail!(
                "adapter selection must be persisted before running `nodus sync --locked`; rerun without `--locked` or set `[adapters] enabled = [...]` in nodus.toml"
            );
        }
        root.manifest.set_enabled_adapters(&selection.adapters);
        reporter.status(
            "Writing",
            cwd.join(crate::manifest::MANIFEST_FILE).display(),
        )?;
        write_manifest(&cwd.join(crate::manifest::MANIFEST_FILE), &root.manifest)?;
    }

    reporter.status("Resolving", format!("package graph in {}", cwd.display()))?;
    let resolution = resolve_project(cwd, cache_root, ResolveMode::Sync, reporter)?;
    reporter.status("Checking", "declared capabilities")?;
    enforce_capabilities(&resolution, allow_high_sensitivity, reporter)?;
    reporter.status(
        "Snapshotting",
        format!("{} packages", resolution.packages.len()),
    )?;
    let stored_packages = snapshot_resolution(cache_root, &resolution)?;
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
    let selected_adapters = Adapters::from_slice(&selection.adapters);
    let output_plan = build_output_plan(cwd, &package_snapshots, selected_adapters)?;
    let planned_files = &output_plan.files;
    let desired_paths = resolution.managed_paths(cwd, selected_adapters)?;
    let lockfile = resolution.to_lockfile(selected_adapters)?;
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
                "{} is out of date; run `nodus sync` without `--locked` to regenerate it",
                LOCKFILE_NAME
            );
        }
    }

    validate_collisions(planned_files, &owned_paths)?;
    prune_stale_files(&owned_paths, &desired_paths, cwd)?;
    reporter.status("Writing", "managed runtime outputs")?;
    write_managed_files(planned_files)?;

    if !locked {
        reporter.status("Writing", lockfile_path.display())?;
        lockfile.write(&lockfile_path)?;
    }

    for warning in resolution
        .warnings
        .iter()
        .chain(output_plan.warnings.iter())
    {
        reporter.warning(warning)?;
    }

    Ok(SyncSummary {
        package_count: resolution.packages.len(),
        adapters: selection.adapters,
        managed_file_count: planned_files.len(),
    })
}

#[allow(dead_code)]
pub fn doctor(cache_root: &Path, reporter: &Reporter) -> Result<DoctorSummary> {
    let cwd = env::current_dir().context("failed to determine the current directory")?;
    doctor_in_dir(&cwd, cache_root, reporter)
}

#[cfg(test)]
pub fn resolve_project_for_sync(
    root: &Path,
    cache_root: &Path,
    reporter: &Reporter,
) -> Result<Resolution> {
    resolve_project(root, cache_root, ResolveMode::Sync, reporter)
}

pub fn doctor_in_dir(cwd: &Path, cache_root: &Path, reporter: &Reporter) -> Result<DoctorSummary> {
    let root = load_root_from_dir(cwd)?;
    let selection = resolve_adapter_selection(cwd, &root.manifest, &[], false)?;
    let selected_adapters = Adapters::from_slice(&selection.adapters);
    reporter.status(
        "Checking",
        "manifest, lockfile, shared store, and managed outputs",
    )?;
    let resolution = resolve_project(cwd, cache_root, ResolveMode::Doctor, reporter)?;
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
    let output_plan = build_output_plan(cwd, &package_roots, selected_adapters)?;
    let planned_files = &output_plan.files;
    let desired_paths = resolution.managed_paths(cwd, selected_adapters)?;
    let expected_lockfile = resolution.to_lockfile(selected_adapters)?;
    if existing_lockfile != expected_lockfile {
        bail!("{LOCKFILE_NAME} is out of date");
    }
    let owned_paths = load_owned_paths(cwd, Some(&existing_lockfile))?;

    validate_collisions(planned_files, &owned_paths)?;
    validate_state_consistency(&owned_paths, &desired_paths, planned_files)?;

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

    for warning in resolution
        .warnings
        .iter()
        .chain(output_plan.warnings.iter())
    {
        reporter.warning(warning)?;
    }

    Ok(DoctorSummary {
        package_count: resolution.packages.len(),
    })
}

fn resolve_project(
    root: &Path,
    cache_root: &Path,
    mode: ResolveMode,
    reporter: &Reporter,
) -> Result<Resolution> {
    let project_root = root
        .canonicalize()
        .with_context(|| format!("failed to access {}", root.display()))?;
    let context = ResolveContext {
        cache_root,
        mode,
        reporter,
    };
    let mut state = ResolverState::default();
    resolve_package(
        &context,
        "root".to_string(),
        project_root.clone(),
        PackageSource::Root,
        PackageRole::Root,
        None,
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
    selected_components: Option<Vec<DependencyComponent>>,
    state: &mut ResolverState,
) -> Result<ResolvedPackage> {
    if let Some(existing) = state.resolved_by_path.get_mut(&package_root) {
        existing.selected_components =
            union_selected_components(existing.selected_components.clone(), selected_components);
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
        selected_components,
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
            let declared_path = dependency
                .path
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("dependency `{alias}` must declare `path`"))?;
            let dependency_root = parent
                .resolve_path(declared_path)
                .with_context(|| format!("failed to resolve dependency `{alias}`"))?;
            let source = PackageSource::Path {
                path: declared_path.clone(),
                tag: dependency.tag.clone(),
            };
            resolve_package(
                context,
                alias.to_string(),
                dependency_root,
                source,
                PackageRole::Dependency,
                dependency.effective_selected_components(),
                state,
            )
        }
        DependencySourceKind::Git => {
            let url = dependency.resolved_git_url()?;
            let tag = dependency.tag.as_deref().unwrap_or_default();
            let checkout = ensure_git_dependency(
                context.cache_root,
                &url,
                Some(tag),
                context.mode == ResolveMode::Sync,
                context.reporter,
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
                dependency.effective_selected_components(),
                state,
            )
        }
    }
}

fn union_selected_components(
    left: Option<Vec<DependencyComponent>>,
    right: Option<Vec<DependencyComponent>>,
) -> Option<Vec<DependencyComponent>> {
    match (left, right) {
        (None, _) | (_, None) => None,
        (Some(mut left), Some(right)) => {
            left.extend(right);
            left.sort();
            left.dedup();
            Some(left)
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
    pub fn to_lockfile(&self, selected_adapters: Adapters) -> Result<Lockfile> {
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
                    path: Some(display_path(path)),
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
                selected_components: package.selected_components.clone(),
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

        Ok(Lockfile::new(
            packages,
            self.lockfile_managed_files(selected_adapters)?,
        ))
    }

    pub fn managed_paths(
        &self,
        project_root: &Path,
        selected_adapters: Adapters,
    ) -> Result<HashSet<PathBuf>> {
        let lockfile = self.to_lockfile(selected_adapters)?;
        lockfile.managed_paths(project_root)
    }

    fn lockfile_managed_files(&self, selected_adapters: Adapters) -> Result<Vec<String>> {
        let package_roots = self
            .packages
            .iter()
            .map(|package| (package.clone(), package.root.clone()))
            .collect::<Vec<_>>();
        Ok(build_output_plan(&self.project_root, &package_roots, selected_adapters)?.managed_files)
    }
}

fn sorted_ids<'a>(ids: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut ids: Vec<_> = ids.cloned().collect();
    ids.sort();
    ids
}

impl ResolvedPackage {
    pub fn selects_component(&self, component: DependencyComponent) -> bool {
        self.selected_components
            .as_ref()
            .is_none_or(|components| components.contains(&component))
    }
}

fn display_path(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        ".".into()
    } else {
        path.to_string_lossy().replace('\\', "/")
    }
}

fn enforce_capabilities(
    resolution: &Resolution,
    allow_high_sensitivity: bool,
    reporter: &Reporter,
) -> Result<()> {
    let mut high_sensitivity = Vec::new();

    for package in &resolution.packages {
        for capability in &package.manifest.manifest.capabilities {
            reporter.note(format!(
                "capability {} {} ({})",
                package.alias, capability.id, capability.sensitivity
            ))?;
            if let Some(justification) = &capability.justification {
                reporter.note(format!("justification: {justification}"))?;
            }
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
        if file.path.exists() && !path_is_owned(&file.path, owned_paths) {
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
    desired_paths: &HashSet<PathBuf>,
    project_root: &Path,
) -> Result<()> {
    for path in owned_paths.difference(desired_paths) {
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if metadata.file_type().is_dir() {
                fs::remove_dir_all(path).with_context(|| {
                    format!(
                        "failed to remove stale managed directory {}",
                        path.display()
                    )
                })?;
            } else {
                fs::remove_file(path).with_context(|| {
                    format!("failed to remove stale managed file {}", path.display())
                })?;
            }
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
    desired_paths: &HashSet<PathBuf>,
    planned_files: &[ManagedFile],
) -> Result<()> {
    if let Some(path) = owned_paths.difference(desired_paths).next() {
        bail!("stale managed state entry for {}", path.display());
    }

    for path in desired_paths.intersection(owned_paths) {
        if !path.exists() {
            bail!("managed file is missing from disk: {}", path.display());
        }
    }

    for file in planned_files {
        if path_is_owned(&file.path, owned_paths) && !file.path.exists() {
            bail!("managed file is missing from disk: {}", file.path.display());
        }
    }

    Ok(())
}

fn path_is_owned(path: &Path, owned_paths: &HashSet<PathBuf>) -> bool {
    owned_paths
        .iter()
        .any(|owned| path == owned || path.starts_with(owned))
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
    use crate::adapters::{Adapter, Adapters, namespaced_file_name, namespaced_skill_id};
    use crate::git::{
        AddSummary, RemoveSummary,
        add_dependency_in_dir_with_adapters as add_dependency_in_dir_with_adapters_impl,
        normalize_alias_from_url, remove_dependency_in_dir as remove_dependency_in_dir_impl,
        shared_checkout_path, shared_repository_path,
    };
    use crate::manifest::{DependencyComponent, MANIFEST_FILE, load_root_from_dir};
    use crate::report::Reporter;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::File::create(path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }

    fn write_manifest(path: &Path, contents: &str) {
        write_file(&path.join(MANIFEST_FILE), contents);
    }

    fn write_skill(path: &Path, name: &str) {
        write_file(
            &path.join("SKILL.md"),
            &format!("---\nname: {name}\ndescription: Example skill.\n---\n# {name}\n"),
        );
    }

    fn write_marketplace(path: &Path, contents: &str) {
        write_file(&path.join(".claude-plugin/marketplace.json"), contents);
    }

    fn write_claude_plugin_json(path: &Path, version: &str) {
        write_file(
            &path.join("claude-code.json"),
            &format!("{{\n  \"name\": \"plugin\",\n  \"version\": \"{version}\"\n}}\n"),
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

    fn tag_repo(path: &Path, tag: &str) {
        let output = Command::new("git")
            .args(["tag", tag])
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn rename_current_branch(path: &Path, branch: &str) {
        let output = Command::new("git")
            .args(["branch", "-m", branch])
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn cache_dir() -> TempDir {
        TempDir::new().unwrap()
    }

    fn resolve_project(root: &Path, cache_root: &Path, mode: ResolveMode) -> Result<Resolution> {
        let reporter = Reporter::silent();
        super::resolve_project(root, cache_root, mode, &reporter)
    }

    fn sync_in_dir(
        cwd: &Path,
        cache_root: &Path,
        locked: bool,
        allow_high_sensitivity: bool,
    ) -> Result<SyncSummary> {
        let reporter = Reporter::silent();
        super::sync_in_dir(cwd, cache_root, locked, allow_high_sensitivity, &reporter)
    }

    fn sync_in_dir_with_adapters(
        cwd: &Path,
        cache_root: &Path,
        locked: bool,
        allow_high_sensitivity: bool,
        adapters: &[Adapter],
    ) -> Result<SyncSummary> {
        let reporter = Reporter::silent();
        super::sync_in_dir_with_adapters(
            cwd,
            cache_root,
            locked,
            allow_high_sensitivity,
            adapters,
            &reporter,
        )
    }

    fn doctor_in_dir(cwd: &Path, cache_root: &Path) -> Result<DoctorSummary> {
        let reporter = Reporter::silent();
        super::doctor_in_dir(cwd, cache_root, &reporter)
    }

    fn add_dependency_in_dir_with_adapters(
        project_root: &Path,
        cache_root: &Path,
        url: &str,
        tag: Option<&str>,
        adapters: &[Adapter],
        components: &[DependencyComponent],
    ) -> Result<AddSummary> {
        let reporter = Reporter::silent();
        add_dependency_in_dir_with_adapters_impl(
            project_root,
            cache_root,
            url,
            tag,
            adapters,
            components,
            &reporter,
        )
    }

    fn remove_dependency_in_dir(
        project_root: &Path,
        cache_root: &Path,
        package: &str,
    ) -> Result<RemoveSummary> {
        let reporter = Reporter::silent();
        remove_dependency_in_dir_impl(project_root, cache_root, package, &reporter)
    }

    fn sync_all(project_root: &Path, cache_root: &Path) {
        sync_in_dir_with_adapters(project_root, cache_root, false, false, &Adapter::ALL).unwrap();
    }

    fn add_dependency_all(project_root: &Path, cache_root: &Path, url: &str, tag: Option<&str>) {
        add_dependency_in_dir_with_adapters(project_root, cache_root, url, tag, &Adapter::ALL, &[])
            .unwrap();
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

    fn canonicalize_git_path_output(path: String) -> PathBuf {
        PathBuf::from(path).canonicalize().unwrap()
    }

    fn toml_path_value(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
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
        let lockfile = resolution
            .to_lockfile(Adapters::from_slice(&Adapter::ALL))
            .unwrap();

        assert_eq!(lockfile.packages.len(), 2);
        assert_eq!(lockfile.packages[0].alias, "root");
        assert_eq!(lockfile.packages[1].alias, "shared");
        assert!(
            !lockfile
                .managed_files
                .contains(&".claude/skills/review".into())
        );
        assert!(
            lockfile
                .managed_files
                .contains(&".codex/skills/checks".into())
        );
    }

    #[test]
    fn add_dependency_clones_repo_and_updates_manifest() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        let (_repo, url) = create_git_dependency();

        add_dependency_all(temp.path(), cache.path(), &url, Some("v0.1.0"));

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
            canonicalize_git_path_output(git_output(
                &checkout_path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"]
            )),
            mirror_path.canonicalize().unwrap()
        );
        let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
        assert!(manifest.contains("[dependencies]"));
        assert!(manifest.contains("tag = \"v0.1.0\""));
        assert!(manifest.contains("url = "));
        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
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
    fn add_dependency_writes_selected_components_to_manifest() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        let (_repo, url) = create_git_dependency();

        add_dependency_in_dir_with_adapters(
            temp.path(),
            cache.path(),
            &url,
            Some("v0.1.0"),
            &[Adapter::Codex],
            &[DependencyComponent::Agents, DependencyComponent::Skills],
        )
        .unwrap();

        let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
        assert!(manifest.contains("components = [\"skills\", \"agents\"]"));
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

        add_dependency_in_dir_with_adapters(
            temp.path(),
            cache.path(),
            &repo.path().to_string_lossy(),
            None,
            &Adapter::ALL,
            &[],
        )
        .unwrap();

        let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
        assert!(manifest.contains("tag = \"v1.2.0\""));
    }

    #[test]
    fn add_dependency_uses_default_branch_when_repo_has_no_tags() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        init_git_repo(repo.path());
        rename_current_branch(repo.path(), "main");

        add_dependency_in_dir_with_adapters(
            temp.path(),
            cache.path(),
            &repo.path().to_string_lossy(),
            None,
            &Adapter::ALL,
            &[],
        )
        .unwrap();

        let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
        assert!(manifest.contains("tag = \"main\""));
    }

    #[test]
    fn add_dependency_rejects_repo_without_supported_directories() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        let repo = TempDir::new().unwrap();
        write_file(&repo.path().join("README.md"), "hello\n");
        init_git_repo(repo.path());
        tag_repo(repo.path(), "v0.1.0");

        let error = add_dependency_in_dir_with_adapters(
            temp.path(),
            cache.path(),
            &repo.path().to_string_lossy(),
            Some("v0.1.0"),
            &Adapter::ALL,
            &[],
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("does not match the Nodus package layout"));
    }

    #[test]
    fn add_dependency_accepts_manifest_only_wrapper_repo_and_syncs_transitive_git_plugins() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();

        let leaf = TempDir::new().unwrap();
        write_skill(&leaf.path().join("skills/checks"), "Checks");
        init_git_repo(leaf.path());
        tag_repo(leaf.path(), "v0.1.0");

        let wrapper = TempDir::new().unwrap();
        write_file(
            &wrapper.path().join(MANIFEST_FILE),
            &format!(
                r#"
[dependencies]
leaf = {{ url = "{}", tag = "v0.1.0" }}
"#,
                toml_path_value(leaf.path())
            ),
        );
        init_git_repo(wrapper.path());
        tag_repo(wrapper.path(), "v0.2.0");
        let wrapper_alias = normalize_alias_from_url(&wrapper.path().to_string_lossy()).unwrap();

        add_dependency_in_dir_with_adapters(
            temp.path(),
            cache.path(),
            &wrapper.path().to_string_lossy(),
            Some("v0.2.0"),
            &Adapter::ALL,
            &[],
        )
        .unwrap();

        let manifest = load_root_from_dir(temp.path()).unwrap();
        assert_eq!(manifest.manifest.dependencies.len(), 1);
        assert!(manifest.manifest.dependencies.contains_key(&wrapper_alias));

        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
        assert_eq!(lockfile.packages.len(), 3);
        assert!(
            lockfile
                .packages
                .iter()
                .any(|package| package.alias == "root")
        );
        let wrapper_package = lockfile
            .packages
            .iter()
            .find(|package| package.alias == wrapper_alias)
            .unwrap();
        assert!(wrapper_package.skills.is_empty());
        assert_eq!(wrapper_package.dependencies, vec!["leaf"]);
        let leaf_package = lockfile
            .packages
            .iter()
            .find(|package| package.alias == "leaf")
            .unwrap();
        assert_eq!(leaf_package.skills, vec!["checks"]);

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let leaf_package = resolution
            .packages
            .iter()
            .find(|package| package.alias == "leaf")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(leaf_package, "checks");
        assert!(
            temp.path()
                .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
    }

    #[test]
    fn add_dependency_accepts_claude_marketplace_wrapper_and_syncs_plugin_contents() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();

        let wrapper = TempDir::new().unwrap();
        write_marketplace(
            wrapper.path(),
            r#"{
  "plugins": [
    {
      "name": "Axiom",
      "source": "./.claude-plugin/plugins/axiom"
    }
  ]
}"#,
        );
        write_skill(
            &wrapper
                .path()
                .join(".claude-plugin/plugins/axiom/skills/review"),
            "Review",
        );
        write_file(
            &wrapper
                .path()
                .join(".claude-plugin/plugins/axiom/agents/security.md"),
            "# Security\n",
        );
        write_file(
            &wrapper
                .path()
                .join(".claude-plugin/plugins/axiom/commands/build.md"),
            "# Build\n",
        );
        write_claude_plugin_json(
            &wrapper.path().join(".claude-plugin/plugins/axiom"),
            "2.34.0",
        );
        init_git_repo(wrapper.path());
        tag_repo(wrapper.path(), "v0.4.0");
        let wrapper_alias = normalize_alias_from_url(&wrapper.path().to_string_lossy()).unwrap();

        add_dependency_in_dir_with_adapters(
            temp.path(),
            cache.path(),
            &wrapper.path().to_string_lossy(),
            Some("v0.4.0"),
            &Adapter::ALL,
            &[],
        )
        .unwrap();

        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
        let wrapper_package = lockfile
            .packages
            .iter()
            .find(|package| package.alias == wrapper_alias)
            .unwrap();
        assert!(wrapper_package.skills.is_empty());
        assert_eq!(wrapper_package.dependencies, vec!["axiom"]);

        let plugin_package = lockfile
            .packages
            .iter()
            .find(|package| package.alias == "axiom")
            .unwrap();
        assert_eq!(plugin_package.version_tag.as_deref(), Some("2.34.0"));
        assert_eq!(
            plugin_package.source.path.as_deref(),
            Some("./.claude-plugin/plugins/axiom")
        );
        assert_eq!(plugin_package.skills, vec!["review"]);
        assert_eq!(plugin_package.agents, vec!["security"]);
        assert_eq!(plugin_package.commands, vec!["build"]);

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let plugin_package = resolution
            .packages
            .iter()
            .find(|package| package.alias == "axiom")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(plugin_package, "review");
        let managed_agent_file = namespaced_file_name(plugin_package, "security", "md");
        let managed_command_file = namespaced_file_name(plugin_package, "build", "md");
        assert!(
            temp.path()
                .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".claude/agents/{managed_agent_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".claude/commands/{managed_command_file}"))
                .exists()
        );
    }

    #[test]
    fn add_dependency_syncs_path_dependencies_inside_manifest_only_wrapper_repo() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();

        let wrapper = TempDir::new().unwrap();
        write_file(
            &wrapper.path().join(MANIFEST_FILE),
            r#"
[dependencies]
bundled = { path = "vendor/bundled" }
"#,
        );
        write_skill(
            &wrapper.path().join("vendor/bundled/skills/bundled"),
            "Bundled",
        );
        init_git_repo(wrapper.path());
        tag_repo(wrapper.path(), "v0.3.0");
        let wrapper_alias = normalize_alias_from_url(&wrapper.path().to_string_lossy()).unwrap();

        add_dependency_in_dir_with_adapters(
            temp.path(),
            cache.path(),
            &wrapper.path().to_string_lossy(),
            Some("v0.3.0"),
            &Adapter::ALL,
            &[],
        )
        .unwrap();

        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
        let wrapper_package = lockfile
            .packages
            .iter()
            .find(|package| package.alias == wrapper_alias)
            .unwrap();
        assert_eq!(wrapper_package.dependencies, vec!["bundled"]);
        let bundled_package = lockfile
            .packages
            .iter()
            .find(|package| package.alias == "bundled")
            .unwrap();
        assert_eq!(bundled_package.source.kind, "path");
        assert_eq!(
            bundled_package.source.path.as_deref(),
            Some("vendor/bundled")
        );
        assert_eq!(bundled_package.skills, vec!["bundled"]);

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let bundled_package = resolution
            .packages
            .iter()
            .find(|package| package.alias == "bundled")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(bundled_package, "bundled");
        assert!(
            temp.path()
                .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
    }

    #[test]
    fn remove_dependency_updates_manifest_and_prunes_managed_files() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        let (_repo, url) = create_git_dependency();
        let alias = normalize_alias_from_url(&url).unwrap();

        add_dependency_all(temp.path(), cache.path(), &url, Some("v0.1.0"));

        let manifest_before = load_root_from_dir(temp.path()).unwrap();
        let dependency = resolve_project(temp.path(), cache.path(), ResolveMode::Sync)
            .unwrap()
            .packages
            .into_iter()
            .find(|package| package.alias != "root")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(&dependency, "review");

        assert!(manifest_before.manifest.dependencies.contains_key(&alias));
        assert!(
            temp.path()
                .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );

        remove_dependency_in_dir(temp.path(), cache.path(), &alias).unwrap();

        let manifest_after = load_root_from_dir(temp.path()).unwrap();
        assert!(manifest_after.manifest.dependencies.is_empty());
        assert!(
            !temp
                .path()
                .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );

        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
        assert_eq!(lockfile.packages.len(), 1);
        assert_eq!(lockfile.packages[0].alias, "root");
    }

    #[test]
    fn remove_dependency_accepts_repository_reference() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        let (_repo, url) = create_git_dependency();

        add_dependency_all(temp.path(), cache.path(), &url, Some("v0.1.0"));

        remove_dependency_in_dir(temp.path(), cache.path(), &url).unwrap();

        let manifest = load_root_from_dir(temp.path()).unwrap();
        assert!(manifest.manifest.dependencies.is_empty());
    }

    #[test]
    fn remove_dependency_rejects_unknown_package() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();

        let error = remove_dependency_in_dir(temp.path(), cache.path(), "missing")
            .unwrap_err()
            .to_string();

        assert!(error.contains("dependency `missing` does not exist"));
    }

    #[test]
    fn sync_emits_dependency_outputs_without_mirroring_root_content() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");
        write_file(&temp.path().join("agents/security.md"), "# Security\n");
        write_file(&temp.path().join("rules/default.rules"), "allow = []\n");
        write_file(&temp.path().join("commands/build.txt"), "cargo test\n");
        write_manifest(
            temp.path(),
            r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/checks"), "Checks");
        write_file(
            &temp.path().join("vendor/shared/agents/shared.md"),
            "# Shared\n",
        );
        write_file(
            &temp.path().join("vendor/shared/rules/default.rules"),
            "allow = []\n",
        );
        write_file(
            &temp.path().join("vendor/shared/commands/build.txt"),
            "cargo test\n",
        );
        write_file(&temp.path().join("AGENTS.md"), "user-owned instructions\n");

        sync_all(temp.path(), cache.path());

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let dependency = resolution
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(dependency, "checks");
        let managed_agent_file = namespaced_file_name(dependency, "shared", "md");
        let managed_command_file = namespaced_file_name(dependency, "build", "md");
        let managed_claude_rule_file = namespaced_file_name(dependency, "default", "md");
        let managed_codex_rule_file = namespaced_file_name(dependency, "default", "rules");

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
        assert!(
            temp.path()
                .join(format!(".claude/agents/{managed_agent_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".claude/commands/{managed_command_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".claude/rules/{managed_claude_rule_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".codex/rules/{managed_codex_rule_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".opencode/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".opencode/agents/{managed_agent_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".opencode/commands/{managed_command_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".opencode/rules/{managed_claude_rule_file}"))
                .exists()
        );
        assert!(!temp.path().join(".claude/agents/security.md").exists());
        assert!(!temp.path().join(".opencode/agents/security.md").exists());
        assert!(
            fs::read_to_string(
                temp.path()
                    .join(format!(".opencode/skills/{managed_skill_id}/SKILL.md"))
            )
            .unwrap()
            .contains(&format!("name: {managed_skill_id}"))
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("AGENTS.md")).unwrap(),
            "user-owned instructions\n"
        );
    }

    #[test]
    fn sync_filters_dependency_outputs_by_selected_components() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies]
shared = { path = "vendor/shared", components = ["skills"] }
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
        write_file(
            &temp.path().join("vendor/shared/agents/shared.md"),
            "# Shared\n",
        );
        write_file(
            &temp.path().join("vendor/shared/rules/default.rules"),
            "allow = []\n",
        );
        write_file(
            &temp.path().join("vendor/shared/commands/build.txt"),
            "cargo test\n",
        );

        sync_all(temp.path(), cache.path());

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let dependency = resolution
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(dependency, "review");
        let managed_agent_file = namespaced_file_name(dependency, "shared", "md");
        let managed_command_file = namespaced_file_name(dependency, "build", "md");
        let managed_claude_rule_file = namespaced_file_name(dependency, "default", "md");
        let managed_codex_rule_file = namespaced_file_name(dependency, "default", "rules");

        assert_eq!(
            dependency.selected_components,
            Some(vec![DependencyComponent::Skills])
        );
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
        assert!(
            temp.path()
                .join(format!(".opencode/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            !temp
                .path()
                .join(format!(".claude/agents/{managed_agent_file}"))
                .exists()
        );
        assert!(
            !temp
                .path()
                .join(format!(".opencode/agents/{managed_agent_file}"))
                .exists()
        );
        assert!(
            !temp
                .path()
                .join(format!(".claude/commands/{managed_command_file}"))
                .exists()
        );
        assert!(
            !temp
                .path()
                .join(format!(".opencode/commands/{managed_command_file}"))
                .exists()
        );
        assert!(
            !temp
                .path()
                .join(format!(".claude/rules/{managed_claude_rule_file}"))
                .exists()
        );
        assert!(
            !temp
                .path()
                .join(format!(".codex/rules/{managed_codex_rule_file}"))
                .exists()
        );
        assert!(
            !temp
                .path()
                .join(format!(".opencode/rules/{managed_claude_rule_file}"))
                .exists()
        );

        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
        let shared = lockfile
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        assert_eq!(
            shared.selected_components,
            Some(vec![DependencyComponent::Skills])
        );
        assert!(
            lockfile
                .managed_files
                .contains(&".claude/skills/review".into())
        );
        assert!(
            !lockfile
                .managed_files
                .contains(&".claude/agents/shared.md".into())
        );
    }

    #[test]
    fn sync_detects_existing_codex_root_and_persists_only_codex() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");
        fs::create_dir_all(temp.path().join(".codex")).unwrap();

        sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

        let manifest = load_root_from_dir(temp.path()).unwrap();
        assert_eq!(
            manifest.manifest.enabled_adapters().unwrap(),
            [Adapter::Codex].as_slice()
        );
        assert!(!temp.path().join(".codex/skills").exists());
        assert!(!temp.path().join(".claude/skills").exists());
        assert!(!temp.path().join(".opencode/skills").exists());
    }

    #[test]
    fn sync_writes_runtime_gitignores_for_managed_outputs() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
        write_file(
            &temp.path().join("vendor/shared/rules/default.rules"),
            "allow = []\n",
        );

        sync_all(temp.path(), cache.path());

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let dependency = resolution
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(dependency, "review");
        let codex_gitignore = fs::read_to_string(temp.path().join(".codex/.gitignore")).unwrap();

        assert!(codex_gitignore.contains("# Managed by nodus"));
        assert!(codex_gitignore.contains(".gitignore"));
        let (_, suffix) = managed_skill_id.rsplit_once('_').unwrap();
        assert!(codex_gitignore.contains(&format!("skills/*_{suffix}/")));
        assert!(codex_gitignore.contains(&format!("rules/*_{suffix}.rules")));
    }

    #[test]
    fn sync_detects_multiple_adapter_roots_and_persists_them() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");
        fs::create_dir_all(temp.path().join(".claude")).unwrap();
        fs::create_dir_all(temp.path().join(".opencode")).unwrap();

        sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

        let manifest = load_root_from_dir(temp.path()).unwrap();
        assert_eq!(
            manifest.manifest.enabled_adapters().unwrap(),
            [Adapter::Claude, Adapter::OpenCode].as_slice()
        );
        assert!(!temp.path().join(".claude/skills").exists());
        assert!(!temp.path().join(".codex/skills").exists());
        assert!(!temp.path().join(".opencode/skills").exists());
    }

    #[test]
    fn sync_persists_explicit_adapter_selection_when_repo_has_no_roots() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");

        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex])
            .unwrap();

        let manifest = load_root_from_dir(temp.path()).unwrap();
        assert_eq!(
            manifest.manifest.enabled_adapters().unwrap(),
            [Adapter::Codex].as_slice()
        );
        assert!(!temp.path().join(".codex/skills").exists());
        assert!(!temp.path().join(".claude/skills").exists());
        assert!(!temp.path().join(".opencode/skills").exists());
    }

    #[test]
    fn sync_requires_explicit_adapter_when_repo_has_no_signals() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");

        let error = sync_in_dir(temp.path(), cache.path(), false, false)
            .unwrap_err()
            .to_string();

        assert!(error.contains("Pass `--adapter"));
    }

    #[test]
    fn sync_prefers_manifest_selection_over_detected_roots() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[adapters]
enabled = ["codex"]
"#,
        );
        fs::create_dir_all(temp.path().join(".claude")).unwrap();

        sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

        assert!(!temp.path().join(".codex/skills").exists());
        assert!(!temp.path().join(".claude/skills").exists());
    }

    #[test]
    fn sync_prunes_outputs_when_adapter_selection_is_narrowed() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

        sync_all(temp.path(), cache.path());
        assert!(temp.path().join(".claude/skills").exists());
        assert!(temp.path().join(".codex/skills").exists());
        assert!(temp.path().join(".opencode/skills").exists());

        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex])
            .unwrap();

        let manifest = load_root_from_dir(temp.path()).unwrap();
        assert_eq!(
            manifest.manifest.enabled_adapters().unwrap(),
            [Adapter::Codex].as_slice()
        );
        assert!(!temp.path().join(".claude/skills").exists());
        assert!(!temp.path().join(".claude/.gitignore").exists());
        assert!(temp.path().join(".codex/skills").exists());
        assert!(temp.path().join(".codex/.gitignore").exists());
        assert!(!temp.path().join(".opencode/skills").exists());
        assert!(!temp.path().join(".opencode/.gitignore").exists());
    }

    #[test]
    fn sync_prunes_outputs_when_dependency_components_are_narrowed() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
        write_file(
            &temp.path().join("vendor/shared/agents/shared.md"),
            "# Shared\n",
        );

        sync_all(temp.path(), cache.path());

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let dependency = resolution
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(dependency, "review");
        let managed_agent_file = namespaced_file_name(dependency, "shared", "md");
        assert!(
            temp.path()
                .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".claude/agents/{managed_agent_file}"))
                .exists()
        );

        write_manifest(
            temp.path(),
            r#"
[adapters]
enabled = ["claude", "codex", "opencode"]

[dependencies]
shared = { path = "vendor/shared", components = ["skills"] }
"#,
        );

        sync_all(temp.path(), cache.path());

        assert!(
            temp.path()
                .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            !temp
                .path()
                .join(format!(".claude/agents/{managed_agent_file}"))
                .exists()
        );
        assert!(
            !temp
                .path()
                .join(format!(".opencode/agents/{managed_agent_file}"))
                .exists()
        );
    }

    #[test]
    fn sync_records_stable_skill_roots_in_lockfile() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
        );
        write_skill(
            &temp.path().join("vendor/shared/skills/iframe-ad"),
            "Iframe Ad",
        );

        sync_all(temp.path(), cache.path());

        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();

        assert!(
            lockfile
                .managed_files
                .contains(&".claude/skills/iframe-ad".into())
        );
        assert!(
            lockfile
                .managed_files
                .contains(&".codex/skills/iframe-ad".into())
        );
        assert!(
            lockfile
                .managed_files
                .contains(&".opencode/skills/iframe-ad".into())
        );
        assert!(
            !lockfile
                .managed_files
                .iter()
                .any(|path| path.contains("iframe-ad_"))
        );
    }

    #[test]
    fn sync_records_selected_components_without_supported_outputs() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies]
shared = { path = "vendor/shared", components = ["agents"] }
"#,
        );
        write_file(
            &temp.path().join("vendor/shared/agents/shared.md"),
            "# Shared\n",
        );

        let summary =
            sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex])
                .unwrap();
        assert_eq!(summary.managed_file_count, 0);

        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
        let shared = lockfile
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        assert_eq!(
            shared.selected_components,
            Some(vec![DependencyComponent::Agents])
        );
        assert!(lockfile.managed_files.is_empty());
        assert!(!temp.path().join(".codex/agents").exists());
    }

    #[test]
    fn doctor_detects_lockfile_drift_when_only_components_change() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
        write_file(
            &temp.path().join("vendor/shared/agents/shared.md"),
            "# Shared\n",
        );

        sync_all(temp.path(), cache.path());

        write_manifest(
            temp.path(),
            r#"
[adapters]
enabled = ["claude", "codex", "opencode"]

[dependencies]
shared = { path = "vendor/shared", components = ["skills"] }
"#,
        );

        let error = doctor_in_dir(temp.path(), cache.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("nodus.lock is out of date"));
    }

    #[test]
    fn sync_unions_component_selection_for_duplicate_package_references() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies]
shared_agents = { path = "vendor/shared", components = ["agents"] }
shared_skills = { path = "vendor/shared", components = ["skills"] }
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
        write_file(
            &temp.path().join("vendor/shared/agents/shared.md"),
            "# Shared\n",
        );

        sync_all(temp.path(), cache.path());

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        assert_eq!(resolution.packages.len(), 2);
        let dependency = resolution
            .packages
            .iter()
            .find(|package| package.alias != "root")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(dependency, "review");
        let managed_agent_file = namespaced_file_name(dependency, "shared", "md");
        assert_eq!(
            dependency.selected_components,
            Some(vec![
                DependencyComponent::Skills,
                DependencyComponent::Agents,
            ])
        );
        assert!(
            temp.path()
                .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".claude/agents/{managed_agent_file}"))
                .exists()
        );

        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
        let shared = lockfile
            .packages
            .iter()
            .find(|package| package.alias != "root")
            .unwrap();
        assert_eq!(
            shared.selected_components,
            Some(vec![
                DependencyComponent::Skills,
                DependencyComponent::Agents,
            ])
        );
    }

    #[test]
    fn sync_keeps_transitive_dependencies_when_parent_components_are_narrowed() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies]
wrapper = { path = "vendor/wrapper", components = ["skills"] }
"#,
        );
        write_file(
            &temp.path().join("vendor/wrapper/nodus.toml"),
            r#"
[dependencies]
leaf = { path = "vendor/leaf" }
"#,
        );
        write_file(
            &temp.path().join("vendor/wrapper/agents/wrapper.md"),
            "# Wrapper\n",
        );
        write_skill(
            &temp.path().join("vendor/wrapper/vendor/leaf/skills/checks"),
            "Checks",
        );

        sync_all(temp.path(), cache.path());

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let wrapper = resolution
            .packages
            .iter()
            .find(|package| package.alias == "wrapper")
            .unwrap();
        let leaf = resolution
            .packages
            .iter()
            .find(|package| package.alias == "leaf")
            .unwrap();
        let managed_wrapper_agent_file = namespaced_file_name(wrapper, "wrapper", "md");
        let managed_leaf_skill_id = namespaced_skill_id(leaf, "checks");

        assert_eq!(
            wrapper.selected_components,
            Some(vec![DependencyComponent::Skills])
        );
        assert!(
            !temp
                .path()
                .join(format!(".claude/agents/{managed_wrapper_agent_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".claude/skills/{managed_leaf_skill_id}/SKILL.md"))
                .exists()
        );
    }

    #[test]
    fn sync_requires_opt_in_for_high_sensitivity_capabilities() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[[capabilities]]
id = "shell.exec"
sensitivity = "high"

[dependencies]
shared = { path = "vendor/shared" }
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

        let error =
            sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &Adapter::ALL)
                .unwrap_err()
                .to_string();
        assert!(error.contains("--allow-high-sensitivity"));

        sync_in_dir_with_adapters(temp.path(), cache.path(), false, true, &Adapter::ALL).unwrap();
        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let dependency = resolution
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(dependency, "review");
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

        add_dependency_all(temp.path(), cache.path(), &url, Some("v0.1.0"));
        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let dependency = resolution
            .packages
            .iter()
            .find(|package| matches!(package.source, PackageSource::Git { .. }))
            .unwrap();
        let managed_skill_id = namespaced_skill_id(dependency, "review");

        sync_all(temp.path(), cache.path());

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

        write_manifest(
            temp.path(),
            r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
        write_file(
            &temp.path().join("vendor/shared/agents/security.md"),
            "# Security\n",
        );
        write_file(
            &temp.path().join("vendor/shared/rules/default.rules"),
            "allow = []\n",
        );
        write_file(
            &temp.path().join("vendor/shared/commands/build.txt"),
            "cargo test\n",
        );

        sync_all(temp.path(), cache.path());
        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let dependency = resolution
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        let managed_agent_file = namespaced_file_name(dependency, "security", "md");
        let managed_command_file = namespaced_file_name(dependency, "build", "md");
        let managed_rule_file = namespaced_file_name(dependency, "default", "md");
        assert!(
            temp.path()
                .join(format!(".claude/agents/{managed_agent_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".claude/commands/{managed_command_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".claude/rules/{managed_rule_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".opencode/agents/{managed_agent_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".opencode/rules/{managed_rule_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".opencode/commands/{managed_command_file}"))
                .exists()
        );

        fs::remove_file(temp.path().join("vendor/shared/agents/security.md")).unwrap();
        fs::remove_dir(temp.path().join("vendor/shared/agents")).unwrap();
        fs::remove_file(temp.path().join("vendor/shared/rules/default.rules")).unwrap();
        fs::remove_dir(temp.path().join("vendor/shared/rules")).unwrap();
        fs::remove_file(temp.path().join("vendor/shared/commands/build.txt")).unwrap();
        fs::remove_dir(temp.path().join("vendor/shared/commands")).unwrap();
        sync_all(temp.path(), cache.path());

        assert!(
            !temp
                .path()
                .join(format!(".claude/agents/{managed_agent_file}"))
                .exists()
        );
        assert!(
            !temp
                .path()
                .join(format!(".claude/commands/{managed_command_file}"))
                .exists()
        );
        assert!(
            !temp
                .path()
                .join(format!(".claude/rules/{managed_rule_file}"))
                .exists()
        );
        assert!(
            !temp
                .path()
                .join(format!(".opencode/agents/{managed_agent_file}"))
                .exists()
        );
        assert!(
            !temp
                .path()
                .join(format!(".opencode/rules/{managed_rule_file}"))
                .exists()
        );
        assert!(
            !temp
                .path()
                .join(format!(".opencode/commands/{managed_command_file}"))
                .exists()
        );
    }

    #[test]
    fn sync_preserves_user_owned_root_instruction_files() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
        );
        write_file(
            &temp.path().join("vendor/shared/rules/default.rules"),
            "allow = []\n",
        );
        write_file(&temp.path().join("CLAUDE.md"), "user-owned memory\n");
        write_file(&temp.path().join("AGENTS.md"), "user-owned agents\n");

        sync_all(temp.path(), cache.path());

        assert_eq!(
            fs::read_to_string(temp.path().join("CLAUDE.md")).unwrap(),
            "user-owned memory\n"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("AGENTS.md")).unwrap(),
            "user-owned agents\n"
        );
        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let dependency = resolution
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        let managed_rule_file = namespaced_file_name(dependency, "default", "md");
        assert!(
            temp.path()
                .join(format!(".claude/rules/{managed_rule_file}"))
                .exists()
        );
    }

    #[test]
    fn sync_namespaces_duplicate_opencode_skill_ids_across_packages() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies]
shared = { path = "vendor/shared" }
other = { path = "vendor/other" }
"#,
        );
        write_file(
            &temp.path().join("vendor/shared/skills/review/SKILL.md"),
            "---\nname: Shared Review\ndescription: Different review skill.\n---\n# Shared Review\n",
        );
        write_file(
            &temp.path().join("vendor/other/skills/review/SKILL.md"),
            "---\nname: Other Review\ndescription: Another review skill.\n---\n# Other Review\n",
        );

        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &Adapter::ALL).unwrap();

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let shared = resolution
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        let other = resolution
            .packages
            .iter()
            .find(|package| package.alias == "other")
            .unwrap();
        let shared_skill_id = namespaced_skill_id(shared, "review");
        let other_skill_id = namespaced_skill_id(other, "review");

        assert_ne!(shared_skill_id, other_skill_id);
        assert!(
            temp.path()
                .join(format!(".opencode/skills/{shared_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".opencode/skills/{other_skill_id}/SKILL.md"))
                .exists()
        );
    }

    #[test]
    fn sync_namespaces_duplicate_file_ids_across_packages() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies]
shared = { path = "vendor/shared" }
other = { path = "vendor/other" }
"#,
        );
        write_file(
            &temp.path().join("vendor/shared/agents/security.md"),
            "# Shared Security\n",
        );
        write_file(
            &temp.path().join("vendor/shared/rules/default.rules"),
            "allow = []\n",
        );
        write_file(
            &temp.path().join("vendor/shared/commands/build.txt"),
            "cargo test\n",
        );
        write_file(
            &temp.path().join("vendor/other/agents/security.md"),
            "# Other Security\n",
        );
        write_file(
            &temp.path().join("vendor/other/rules/default.rules"),
            "deny = []\n",
        );
        write_file(
            &temp.path().join("vendor/other/commands/build.txt"),
            "cargo check\n",
        );

        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &Adapter::ALL).unwrap();

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let shared = resolution
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        let other = resolution
            .packages
            .iter()
            .find(|package| package.alias == "other")
            .unwrap();

        let shared_agent_file = namespaced_file_name(shared, "security", "md");
        let other_agent_file = namespaced_file_name(other, "security", "md");
        let shared_command_file = namespaced_file_name(shared, "build", "md");
        let other_command_file = namespaced_file_name(other, "build", "md");
        let shared_claude_rule_file = namespaced_file_name(shared, "default", "md");
        let other_claude_rule_file = namespaced_file_name(other, "default", "md");
        let shared_codex_rule_file = namespaced_file_name(shared, "default", "rules");
        let other_codex_rule_file = namespaced_file_name(other, "default", "rules");

        assert_ne!(shared_agent_file, other_agent_file);
        assert_ne!(shared_command_file, other_command_file);
        assert_ne!(shared_claude_rule_file, other_claude_rule_file);
        assert_ne!(shared_codex_rule_file, other_codex_rule_file);

        assert!(
            temp.path()
                .join(format!(".claude/agents/{shared_agent_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".claude/agents/{other_agent_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".claude/commands/{shared_command_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".claude/commands/{other_command_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".claude/rules/{shared_claude_rule_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".claude/rules/{other_claude_rule_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".codex/rules/{shared_codex_rule_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".codex/rules/{other_codex_rule_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".opencode/agents/{shared_agent_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".opencode/agents/{other_agent_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".opencode/commands/{shared_command_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".opencode/commands/{other_command_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".opencode/rules/{shared_claude_rule_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".opencode/rules/{other_claude_rule_file}"))
                .exists()
        );
    }

    #[test]
    fn sync_prunes_old_skill_directories_when_digest_changes() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

        sync_all(temp.path(), cache.path());

        let first_resolution =
            resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let first_dependency = first_resolution
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        let first_skill_id = namespaced_skill_id(first_dependency, "review");
        let first_skill_dir = temp.path().join(format!(".claude/skills/{first_skill_id}"));
        assert!(first_skill_dir.exists());

        write_file(
            &temp.path().join("vendor/shared/skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Updated review skill.\n---\n# Review\nchanged\n",
        );

        sync_all(temp.path(), cache.path());

        let second_resolution =
            resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let second_dependency = second_resolution
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        let second_skill_id = namespaced_skill_id(second_dependency, "review");
        let second_skill_dir = temp
            .path()
            .join(format!(".claude/skills/{second_skill_id}"));

        assert_ne!(first_skill_id, second_skill_id);
        assert!(second_skill_dir.exists());
        assert!(!first_skill_dir.exists());
    }

    #[test]
    fn doctor_detects_missing_file_inside_managed_skill_directory() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

        sync_all(temp.path(), cache.path());

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let dependency = resolution
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(dependency, "review");
        fs::remove_file(
            temp.path()
                .join(format!(".claude/skills/{managed_skill_id}/SKILL.md")),
        )
        .unwrap();

        let error = doctor_in_dir(temp.path(), cache.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("managed file is missing from disk"));
    }

    #[test]
    fn doctor_detects_lockfile_drift() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");
        sync_all(temp.path(), cache.path());

        write_skill(&temp.path().join("skills/renamed"), "Renamed");

        let error = doctor_in_dir(temp.path(), cache.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("out of date"));
    }

    #[test]
    fn doctor_accepts_legacy_detected_adapter_roots_without_manifest_config() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");
        fs::create_dir_all(temp.path().join(".codex")).unwrap();

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let package_roots = resolution
            .packages
            .iter()
            .map(|package| (package.clone(), package.root.clone()))
            .collect::<Vec<_>>();
        let output_plan = build_output_plan(temp.path(), &package_roots, Adapters::CODEX).unwrap();
        write_managed_files(&output_plan.files).unwrap();
        resolution
            .to_lockfile(Adapters::CODEX)
            .unwrap()
            .write(&temp.path().join(LOCKFILE_NAME))
            .unwrap();

        doctor_in_dir(temp.path(), cache.path()).unwrap();
    }

    #[test]
    fn shared_cache_is_reused_across_multiple_projects() {
        let cache = cache_dir();
        let project_one = TempDir::new().unwrap();
        let project_two = TempDir::new().unwrap();
        let (_repo, url) = create_git_dependency();

        add_dependency_all(project_one.path(), cache.path(), &url, Some("v0.1.0"));
        add_dependency_all(project_two.path(), cache.path(), &url, Some("v0.1.0"));

        let mirror_path = shared_repository_path(cache.path(), &url).unwrap();
        let rev = git_output(&mirror_path, &["rev-parse", "v0.1.0^{commit}"]);
        let checkout_path = shared_checkout_path(cache.path(), &url, &rev).unwrap();
        assert!(mirror_path.exists());
        assert!(checkout_path.exists());
        assert_eq!(
            canonicalize_git_path_output(git_output(
                &checkout_path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"]
            )),
            mirror_path.canonicalize().unwrap()
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

        add_dependency_all(temp.path(), cache.path(), &url, Some("v0.1.0"));

        assert!(shared_repository_path(cache.path(), &url).unwrap().exists());
    }

    #[test]
    fn doctor_accepts_shared_mirror_backed_checkouts() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        let (_repo, url) = create_git_dependency();

        add_dependency_all(temp.path(), cache.path(), &url, Some("v0.1.0"));

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
