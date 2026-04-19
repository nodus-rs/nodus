mod doctor;
mod resolve;
mod support;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub use self::doctor::{
    DoctorActionRecord, DoctorFinding, DoctorFindingKind, DoctorMode, DoctorStatus, DoctorSummary,
    doctor_in_dir_with_mode,
};
use self::resolve::resolve_project;
use self::support::{
    build_sync_execution_plan, enforce_capabilities, execute_sync_plan, find_managed_collision,
    find_unmanaged_collision, load_owned_paths, recover_runtime_owned_paths,
    unmanaged_collision_guidance,
};
#[cfg(test)]
use self::support::{prune_empty_parent_dirs, write_managed_files};
use crate::adapters::{Adapter, Adapters, ManagedFile, build_output_plan};
use crate::execution::ExecutionMode;
use crate::install_paths::{InstallPaths, InstallScope};
use crate::lockfile::{LOCKFILE_NAME, LockedPackage, LockedSource, Lockfile};
use crate::manifest::{
    DependencyComponent, LoadedManifest, ManagedPlacement, PackageRole, load_dependency_from_dir,
    load_root_from_dir_allow_missing,
};
use crate::paths::display_path;
use crate::report::Reporter;
use crate::selection::{
    resolve_adapter_selection, resolve_global_adapter_selection, should_prompt_for_adapter,
};
use crate::store::{SnapshotSource, snapshot_packages};
use anyhow::{Result, bail};
#[cfg(test)]
use std::fs;

#[derive(Debug, Clone)]
pub struct Resolution {
    pub packages: Vec<ResolvedPackage>,
    pub warnings: Vec<String>,
    pub(crate) managed_migrations: Vec<ManagedMappingMigration>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub alias: String,
    pub root: PathBuf,
    pub manifest: LoadedManifest,
    pub source: PackageSource,
    pub digest: String,
    pub selected_components: Option<Vec<DependencyComponent>>,
    pub selected_workspace_members: Option<Vec<String>>,
    pub managed_paths: Vec<ResolvedManagedPath>,
    extra_package_files: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedManagedPath {
    pub source_root: PathBuf,
    pub target_root: PathBuf,
    pub ownership_root: PathBuf,
    pub files: Vec<ResolvedManagedFile>,
    pub origin: ResolvedManagedPathOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResolvedManagedFile {
    pub source_relative: PathBuf,
    pub target_relative: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedManagedPathOrigin {
    LegacyDependencyMapping,
    PackageManagedExport { placement: ManagedPlacement },
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedMappingMigration {
    alias: String,
    legacy_target_roots: Vec<PathBuf>,
    adds_additional_package_exports: bool,
}

#[derive(Debug, Clone)]
pub struct SyncSummary {
    pub package_count: usize,
    pub adapters: Vec<Adapter>,
    pub managed_file_count: usize,
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
        subpath: Option<PathBuf>,
        tag: Option<String>,
        branch: Option<String>,
        rev: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolveMode {
    Sync,
    Doctor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncMode {
    Normal,
    Locked,
    Frozen,
}

impl SyncMode {
    fn checks_lockfile(self) -> bool {
        matches!(self, Self::Locked | Self::Frozen)
    }

    fn installs_from_lockfile(self) -> bool {
        matches!(self, Self::Frozen)
    }

    fn flag(self) -> &'static str {
        match self {
            Self::Normal => "`nodus sync`",
            Self::Locked => "`nodus sync --locked`",
            Self::Frozen => "`nodus sync --frozen`",
        }
    }
}

fn lockfile_out_of_date_message() -> String {
    format!(
        "{LOCKFILE_NAME} is out of date; run `nodus sync` to regenerate the lockfile and managed outputs, then run `nodus doctor` to verify the project state"
    )
}

fn checked_sync_lockfile_out_of_date_message() -> String {
    format!(
        "{LOCKFILE_NAME} is out of date; run `nodus sync` without `--locked` or `--frozen` to regenerate the lockfile and managed outputs"
    )
}

#[derive(Debug, Clone)]
struct PlannedFileWrite {
    path: PathBuf,
    contents: Vec<u8>,
    create: bool,
}

#[derive(Debug, Clone)]
struct SyncExecutionPlan {
    runtime_root: PathBuf,
    owned_paths: HashSet<PathBuf>,
    desired_paths: HashSet<PathBuf>,
    manifest_write: Option<PlannedFileWrite>,
    removals: Vec<PathBuf>,
    managed_writes: Vec<ManagedFile>,
    lockfile_write: Option<PlannedFileWrite>,
    warnings: Vec<String>,
    summary: SyncSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UnmanagedCollision {
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManagedCollision {
    alias: String,
    ownership_root: PathBuf,
    collision_path: PathBuf,
    source: ManagedCollisionSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedCollisionSource {
    LegacyDependencyMapping,
    PackageManagedExport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedCollisionChoice {
    Adopt,
    RemoveMapping,
    Cancel,
}

trait ManagedCollisionResolver {
    fn resolve(
        &mut self,
        project_root: &Path,
        collision: &ManagedCollision,
    ) -> Result<ManagedCollisionChoice>;
}

struct TtyManagedCollisionResolver;

#[allow(clippy::too_many_arguments)]
pub fn sync_in_dir_with_adapters(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    let install_paths = InstallPaths::project(cwd);
    sync_in_dir_with_adapters_mode(
        &install_paths,
        cache_root,
        if locked {
            SyncMode::Locked
        } else {
            SyncMode::Normal
        },
        allow_high_sensitivity,
        force,
        adapters,
        sync_on_launch,
        ExecutionMode::Apply,
        None,
        reporter,
    )
}

pub fn sync_in_dir_with_adapters_frozen(
    cwd: &Path,
    cache_root: &Path,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    let install_paths = InstallPaths::project(cwd);
    sync_in_dir_with_adapters_mode(
        &install_paths,
        cache_root,
        SyncMode::Frozen,
        allow_high_sensitivity,
        force,
        adapters,
        sync_on_launch,
        ExecutionMode::Apply,
        None,
        reporter,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn sync_in_dir_with_adapters_dry_run(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    let install_paths = InstallPaths::project(cwd);
    sync_in_dir_with_adapters_mode(
        &install_paths,
        cache_root,
        if locked {
            SyncMode::Locked
        } else {
            SyncMode::Normal
        },
        allow_high_sensitivity,
        force,
        adapters,
        sync_on_launch,
        ExecutionMode::DryRun,
        None,
        reporter,
    )
}

pub fn sync_in_dir_with_adapters_frozen_dry_run(
    cwd: &Path,
    cache_root: &Path,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    let install_paths = InstallPaths::project(cwd);
    sync_in_dir_with_adapters_mode(
        &install_paths,
        cache_root,
        SyncMode::Frozen,
        allow_high_sensitivity,
        force,
        adapters,
        sync_on_launch,
        ExecutionMode::DryRun,
        None,
        reporter,
    )
}

#[allow(clippy::too_many_arguments)]
fn sync_in_dir_with_adapters_mode(
    install_paths: &InstallPaths,
    cache_root: &Path,
    sync_mode: SyncMode,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    execution_mode: ExecutionMode,
    root_override: Option<LoadedManifest>,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    let mut collision_resolver = TtyManagedCollisionResolver;
    sync_in_dir_with_adapters_mode_and_collision_resolution(
        install_paths,
        cache_root,
        sync_mode,
        allow_high_sensitivity,
        force,
        adapters,
        sync_on_launch,
        execution_mode,
        root_override,
        if sync_mode.checks_lockfile() || !should_prompt_for_adapter() {
            None
        } else {
            Some(&mut collision_resolver)
        },
        reporter,
    )
}

#[allow(clippy::too_many_arguments)]
fn sync_in_dir_with_adapters_mode_and_collision_resolution(
    install_paths: &InstallPaths,
    cache_root: &Path,
    sync_mode: SyncMode,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    execution_mode: ExecutionMode,
    root_override: Option<LoadedManifest>,
    mut collision_resolver: Option<&mut dyn ManagedCollisionResolver>,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    if matches!(install_paths.scope, InstallScope::Project) {
        crate::relay::ensure_no_pending_relay_edits_in_dir(&install_paths.config_root, cache_root)?;
    }
    let has_root_override = root_override.is_some();
    let original_root = load_root_from_dir_allow_missing(&install_paths.config_root)?;
    let mut root = root_override.unwrap_or_else(|| original_root.clone());
    let mut adopted_owned_paths = HashSet::new();
    let selection = match install_paths.scope {
        InstallScope::Project => resolve_adapter_selection(
            &install_paths.adapter_detection_root,
            &root.manifest,
            adapters,
            !sync_mode.checks_lockfile() && should_prompt_for_adapter(),
        )?,
        InstallScope::Global => {
            if sync_on_launch {
                bail!("`nodus add --global` does not support `--sync-on-launch`");
            }
            resolve_global_adapter_selection(
                &install_paths.adapter_detection_root,
                &root.manifest,
                adapters,
            )?
        }
    };
    if selection.should_persist {
        if sync_mode.checks_lockfile() {
            bail!(
                "adapter selection must be persisted before running {}; rerun without `--locked` or `--frozen`, or set `[adapters] enabled = [...]` in nodus.toml",
                sync_mode.flag(),
            );
        }
        root.manifest.set_enabled_adapters(&selection.adapters);
    }
    if sync_on_launch {
        if sync_mode.checks_lockfile() {
            bail!(
                "launch hook configuration must be persisted before running {}; rerun without `--locked` or `--frozen`, or declare the `nodus.sync_on_startup` hook in [[hooks]]",
                sync_mode.flag(),
            );
        }
        root.manifest.set_sync_on_launch(true);
    }
    if has_root_override || selection.should_persist || sync_on_launch {
        root = original_root.with_manifest(root.manifest.clone(), PackageRole::Root)?;
    }

    let lockfile_path = install_paths.config_root.join(LOCKFILE_NAME);
    let existing_lockfile = if lockfile_path.exists() {
        Some(if sync_mode.checks_lockfile() {
            Lockfile::read(&lockfile_path)?
        } else {
            Lockfile::read_for_sync(&lockfile_path)?
        })
    } else {
        None
    };
    if let Some(lockfile) = existing_lockfile.as_ref()
        && !lockfile.uses_current_schema()
    {
        reporter.note(format!(
            "upgrading {LOCKFILE_NAME} from version {} to {}",
            lockfile.version,
            Lockfile::current_version()
        ))?;
    }
    if sync_mode.installs_from_lockfile() && existing_lockfile.is_none() {
        bail!(
            "`--frozen` requires an existing {} in {}",
            LOCKFILE_NAME,
            install_paths.config_root.display()
        );
    }

    loop {
        reporter.status(
            "Resolving",
            format!("package graph in {}", install_paths.config_root.display()),
        )?;
        let resolution = resolve_project(
            &install_paths.config_root,
            cache_root,
            ResolveMode::Sync,
            reporter,
            existing_lockfile
                .as_ref()
                .filter(|_| sync_mode.installs_from_lockfile()),
            Some(&root),
        )?;
        if !resolution.managed_migrations().is_empty() {
            if sync_mode.checks_lockfile() {
                bail!(
                    "legacy dependency `managed` mappings must be migrated before running {}; rerun plain `nodus sync` to let Nodus adopt package-owned `managed_exports`",
                    sync_mode.flag(),
                );
            }
            for migration in resolution.managed_migrations() {
                for target_root in &migration.legacy_target_roots {
                    if !root
                        .manifest
                        .remove_managed_mapping(&migration.alias, target_root)?
                    {
                        bail!(
                            "failed to migrate legacy managed mapping for dependency `{}` targeting {}",
                            migration.alias,
                            target_root.display()
                        );
                    }
                }
                let mut message = format!(
                    "migrating dependency `{}` to package-owned `managed_exports`",
                    migration.alias
                );
                if migration.adds_additional_package_exports {
                    message.push_str(
                        "; package-declared exports include additional managed files beyond the legacy subset",
                    );
                }
                reporter.note(message)?;
            }
            root = root.with_manifest(root.manifest.clone(), PackageRole::Root)?;
            continue;
        }
        reporter.status("Checking", "declared capabilities")?;
        enforce_capabilities(&resolution, allow_high_sensitivity, reporter)?;
        reporter.status(
            "Snapshotting",
            format!("{} packages", resolution.packages.len()),
        )?;
        let stored_packages = snapshot_packages(cache_root, &resolution.packages)?;

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
        let output_plan = build_output_plan(
            &install_paths.runtime_root,
            &package_snapshots,
            selected_adapters,
            existing_lockfile.as_ref(),
            true,
        )?;
        let mut planned_files = output_plan.files.clone();
        let mut desired_paths =
            resolution.managed_paths(&install_paths.runtime_root, selected_adapters)?;
        let workspace_marketplace_files =
            planned_workspace_marketplace_files(&root, &install_paths.runtime_root)?;
        desired_paths.extend(
            workspace_marketplace_files
                .iter()
                .map(|file| file.path.clone()),
        );
        planned_files.extend(workspace_marketplace_files);
        let lockfile = resolution.to_lockfile(selected_adapters, &install_paths.runtime_root)?;
        let mut owned_paths =
            load_owned_paths(&install_paths.runtime_root, existing_lockfile.as_ref())?;
        if existing_lockfile.is_none() {
            owned_paths.extend(recover_runtime_owned_paths(
                &install_paths.runtime_root,
                &desired_paths,
            ));
        }
        owned_paths.extend(adopted_owned_paths.iter().cloned());

        if sync_mode.checks_lockfile() {
            let Some(existing) = existing_lockfile.as_ref() else {
                bail!(
                    "{} requires an existing {} in {}",
                    sync_mode.flag(),
                    LOCKFILE_NAME,
                    install_paths.config_root.display()
                );
            };
            if *existing != lockfile {
                bail!("{}", checked_sync_lockfile_out_of_date_message());
            }
        }

        if let Some(unmanaged_collision) =
            find_unmanaged_collision(&planned_files, &owned_paths, &install_paths.runtime_root)
        {
            if force {
                reporter.note(format!(
                    "forcing overwrite of unmanaged path {}",
                    display_path(&unmanaged_collision.path)
                ))?;
                adopted_owned_paths.insert(unmanaged_collision.path.clone());
                continue;
            }
            let Some(managed_collision) = find_managed_collision(
                &install_paths.runtime_root,
                &resolution,
                &unmanaged_collision,
            ) else {
                bail!(
                    "refusing to overwrite unmanaged file {}",
                    display_path(&unmanaged_collision.path)
                );
            };
            let Some(resolver) = collision_resolver.as_deref_mut() else {
                bail!(
                    "{}",
                    unmanaged_collision_guidance(
                        &install_paths.runtime_root,
                        &managed_collision,
                        sync_mode,
                    )
                );
            };
            match resolver.resolve(&install_paths.runtime_root, &managed_collision)? {
                ManagedCollisionChoice::Adopt => {
                    let ownership_root = install_paths
                        .runtime_root
                        .join(&managed_collision.ownership_root);
                    reporter.note(format!(
                        "adopting managed target {}",
                        display_path(&ownership_root)
                    ))?;
                    adopted_owned_paths.insert(ownership_root);
                    continue;
                }
                ManagedCollisionChoice::RemoveMapping => {
                    if managed_collision.source != ManagedCollisionSource::LegacyDependencyMapping {
                        bail!(
                            "cannot remove package-owned managed export for dependency `{}` from the consumer manifest",
                            managed_collision.alias
                        );
                    }
                    if !root.manifest.remove_managed_mapping(
                        &managed_collision.alias,
                        &managed_collision.ownership_root,
                    )? {
                        bail!(
                            "failed to remove managed mapping for dependency `{}` targeting {}",
                            managed_collision.alias,
                            managed_collision.ownership_root.display()
                        );
                    }
                    reporter.note(format!(
                        "removing managed mapping for dependency `{}` targeting {}",
                        managed_collision.alias,
                        managed_collision.ownership_root.display()
                    ))?;
                    root = root.with_manifest(root.manifest.clone(), PackageRole::Root)?;
                    continue;
                }
                ManagedCollisionChoice::Cancel => {
                    bail!(
                        "cancelled {} because managed target {} collides with existing unmanaged path {}",
                        sync_mode.flag(),
                        display_path(
                            &install_paths
                                .runtime_root
                                .join(&managed_collision.ownership_root),
                        ),
                        display_path(&managed_collision.collision_path)
                    );
                }
            }
        }

        let plan = build_sync_execution_plan(
            &original_root,
            &root,
            &lockfile_path,
            &lockfile,
            &install_paths.runtime_root,
            &owned_paths,
            &desired_paths,
            &planned_files,
            resolution
                .warnings
                .iter()
                .chain(output_plan.warnings.iter())
                .cloned()
                .collect(),
            SyncSummary {
                package_count: resolution.packages.len(),
                adapters: selection.adapters,
                managed_file_count: planned_files.len(),
            },
            sync_mode,
        )?;
        execute_sync_plan(&plan, execution_mode, reporter)?;

        return Ok(plan.summary);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn sync_in_dir_with_loaded_root(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    execution_mode: ExecutionMode,
    root: LoadedManifest,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    let install_paths = InstallPaths::project(cwd);
    sync_with_loaded_root_at_paths(
        &install_paths,
        cache_root,
        locked,
        allow_high_sensitivity,
        force,
        adapters,
        sync_on_launch,
        execution_mode,
        root,
        reporter,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn sync_with_loaded_root_at_paths(
    install_paths: &InstallPaths,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    execution_mode: ExecutionMode,
    root: LoadedManifest,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_mode(
        install_paths,
        cache_root,
        if locked {
            SyncMode::Locked
        } else {
            SyncMode::Normal
        },
        allow_high_sensitivity,
        force,
        adapters,
        sync_on_launch,
        execution_mode,
        Some(root),
        reporter,
    )
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub fn resolve_project_for_sync(
    root: &Path,
    cache_root: &Path,
    reporter: &Reporter,
) -> Result<Resolution> {
    resolve_project(root, cache_root, ResolveMode::Sync, reporter, None, None)
}

pub fn resolve_project_from_existing_lockfile_in_dir(
    cwd: &Path,
    cache_root: &Path,
    _selected_adapters: Adapters,
    reporter: &Reporter,
) -> Result<(Resolution, Lockfile)> {
    let lockfile_path = cwd.join(LOCKFILE_NAME);
    if !lockfile_path.exists() {
        bail!("missing {}", LOCKFILE_NAME);
    }

    let lockfile = Lockfile::read(&lockfile_path)?;
    let resolution = resolve_project(
        cwd,
        cache_root,
        ResolveMode::Doctor,
        reporter,
        Some(&lockfile),
        None,
    )?;

    Ok((resolution, lockfile))
}

impl Resolution {
    fn managed_migrations(&self) -> &[ManagedMappingMigration] {
        &self.managed_migrations
    }

    pub fn to_lockfile(
        &self,
        selected_adapters: Adapters,
        runtime_root: &Path,
    ) -> Result<Lockfile> {
        let mut packages = Vec::new();

        for package in &self.packages {
            let source = match &package.source {
                PackageSource::Root => LockedSource {
                    kind: "path".into(),
                    path: Some(".".into()),
                    url: None,
                    tag: None,
                    branch: None,
                    rev: None,
                },
                PackageSource::Path { path, tag } => LockedSource {
                    kind: "path".into(),
                    path: Some(display_path(path)),
                    url: None,
                    tag: tag.clone(),
                    branch: None,
                    rev: None,
                },
                PackageSource::Git {
                    url,
                    subpath,
                    tag,
                    branch,
                    rev,
                } => LockedSource {
                    kind: "git".into(),
                    path: subpath.as_ref().map(|path| display_path(path)),
                    url: Some(url.clone()),
                    tag: tag.clone(),
                    branch: branch.clone(),
                    rev: Some(rev.clone()),
                },
            };

            let package_role = match package.source {
                PackageSource::Root => PackageRole::Root,
                _ => PackageRole::Dependency,
            };
            let mut dependencies = package_dependency_aliases(package, package_role)?;
            dependencies.sort();

            packages.push(LockedPackage {
                alias: package.alias.clone(),
                name: package.manifest.effective_name(),
                version_tag: match &package.source {
                    PackageSource::Git { tag, .. } => package
                        .manifest
                        .effective_version()
                        .map(|v| v.to_string())
                        .or_else(|| tag.clone()),
                    PackageSource::Path { tag, .. } => package
                        .manifest
                        .effective_version()
                        .map(|v| v.to_string())
                        .or_else(|| tag.clone()),
                    PackageSource::Root => {
                        package.manifest.effective_version().map(|v| v.to_string())
                    }
                },
                source,
                digest: package.digest.clone(),
                selected_components: package.selected_components.clone(),
                skills: emitted_artifact_ids(
                    package,
                    DependencyComponent::Skills,
                    package
                        .manifest
                        .discovered
                        .skills
                        .iter()
                        .map(|item| &item.id),
                ),
                agents: emitted_artifact_ids(
                    package,
                    DependencyComponent::Agents,
                    package
                        .manifest
                        .discovered
                        .agents
                        .iter()
                        .map(|item| &item.id),
                ),
                rules: emitted_artifact_ids(
                    package,
                    DependencyComponent::Rules,
                    package
                        .manifest
                        .discovered
                        .rules
                        .iter()
                        .map(|item| &item.id),
                ),
                commands: emitted_artifact_ids(
                    package,
                    DependencyComponent::Commands,
                    package
                        .manifest
                        .discovered
                        .commands
                        .iter()
                        .map(|item| &item.id),
                ),
                mcp_servers: sorted_ids(package.manifest.manifest.mcp_servers.keys()),
                dependencies,
                capabilities: package.manifest.manifest.capabilities.clone(),
            });
        }

        Ok(Lockfile::new(
            packages,
            self.lockfile_managed_files(selected_adapters, runtime_root)?,
        ))
    }

    pub fn managed_paths(
        &self,
        runtime_root: &Path,
        selected_adapters: Adapters,
    ) -> Result<HashSet<PathBuf>> {
        let lockfile = self.to_lockfile(selected_adapters, runtime_root)?;
        lockfile.managed_paths(runtime_root)
    }

    fn lockfile_managed_files(
        &self,
        selected_adapters: Adapters,
        runtime_root: &Path,
    ) -> Result<Vec<String>> {
        let package_roots = self
            .packages
            .iter()
            .map(|package| (package.clone(), package.root.clone()))
            .collect::<Vec<_>>();
        let mut managed_files =
            build_output_plan(runtime_root, &package_roots, selected_adapters, None, false)?
                .managed_files;
        managed_files.extend(workspace_marketplace_managed_files(self)?);
        managed_files.sort();
        managed_files.dedup();
        Ok(managed_files)
    }
}

fn package_dependency_aliases(
    package: &ResolvedPackage,
    package_role: PackageRole,
) -> Result<Vec<String>> {
    let mut dependencies: Vec<_> = package
        .manifest
        .manifest
        .active_dependency_entries_for_role(package_role)
        .into_iter()
        .map(|entry| entry.alias.to_string())
        .collect();

    if package_role == PackageRole::Dependency
        && package.manifest.manifest.workspace.is_none()
        && package.manifest.discovered.is_empty()
    {
        let selected = package
            .selected_workspace_members
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect::<HashSet<_>>();
        dependencies.retain(|alias| selected.contains(alias));
    }

    let workspace_members = package.manifest.resolved_workspace_members()?;
    if !workspace_members.is_empty() {
        let selected = match &package.selected_workspace_members {
            Some(selected) => selected.iter().cloned().collect::<HashSet<_>>(),
            None if package_role == PackageRole::Root => workspace_members
                .iter()
                .map(|member| member.id.clone())
                .collect::<HashSet<_>>(),
            None => HashSet::new(),
        };
        dependencies.extend(
            workspace_members
                .into_iter()
                .filter(|member| selected.contains(&member.id))
                .map(|member| member.id),
        );
    }

    dependencies.sort();
    dependencies.dedup();
    Ok(dependencies)
}

fn planned_workspace_marketplace_files(
    root: &LoadedManifest,
    runtime_root: &Path,
) -> Result<Vec<ManagedFile>> {
    if root.manifest.workspace.is_none() {
        return Ok(Vec::new());
    }

    let members = root
        .workspace_member_statuses()?
        .into_iter()
        .filter(|member| member.enabled)
        .collect::<Vec<_>>();
    if members.is_empty() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    let claude_marketplace_name = workspace_marketplace_name(root);
    let claude_marketplace_owner_name = workspace_marketplace_owner_name(root);
    let claude_plugins = members
        .iter()
        .map(|member| {
            let member_root = root.resolve_path(&member.path)?;
            let manifest = load_dependency_from_dir(&member_root)?;
            let mut value = serde_json::Map::from_iter([
                (
                    "name".to_string(),
                    serde_json::Value::String(
                        member
                            .name
                            .clone()
                            .unwrap_or_else(|| manifest.effective_name()),
                    ),
                ),
                (
                    "source".to_string(),
                    serde_json::Value::String(display_path(&member.path)),
                ),
            ]);
            if let Some(version) = manifest
                .effective_version()
                .map(|version| version.to_string())
            {
                value.insert("version".to_string(), serde_json::Value::String(version));
            }
            Ok(serde_json::Value::Object(value))
        })
        .collect::<Result<Vec<_>>>()?;
    files.push(ManagedFile {
        path: runtime_root.join(".claude-plugin/marketplace.json"),
        contents: serde_json::to_vec_pretty(&serde_json::json!({
            "name": claude_marketplace_name,
            "owner": {
                "name": claude_marketplace_owner_name,
            },
            "plugins": claude_plugins,
        }))?,
    });

    let codex_plugins = members
        .iter()
        .filter_map(|member| {
            member.codex.as_ref().map(|codex| {
                serde_json::json!({
                    "name": member.name.clone().unwrap_or_else(|| member.id.clone()),
                    "source": {
                        "source": "local",
                        "path": codex_workspace_plugin_path(&member.path),
                    },
                    "policy": {
                        "installation": codex.installation,
                        "authentication": codex.authentication,
                    },
                    "category": codex.category,
                })
            })
        })
        .collect::<Vec<_>>();
    if !codex_plugins.is_empty() {
        files.push(ManagedFile {
            path: runtime_root.join(".agents/plugins/marketplace.json"),
            contents: serde_json::to_vec_pretty(&serde_json::json!({
                "name": claude_marketplace_name,
                "plugins": codex_plugins,
            }))?,
        });
    }

    Ok(files)
}

fn codex_workspace_plugin_path(member_path: &Path) -> String {
    let path = display_path(member_path);
    if path.starts_with("./") {
        path
    } else {
        format!("./{path}")
    }
}

fn workspace_marketplace_name(root: &LoadedManifest) -> String {
    let source_name = root
        .manifest
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| workspace_marketplace_root_basename(&root.root));
    normalize_workspace_marketplace_name(&source_name)
}

fn workspace_marketplace_owner_name(root: &LoadedManifest) -> String {
    root.manifest
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| workspace_marketplace_root_basename(&root.root))
}

fn workspace_marketplace_root_basename(root: &Path) -> String {
    root.file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| String::from("agentpack"))
}

fn normalize_workspace_marketplace_name(value: &str) -> String {
    let mut normalized = String::new();

    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            normalized.push(character.to_ascii_lowercase());
        } else if !normalized.ends_with('-') {
            normalized.push('-');
        }
    }

    let normalized = normalized.trim_matches('-').to_string();
    if normalized.is_empty() {
        String::from("agentpack")
    } else {
        normalized
    }
}

fn workspace_marketplace_managed_files(resolution: &Resolution) -> Result<Vec<String>> {
    let Some(root) = resolution
        .packages
        .iter()
        .find(|package| matches!(package.source, PackageSource::Root))
        .map(|package| &package.manifest)
    else {
        return Ok(Vec::new());
    };
    Ok(planned_workspace_marketplace_files(root, &root.root)?
        .into_iter()
        .map(|file| display_path(file.path.strip_prefix(&root.root).unwrap_or(&file.path)))
        .collect())
}

fn sorted_ids<'a>(ids: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut ids: Vec<_> = ids.cloned().collect();
    ids.sort();
    ids
}

fn emitted_artifact_ids<'a>(
    package: &ResolvedPackage,
    component: DependencyComponent,
    ids: impl Iterator<Item = &'a String>,
) -> Vec<String> {
    if package.emits_runtime_outputs() && package.selects_component(component) {
        sorted_ids(ids)
    } else {
        Vec::new()
    }
}

impl ResolvedPackage {
    pub fn emits_runtime_outputs(&self) -> bool {
        !matches!(self.source, PackageSource::Root) || self.manifest.manifest.publish_root
    }

    pub fn selects_component(&self, component: DependencyComponent) -> bool {
        self.selected_components
            .as_ref()
            .is_none_or(|components| components.contains(&component))
    }

    pub fn package_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = self.manifest.package_files()?;
        files.extend(self.extra_package_files.iter().cloned());
        files.sort();
        files.dedup();
        Ok(files)
    }

    pub fn managed_paths(&self) -> &[ResolvedManagedPath] {
        &self.managed_paths
    }
}

impl SnapshotSource for ResolvedPackage {
    fn digest(&self) -> &str {
        &self.digest
    }

    fn package_root(&self) -> &Path {
        &self.manifest.root
    }

    fn package_files(&self) -> Result<Vec<PathBuf>> {
        ResolvedPackage::package_files(self)
    }

    fn read_package_file(&self, path: &Path) -> Result<Vec<u8>> {
        self.manifest.read_package_file(path)
    }
}

#[cfg(test)]
mod tests;
