use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::adapters::{Adapter, Adapters, ManagedFile, build_output_plan};
use crate::execution::{ExecutionMode, PreviewChange};
use crate::git::{
    current_rev, ensure_git_dependency, ensure_git_dependency_at_rev, shared_checkout_path,
    shared_repository_path, validate_shared_checkout,
};
use crate::lockfile::{LOCKFILE_NAME, LockedPackage, LockedSource, Lockfile};
use crate::manifest::{
    DependencyComponent, DependencySourceKind, DependencySpec, LoadedManifest, PackageRole,
    RequestedGitRef, load_dependency_from_dir, load_root_from_dir,
};
use crate::paths::display_path;
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
    pub direct_managed_paths: Vec<ResolvedManagedPath>,
    extra_package_files: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedManagedPath {
    pub source_root: PathBuf,
    pub target_root: PathBuf,
    pub ownership_root: PathBuf,
    pub files: Vec<ResolvedManagedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResolvedManagedFile {
    pub source_relative: PathBuf,
    pub target_relative: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SyncSummary {
    pub package_count: usize,
    pub adapters: Vec<Adapter>,
    pub managed_file_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorSummary {
    pub package_count: usize,
    pub warnings: Vec<String>,
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

#[derive(Debug, Default)]
struct ResolverState {
    stack: Vec<PathBuf>,
    resolved_by_path: HashMap<PathBuf, ResolvedPackage>,
}

#[derive(Clone, Copy)]
struct ResolveContext<'a> {
    cache_root: &'a Path,
    mode: ResolveMode,
    frozen_lockfile: Option<&'a Lockfile>,
    root_override: Option<&'a LoadedManifest>,
    reporter: &'a Reporter,
}

struct ResolvePackageInput {
    alias: String,
    package_root: PathBuf,
    source: PackageSource,
    role: PackageRole,
    selected_components: Option<Vec<DependencyComponent>>,
    direct_managed_paths: Vec<ResolvedManagedPath>,
    extra_package_files: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct PlannedFileWrite {
    path: PathBuf,
    contents: Vec<u8>,
    create: bool,
}

#[derive(Debug, Clone)]
struct SyncExecutionPlan {
    project_root: PathBuf,
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

pub fn sync_in_dir_with_adapters(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_mode(
        cwd,
        cache_root,
        if locked {
            SyncMode::Locked
        } else {
            SyncMode::Normal
        },
        allow_high_sensitivity,
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
    adapters: &[Adapter],
    sync_on_launch: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_mode(
        cwd,
        cache_root,
        SyncMode::Frozen,
        allow_high_sensitivity,
        adapters,
        sync_on_launch,
        ExecutionMode::Apply,
        None,
        reporter,
    )
}

pub fn sync_in_dir_with_adapters_dry_run(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_mode(
        cwd,
        cache_root,
        if locked {
            SyncMode::Locked
        } else {
            SyncMode::Normal
        },
        allow_high_sensitivity,
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
    adapters: &[Adapter],
    sync_on_launch: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_mode(
        cwd,
        cache_root,
        SyncMode::Frozen,
        allow_high_sensitivity,
        adapters,
        sync_on_launch,
        ExecutionMode::DryRun,
        None,
        reporter,
    )
}

#[allow(clippy::too_many_arguments)]
fn sync_in_dir_with_adapters_mode(
    cwd: &Path,
    cache_root: &Path,
    sync_mode: SyncMode,
    allow_high_sensitivity: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    execution_mode: ExecutionMode,
    root_override: Option<LoadedManifest>,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    let mut collision_resolver = TtyManagedCollisionResolver;
    sync_in_dir_with_adapters_mode_and_collision_resolution(
        cwd,
        cache_root,
        sync_mode,
        allow_high_sensitivity,
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
    cwd: &Path,
    cache_root: &Path,
    sync_mode: SyncMode,
    allow_high_sensitivity: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    execution_mode: ExecutionMode,
    root_override: Option<LoadedManifest>,
    mut collision_resolver: Option<&mut dyn ManagedCollisionResolver>,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    crate::relay::ensure_no_pending_relay_edits_in_dir(cwd, cache_root)?;
    let has_root_override = root_override.is_some();
    let original_root = load_root_from_dir(cwd)?;
    let mut root = root_override.unwrap_or_else(|| original_root.clone());
    let mut adopted_owned_paths = HashSet::new();
    let selection = resolve_adapter_selection(
        cwd,
        &root.manifest,
        adapters,
        !sync_mode.checks_lockfile() && should_prompt_for_adapter(),
    )?;
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
                "launch hook configuration must be persisted before running {}; rerun without `--locked` or `--frozen`, or set `[launch_hooks] sync_on_startup = true` in nodus.toml",
                sync_mode.flag(),
            );
        }
        root.manifest.set_sync_on_launch(true);
    }
    if has_root_override || selection.should_persist || sync_on_launch {
        root = original_root.with_manifest(root.manifest.clone(), PackageRole::Root)?;
    }

    let lockfile_path = cwd.join(LOCKFILE_NAME);
    let existing_lockfile = if lockfile_path.exists() {
        Some(if sync_mode.checks_lockfile() {
            Lockfile::read(&lockfile_path)?
        } else {
            Lockfile::read_for_sync(&lockfile_path)?
        })
    } else {
        None
    };
    if let Some(lockfile) = existing_lockfile.as_ref() {
        if !lockfile.uses_current_schema() {
            reporter.note(format!(
                "upgrading {LOCKFILE_NAME} from version {} to {}",
                lockfile.version,
                Lockfile::current_version()
            ))?;
        }
    }
    if sync_mode.installs_from_lockfile() && existing_lockfile.is_none() {
        bail!(
            "`--frozen` requires an existing {} in {}",
            LOCKFILE_NAME,
            cwd.display()
        );
    }

    loop {
        reporter.status("Resolving", format!("package graph in {}", cwd.display()))?;
        let resolution = resolve_project(
            cwd,
            cache_root,
            ResolveMode::Sync,
            reporter,
            existing_lockfile
                .as_ref()
                .filter(|_| sync_mode.installs_from_lockfile()),
            Some(&root),
        )?;
        reporter.status("Checking", "declared capabilities")?;
        enforce_capabilities(&resolution, allow_high_sensitivity, reporter)?;
        reporter.status(
            "Snapshotting",
            format!("{} packages", resolution.packages.len()),
        )?;
        let stored_packages = snapshot_resolution(cache_root, &resolution)?;

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
            cwd,
            &package_snapshots,
            selected_adapters,
            existing_lockfile.as_ref(),
            true,
        )?;
        let planned_files = &output_plan.files;
        let desired_paths = resolution.managed_paths(cwd, selected_adapters)?;
        let lockfile = resolution.to_lockfile(selected_adapters)?;
        let mut owned_paths = load_owned_paths(cwd, existing_lockfile.as_ref())?;
        if existing_lockfile.is_none() {
            owned_paths.extend(recover_runtime_owned_paths(cwd, &desired_paths));
        }
        owned_paths.extend(adopted_owned_paths.iter().cloned());

        if sync_mode.checks_lockfile() {
            let Some(existing) = existing_lockfile.as_ref() else {
                bail!(
                    "{} requires an existing {} in {}",
                    sync_mode.flag(),
                    LOCKFILE_NAME,
                    cwd.display()
                );
            };
            if *existing != lockfile {
                bail!("{}", checked_sync_lockfile_out_of_date_message());
            }
        }

        if let Some(unmanaged_collision) =
            find_unmanaged_collision(planned_files, &owned_paths, cwd)
        {
            let Some(managed_collision) =
                find_managed_collision(cwd, &resolution, &unmanaged_collision)
            else {
                bail!(
                    "refusing to overwrite unmanaged file {}",
                    display_path(&unmanaged_collision.path)
                );
            };
            let Some(resolver) = collision_resolver.as_deref_mut() else {
                bail!(
                    "{}",
                    unmanaged_collision_guidance(cwd, &managed_collision, sync_mode)
                );
            };
            match resolver.resolve(cwd, &managed_collision)? {
                ManagedCollisionChoice::Adopt => {
                    let ownership_root = cwd.join(&managed_collision.ownership_root);
                    reporter.note(format!(
                        "adopting managed target {}",
                        display_path(&ownership_root)
                    ))?;
                    adopted_owned_paths.insert(ownership_root);
                    continue;
                }
                ManagedCollisionChoice::RemoveMapping => {
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
                        display_path(&cwd.join(&managed_collision.ownership_root)),
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
            &owned_paths,
            &desired_paths,
            planned_files,
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
    adapters: &[Adapter],
    sync_on_launch: bool,
    execution_mode: ExecutionMode,
    root: LoadedManifest,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_mode(
        cwd,
        cache_root,
        if locked {
            SyncMode::Locked
        } else {
            SyncMode::Normal
        },
        allow_high_sensitivity,
        adapters,
        sync_on_launch,
        execution_mode,
        Some(root),
        reporter,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_sync_execution_plan(
    original_root: &LoadedManifest,
    working_root: &LoadedManifest,
    lockfile_path: &Path,
    lockfile: &Lockfile,
    owned_paths: &HashSet<PathBuf>,
    desired_paths: &HashSet<PathBuf>,
    planned_files: &[ManagedFile],
    warnings: Vec<String>,
    summary: SyncSummary,
    sync_mode: SyncMode,
) -> Result<SyncExecutionPlan> {
    let manifest_write = planned_manifest_write(original_root, working_root)?;
    let mut removals = planned_stale_paths(owned_paths, desired_paths);
    removals.extend(planned_paths_to_replace(
        planned_files,
        owned_paths,
        &working_root.root,
    )?);
    removals.sort();
    removals.dedup();
    let lockfile_write = if sync_mode.checks_lockfile() {
        None
    } else {
        Some(planned_lockfile_write(lockfile_path, lockfile)?)
    };

    Ok(SyncExecutionPlan {
        project_root: working_root.root.clone(),
        owned_paths: owned_paths.clone(),
        desired_paths: desired_paths.clone(),
        manifest_write,
        removals,
        managed_writes: planned_files.to_vec(),
        lockfile_write,
        warnings,
        summary,
    })
}

fn execute_sync_plan(
    plan: &SyncExecutionPlan,
    execution_mode: ExecutionMode,
    reporter: &Reporter,
) -> Result<()> {
    if execution_mode.is_dry_run() {
        if let Some(write) = &plan.manifest_write {
            reporter.preview(&planned_write_preview_change(write))?;
        }
        for path in &plan.removals {
            reporter.preview(&PreviewChange::Remove(path.clone()))?;
        }
        if !plan.managed_writes.is_empty() {
            reporter.status("Preview", "managed runtime outputs")?;
            for file in &plan.managed_writes {
                let change = if file.path.exists() {
                    PreviewChange::Write(file.path.clone())
                } else {
                    PreviewChange::Create(file.path.clone())
                };
                reporter.preview(&change)?;
            }
        }
        if let Some(write) = &plan.lockfile_write {
            reporter.preview(&planned_write_preview_change(write))?;
        }
    } else {
        if let Some(write) = &plan.manifest_write {
            reporter.status("Writing", write.path.display())?;
            write_atomic(&write.path, &write.contents)?;
        }
        prune_stale_files(&plan.owned_paths, &plan.desired_paths, &plan.project_root)?;
        prepare_managed_paths_for_write(
            &plan.managed_writes,
            &plan.owned_paths,
            &plan.project_root,
        )?;
        reporter.status("Writing", "managed runtime outputs")?;
        write_managed_files(&plan.managed_writes)?;
        if let Some(write) = &plan.lockfile_write {
            reporter.status("Writing", write.path.display())?;
            write_atomic(&write.path, &write.contents)?;
        }
    }

    for warning in &plan.warnings {
        reporter.warning(warning)?;
    }

    Ok(())
}

fn planned_manifest_write(
    original_root: &LoadedManifest,
    working_root: &LoadedManifest,
) -> Result<Option<PlannedFileWrite>> {
    let Some(path) = &working_root.manifest_path else {
        return Ok(None);
    };
    let contents = working_root
        .read_package_file(path)
        .with_context(|| format!("failed to read manifest {}", path.display()))?;
    let current = if path.exists() {
        Some(
            std::fs::read(path)
                .with_context(|| format!("failed to read manifest {}", path.display()))?,
        )
    } else {
        None
    };
    if original_root.manifest_path.as_deref() == Some(path)
        && current
            .as_ref()
            .is_some_and(|existing| *existing == contents)
    {
        Ok(None)
    } else {
        Ok(Some(PlannedFileWrite {
            path: path.clone(),
            contents,
            create: !path.exists(),
        }))
    }
}

fn planned_lockfile_write(path: &Path, lockfile: &Lockfile) -> Result<PlannedFileWrite> {
    let contents = toml::to_string_pretty(lockfile)
        .context("failed to serialize lockfile")?
        .into_bytes();
    Ok(PlannedFileWrite {
        path: path.to_path_buf(),
        create: !path.exists(),
        contents,
    })
}

fn planned_stale_paths(
    owned_paths: &HashSet<PathBuf>,
    desired_paths: &HashSet<PathBuf>,
) -> Vec<PathBuf> {
    let mut removals = owned_paths
        .difference(desired_paths)
        .filter(|path| fs::symlink_metadata(path).is_ok())
        .cloned()
        .collect::<Vec<_>>();
    removals.sort();
    removals
}

fn planned_paths_to_replace(
    planned_files: &[ManagedFile],
    owned_paths: &HashSet<PathBuf>,
    project_root: &Path,
) -> Result<Vec<PathBuf>> {
    let mut removed = HashSet::new();

    for file in planned_files {
        if file.path.is_dir()
            && path_is_owned(&file.path, owned_paths)
            && removed.insert(file.path.clone())
        {
            continue;
        }

        let mut current = file.path.parent();
        while let Some(parent) = current {
            if parent == project_root {
                break;
            }
            if parent.is_file()
                && path_is_owned(parent, owned_paths)
                && removed.insert(parent.to_path_buf())
            {
                break;
            }
            current = parent.parent();
        }
    }

    let mut removals = removed.into_iter().collect::<Vec<_>>();
    removals.sort();
    Ok(removals)
}

fn planned_write_preview_change(write: &PlannedFileWrite) -> PreviewChange {
    if write.create {
        PreviewChange::Create(write.path.clone())
    } else {
        PreviewChange::Write(write.path.clone())
    }
}

#[cfg(test)]
pub fn resolve_project_for_sync(
    root: &Path,
    cache_root: &Path,
    reporter: &Reporter,
) -> Result<Resolution> {
    resolve_project(root, cache_root, ResolveMode::Sync, reporter, None, None)
}

pub fn doctor_in_dir(cwd: &Path, cache_root: &Path, reporter: &Reporter) -> Result<DoctorSummary> {
    let root = load_root_from_dir(cwd)?;
    let selection = resolve_adapter_selection(cwd, &root.manifest, &[], false)?;
    let selected_adapters = Adapters::from_slice(&selection.adapters);
    reporter.status(
        "Checking",
        "manifest, lockfile, shared store, and managed outputs",
    )?;
    let resolution = resolve_project(cwd, cache_root, ResolveMode::Doctor, reporter, None, None)?;
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
    let output_plan = build_output_plan(
        cwd,
        &package_roots,
        selected_adapters,
        Some(&existing_lockfile),
        true,
    )?;
    let planned_files = &output_plan.files;
    let desired_paths = resolution.managed_paths(cwd, selected_adapters)?;
    let expected_lockfile = resolution.to_lockfile(selected_adapters)?;
    if existing_lockfile != expected_lockfile {
        bail!("{}", lockfile_out_of_date_message());
    }
    let owned_paths = load_owned_paths(cwd, Some(&existing_lockfile))?;

    validate_collisions(planned_files, &owned_paths, cwd)?;
    validate_state_consistency(&owned_paths, &desired_paths, planned_files)?;

    resolution
        .packages
        .par_iter()
        .map(|package| validate_git_package(package, cache_root))
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<Result<Vec<_>>>()?;

    let warnings = resolution
        .warnings
        .iter()
        .chain(output_plan.warnings.iter())
        .cloned()
        .collect::<Vec<_>>();

    for warning in &warnings {
        reporter.warning(warning)?;
    }

    Ok(DoctorSummary {
        package_count: resolution.packages.len(),
        warnings,
    })
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

fn validate_git_package(package: &ResolvedPackage, cache_root: &Path) -> Result<()> {
    let PackageSource::Git { url, rev, .. } = &package.source else {
        return Ok(());
    };

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
    validate_shared_checkout(&package.root, &mirror_path, url)
}

fn resolve_project(
    root: &Path,
    cache_root: &Path,
    mode: ResolveMode,
    reporter: &Reporter,
    frozen_lockfile: Option<&Lockfile>,
    root_override: Option<&LoadedManifest>,
) -> Result<Resolution> {
    let project_root = root
        .canonicalize()
        .with_context(|| format!("failed to access {}", root.display()))?;
    let context = ResolveContext {
        cache_root,
        mode,
        frozen_lockfile,
        root_override,
        reporter,
    };
    let mut state = ResolverState::default();
    resolve_package(
        &context,
        ResolvePackageInput {
            alias: "root".to_string(),
            package_root: project_root.clone(),
            source: PackageSource::Root,
            role: PackageRole::Root,
            selected_components: None,
            direct_managed_paths: Vec::new(),
            extra_package_files: Vec::new(),
        },
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
    input: ResolvePackageInput,
    state: &mut ResolverState,
) -> Result<ResolvedPackage> {
    let ResolvePackageInput {
        alias,
        package_root,
        source,
        role,
        selected_components,
        direct_managed_paths,
        extra_package_files,
    } = input;
    if let Some(existing) = state.resolved_by_path.get_mut(&package_root) {
        existing.selected_components =
            union_selected_components(existing.selected_components.clone(), selected_components);
        if !direct_managed_paths.is_empty() {
            existing.direct_managed_paths = merge_direct_managed_paths(
                &package_root,
                &existing.direct_managed_paths,
                &direct_managed_paths,
            )?;
            merge_extra_package_files(&mut existing.extra_package_files, &extra_package_files);
            existing.digest =
                compute_package_digest(&existing.manifest, &existing.extra_package_files)?;
        }
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
        PackageRole::Root => {
            if let Some(root_override) = context.root_override {
                root_override.clone()
            } else {
                load_root_from_dir(&package_root)?
            }
        }
        PackageRole::Dependency => load_dependency_from_dir(&package_root)?,
    };

    let dependencies = manifest
        .manifest
        .dependency_entries_for_role(role)
        .into_iter()
        .map(|entry| resolve_dependency(&manifest, role, entry.alias, entry.spec, context, state))
        .collect::<Result<Vec<_>>>()?;

    let digest = compute_package_digest(&manifest, &extra_package_files)?;
    let resolved = ResolvedPackage {
        alias,
        root: package_root.clone(),
        manifest,
        source,
        digest,
        selected_components,
        direct_managed_paths,
        extra_package_files,
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
    parent_role: PackageRole,
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
            let (direct_managed_paths, extra_package_files) =
                resolve_direct_managed_paths(parent_role, alias, dependency, &dependency_root)?;
            resolve_package(
                context,
                ResolvePackageInput {
                    alias: alias.to_string(),
                    package_root: dependency_root,
                    source,
                    role: PackageRole::Dependency,
                    selected_components: dependency.effective_selected_components(),
                    direct_managed_paths,
                    extra_package_files,
                },
                state,
            )
        }
        DependencySourceKind::Git => {
            let url = dependency.resolved_git_url()?;
            let requested_ref = dependency.requested_git_ref()?;
            let checkout = if let Some(lockfile) = context.frozen_lockfile {
                let locked = locked_git_source(lockfile, alias, &url, requested_ref)?;
                let rev = locked.rev.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "dependency `{alias}` in {} does not record a git revision",
                        LOCKFILE_NAME
                    )
                })?;
                ensure_git_dependency_at_rev(
                    context.cache_root,
                    &url,
                    locked.tag.as_deref(),
                    locked.branch.as_deref(),
                    rev,
                    context.mode == ResolveMode::Sync,
                    context.reporter,
                )?
            } else {
                ensure_git_dependency(
                    context.cache_root,
                    &url,
                    Some(requested_ref),
                    context.mode == ResolveMode::Sync,
                    context.reporter,
                )?
            };
            let source = PackageSource::Git {
                url: checkout.url,
                tag: checkout.tag,
                branch: checkout.branch,
                rev: checkout.rev,
            };
            let (direct_managed_paths, extra_package_files) =
                resolve_direct_managed_paths(parent_role, alias, dependency, &checkout.path)?;
            resolve_package(
                context,
                ResolvePackageInput {
                    alias: alias.to_string(),
                    package_root: checkout.path,
                    source,
                    role: PackageRole::Dependency,
                    selected_components: dependency.effective_selected_components(),
                    direct_managed_paths,
                    extra_package_files,
                },
                state,
            )
        }
    }
}

fn locked_git_source<'a>(
    lockfile: &'a Lockfile,
    alias: &str,
    url: &str,
    requested_ref: RequestedGitRef<'_>,
) -> Result<&'a LockedSource> {
    let matches_requested_ref = |source: &LockedSource| match requested_ref {
        RequestedGitRef::Tag(tag) => source.tag.as_deref() == Some(tag) && source.branch.is_none(),
        RequestedGitRef::Branch(branch) => {
            source.branch.as_deref() == Some(branch) && source.tag.is_none()
        }
        RequestedGitRef::Revision(revision) => {
            source.rev.as_deref() == Some(revision)
                && source.tag.is_none()
                && source.branch.is_none()
        }
        RequestedGitRef::VersionReq(requirement) => {
            source
                .tag
                .as_deref()
                .and_then(crate::git::parse_semver_tag)
                .is_some_and(|version| requirement.matches(&version))
                && source.branch.is_none()
        }
    };

    let mut matching_sources = lockfile
        .packages
        .iter()
        .filter(|package| {
            package.source.kind == "git"
                && package.source.url.as_deref() == Some(url)
                && matches_requested_ref(&package.source)
        })
        .collect::<Vec<_>>();

    if matching_sources.is_empty() {
        bail!(
            "dependency `{alias}` is missing from {}; run `nodus sync` without `--frozen` to regenerate it",
            LOCKFILE_NAME
        );
    }

    if matching_sources.len() > 1 {
        let alias_matches = matching_sources
            .iter()
            .copied()
            .filter(|package| package.alias == alias)
            .collect::<Vec<_>>();
        matching_sources = if alias_matches.is_empty() {
            matching_sources
        } else {
            alias_matches
        };
    }

    if matching_sources.len() != 1 {
        bail!(
            "dependency `{alias}` has ambiguous git entries in {}; run `nodus sync` without `--frozen` to regenerate it",
            LOCKFILE_NAME
        );
    }

    Ok(&matching_sources[0].source)
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

fn resolve_direct_managed_paths(
    parent_role: PackageRole,
    alias: &str,
    dependency: &DependencySpec,
    dependency_root: &Path,
) -> Result<(Vec<ResolvedManagedPath>, Vec<PathBuf>)> {
    if dependency.managed_mappings().is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    if parent_role != PackageRole::Root {
        bail!(
            "dependency `{alias}` field `managed` is supported only for direct dependencies in the root manifest"
        );
    }

    let mut ownership_roots = Vec::<PathBuf>::new();
    let mut concrete_targets = HashSet::<PathBuf>::new();
    let mut mappings = Vec::new();
    let mut extra_package_files = Vec::new();

    for spec in dependency.managed_mappings() {
        let source_root = spec.normalized_source()?;
        let target_root = spec.normalized_target()?;
        validate_managed_ownership_root(alias, &ownership_roots, &target_root)?;

        let source_path =
            resolve_dependency_managed_source_path(alias, dependency_root, &source_root)?;
        let metadata = fs::metadata(&source_path)
            .with_context(|| format!("failed to read managed source {}", source_path.display()))?;
        let files = if metadata.is_file() {
            if !concrete_targets.insert(target_root.clone()) {
                bail!(
                    "dependency `{alias}` field `managed` maps multiple sources into {}",
                    target_root.display()
                );
            }
            extra_package_files.push(source_path);
            vec![ResolvedManagedFile {
                source_relative: source_root.clone(),
                target_relative: target_root.clone(),
            }]
        } else if metadata.is_dir() {
            let mut files = Vec::new();
            for entry in walkdir::WalkDir::new(&source_path) {
                let entry = entry?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let relative = entry.path().strip_prefix(&source_path).with_context(|| {
                    format!("failed to make {} relative", entry.path().display())
                })?;
                let source_relative = source_root.join(relative);
                let target_relative = target_root.join(relative);
                if !concrete_targets.insert(target_relative.clone()) {
                    bail!(
                        "dependency `{alias}` field `managed` maps multiple sources into {}",
                        target_relative.display()
                    );
                }
                extra_package_files.push(entry.path().canonicalize().with_context(|| {
                    format!("failed to canonicalize {}", entry.path().display())
                })?);
                files.push(ResolvedManagedFile {
                    source_relative,
                    target_relative,
                });
            }
            files.sort();
            files
        } else {
            bail!(
                "dependency `{alias}` managed source {} must be a file or directory",
                source_root.display()
            );
        };

        ownership_roots.push(target_root.clone());
        mappings.push(ResolvedManagedPath {
            source_root,
            target_root: target_root.clone(),
            ownership_root: target_root,
            files,
        });
    }

    extra_package_files.sort();
    extra_package_files.dedup();
    Ok((mappings, extra_package_files))
}

fn validate_managed_ownership_root(
    alias: &str,
    existing_roots: &[PathBuf],
    candidate: &Path,
) -> Result<()> {
    if let Some(existing) = existing_roots.iter().find(|existing| {
        existing.as_path().starts_with(candidate) || candidate.starts_with(existing)
    }) {
        bail!(
            "dependency `{alias}` field `managed` has overlapping target roots `{}` and `{}`",
            existing.display(),
            candidate.display()
        );
    }
    Ok(())
}

fn resolve_dependency_managed_source_path(
    alias: &str,
    dependency_root: &Path,
    source_root: &Path,
) -> Result<PathBuf> {
    let canonical_dependency_root = dependency_root
        .canonicalize()
        .with_context(|| format!("failed to access {}", dependency_root.display()))?;
    let source_path = dependency_root.join(source_root);
    let canonical = source_path
        .canonicalize()
        .with_context(|| format!("missing managed source {}", source_path.display()))?;
    if !canonical.starts_with(&canonical_dependency_root) {
        bail!(
            "dependency `{alias}` managed source {} escapes the dependency root {}",
            source_root.display(),
            canonical_dependency_root.display()
        );
    }
    Ok(canonical)
}

fn merge_direct_managed_paths(
    package_root: &Path,
    existing: &[ResolvedManagedPath],
    incoming: &[ResolvedManagedPath],
) -> Result<Vec<ResolvedManagedPath>> {
    let mut merged = existing.to_vec();

    for path in incoming {
        if merged.contains(path) {
            continue;
        }

        if let Some(conflict) = merged.iter().find(|existing| {
            existing.ownership_root.starts_with(&path.ownership_root)
                || path.ownership_root.starts_with(&existing.ownership_root)
        }) {
            bail!(
                "direct-managed targets for {} overlap at `{}` and `{}`",
                package_root.display(),
                conflict.ownership_root.display(),
                path.ownership_root.display()
            );
        }

        let existing_targets = merged
            .iter()
            .flat_map(|mapping| mapping.files.iter().map(|file| &file.target_relative))
            .collect::<HashSet<_>>();
        if let Some(conflict) = path
            .files
            .iter()
            .find(|file| existing_targets.contains(&file.target_relative))
        {
            bail!(
                "direct-managed targets for {} overlap at `{}`",
                package_root.display(),
                conflict.target_relative.display()
            );
        }

        merged.push(path.clone());
    }

    Ok(merged)
}

fn merge_extra_package_files(target: &mut Vec<PathBuf>, extra_files: &[PathBuf]) {
    target.extend(extra_files.iter().cloned());
    target.sort();
    target.dedup();
}

fn compute_package_digest(
    manifest: &LoadedManifest,
    extra_package_files: &[PathBuf],
) -> Result<String> {
    let mut files = manifest.package_files()?;
    files.extend(extra_package_files.iter().cloned());
    files.sort();
    files.dedup();

    let file_payloads = files
        .par_iter()
        .map(|file| {
            let relative = file
                .strip_prefix(&manifest.root)
                .with_context(|| format!("failed to make {} relative", file.display()))?
                .to_path_buf();
            let contents = manifest
                .read_package_file(file)
                .with_context(|| format!("failed to read {} for hashing", file.display()))?;
            Ok((relative, contents))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<Result<Vec<_>>>()?;

    let mut hasher = Sha256::new();
    for (relative, contents) in file_payloads {
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(contents);
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
                    tag,
                    branch,
                    rev,
                } => LockedSource {
                    kind: "git".into(),
                    path: None,
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
            let mut dependencies: Vec<_> = package
                .manifest
                .manifest
                .dependency_entries_for_role(package_role)
                .into_iter()
                .map(|entry| entry.alias.to_string())
                .collect();
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
                mcp_servers: sorted_ids(package.manifest.manifest.mcp_servers.keys()),
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
        Ok(build_output_plan(
            &self.project_root,
            &package_roots,
            selected_adapters,
            None,
            false,
        )?
        .managed_files)
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

    pub fn package_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = self.manifest.package_files()?;
        files.extend(self.extra_package_files.iter().cloned());
        files.sort();
        files.dedup();
        Ok(files)
    }

    pub fn direct_managed_paths(&self) -> &[ResolvedManagedPath] {
        &self.direct_managed_paths
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
    project_root: &Path,
) -> Result<()> {
    if let Some(collision) = find_unmanaged_collision(planned_files, owned_paths, project_root) {
        bail!(
            "refusing to overwrite unmanaged file {}",
            display_path(&collision.path)
        );
    }

    Ok(())
}

fn find_unmanaged_collision(
    planned_files: &[ManagedFile],
    owned_paths: &HashSet<PathBuf>,
    project_root: &Path,
) -> Option<UnmanagedCollision> {
    for file in planned_files {
        if file.path.exists()
            && !path_is_owned(&file.path, owned_paths)
            && !allows_managed_merge(project_root, &file.path)
        {
            return Some(UnmanagedCollision {
                path: file.path.clone(),
            });
        }

        let mut current = file.path.parent();
        while let Some(parent) = current {
            if parent == project_root {
                break;
            }
            if parent.exists() && parent.is_file() && !path_is_owned(parent, owned_paths) {
                return Some(UnmanagedCollision {
                    path: parent.to_path_buf(),
                });
            }
            current = parent.parent();
        }
    }

    None
}

fn allows_managed_merge(project_root: &Path, path: &Path) -> bool {
    path == project_root.join(".mcp.json")
}

fn find_managed_collision(
    project_root: &Path,
    resolution: &Resolution,
    collision: &UnmanagedCollision,
) -> Option<ManagedCollision> {
    for package in &resolution.packages {
        for managed_path in package.direct_managed_paths() {
            let ownership_root = project_root.join(&managed_path.ownership_root);
            if collision.path == ownership_root
                || collision.path.starts_with(&ownership_root)
                || ownership_root.starts_with(&collision.path)
            {
                return Some(ManagedCollision {
                    alias: package.alias.clone(),
                    ownership_root: managed_path.ownership_root.clone(),
                    collision_path: collision.path.clone(),
                });
            }

            if managed_path.files.iter().any(|file| {
                let target = project_root.join(&file.target_relative);
                collision.path == target || target.starts_with(&collision.path)
            }) {
                return Some(ManagedCollision {
                    alias: package.alias.clone(),
                    ownership_root: managed_path.ownership_root.clone(),
                    collision_path: collision.path.clone(),
                });
            }
        }
    }

    None
}

fn unmanaged_collision_guidance(
    project_root: &Path,
    collision: &ManagedCollision,
    sync_mode: SyncMode,
) -> String {
    format!(
        "refusing to overwrite unmanaged file {}. Managed target {} from dependency `{}` collides with an existing path. Rerun plain `nodus sync` on a TTY to choose whether to adopt that target, remove the managed mapping from `nodus.toml`, or cancel; {} cannot prompt interactively",
        display_path(&collision.collision_path),
        display_path(&project_root.join(&collision.ownership_root)),
        collision.alias,
        sync_mode.flag(),
    )
}

impl ManagedCollisionResolver for TtyManagedCollisionResolver {
    fn resolve(
        &mut self,
        project_root: &Path,
        collision: &ManagedCollision,
    ) -> Result<ManagedCollisionChoice> {
        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        let stderr = io::stderr();
        let mut stderr = stderr.lock();
        prompt_for_managed_collision(project_root, collision, &mut stdin, &mut stderr)
    }
}

fn prompt_for_managed_collision(
    project_root: &Path,
    collision: &ManagedCollision,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<ManagedCollisionChoice> {
    writeln!(
        output,
        "Managed target {} from dependency `{}` collides with existing unmanaged path {}.",
        display_path(&project_root.join(&collision.ownership_root)),
        collision.alias,
        display_path(&collision.collision_path)
    )?;
    writeln!(output, "Choose how to continue:")?;
    writeln!(
        output,
        "  1. adopt  (let Nodus take ownership and overwrite managed files under that target)"
    )?;
    writeln!(
        output,
        "  2. remove (delete the corresponding managed mapping from nodus.toml and continue)"
    )?;
    writeln!(output, "  3. cancel")?;
    write!(output, "> ")?;
    output.flush()?;

    let mut line = String::new();
    input.read_line(&mut line)?;
    parse_managed_collision_choice(&line)
}

fn parse_managed_collision_choice(answer: &str) -> Result<ManagedCollisionChoice> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "1" | "adopt" => Ok(ManagedCollisionChoice::Adopt),
        "2" | "remove" => Ok(ManagedCollisionChoice::RemoveMapping),
        "3" | "cancel" => Ok(ManagedCollisionChoice::Cancel),
        other => bail!("invalid collision resolution `{other}`"),
    }
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
    planned_files
        .par_iter()
        .map(|file| {
            write_atomic(&file.path, &file.contents)
                .with_context(|| format!("failed to write managed file {}", file.path.display()))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect()
}

fn prepare_managed_paths_for_write(
    planned_files: &[ManagedFile],
    owned_paths: &HashSet<PathBuf>,
    project_root: &Path,
) -> Result<()> {
    let mut removed = HashSet::new();

    for file in planned_files {
        if file.path.is_dir()
            && path_is_owned(&file.path, owned_paths)
            && removed.insert(file.path.clone())
        {
            fs::remove_dir_all(&file.path).with_context(|| {
                format!(
                    "failed to replace managed directory {} with a file",
                    file.path.display()
                )
            })?;
            prune_empty_parent_dirs(&file.path, project_root)?;
        }

        let mut current = file.path.parent();
        while let Some(parent) = current {
            if parent == project_root {
                break;
            }
            if parent.is_file()
                && path_is_owned(parent, owned_paths)
                && removed.insert(parent.to_path_buf())
            {
                fs::remove_file(parent).with_context(|| {
                    format!(
                        "failed to replace managed file {} with a directory",
                        parent.display()
                    )
                })?;
                prune_empty_parent_dirs(parent, project_root)?;
            }
            current = parent.parent();
        }
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
        return if lockfile.uses_current_schema() {
            lockfile.managed_paths(project_root)
        } else {
            lockfile.managed_paths_for_sync(project_root)
        };
    }

    Ok(HashSet::new())
}

fn recover_runtime_owned_paths(
    project_root: &Path,
    desired_paths: &HashSet<PathBuf>,
) -> HashSet<PathBuf> {
    desired_paths
        .iter()
        .filter(|path| is_runtime_managed_path(project_root, path))
        .cloned()
        .collect()
}

fn is_runtime_managed_path(project_root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(project_root) else {
        return false;
    };
    let mut components = relative.components();
    let Some(first) = components.next() else {
        return false;
    };
    match first.as_os_str().to_string_lossy().as_ref() {
        ".agents" | ".claude" | ".codex" | ".cursor" | ".opencode" => true,
        ".github" => matches!(
            components.next().map(|component| component.as_os_str().to_string_lossy()),
            Some(second) if second == "skills" || second == "agents"
        ),
        _ => false,
    }
}

fn prune_empty_parent_dirs(path: &Path, project_root: &Path) -> Result<()> {
    let stop_roots = [
        project_root.to_path_buf(),
        project_root.join(".agents"),
        project_root.join(".claude"),
        project_root.join(".codex"),
        project_root.join(".cursor"),
        project_root.join(".github"),
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
    use std::io::{self, Write};
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    use tempfile::TempDir;

    use super::*;
    use crate::adapters::{Adapter, Adapters, namespaced_file_name, namespaced_skill_id};
    use crate::git::{
        AddDependencyOptions, AddSummary, RemoveSummary,
        add_dependency_in_dir_with_adapters as add_dependency_in_dir_with_adapters_impl,
        normalize_alias_from_url, remove_dependency_in_dir as remove_dependency_in_dir_impl,
        shared_checkout_path, shared_repository_path,
    };
    use crate::manifest::{
        DependencyComponent, DependencyKind, MANIFEST_FILE, RequestedGitRef, load_root_from_dir,
    };
    use crate::report::{ColorMode, Reporter};

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
        run(&["config", "core.autocrlf", "false"]);
        write_file(&path.join(".gitattributes"), "* text eol=lf\n");
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

    fn commit_all(path: &Path, message: &str) {
        let output = Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let output = Command::new("git")
            .args(["commit", "-m", message])
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

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn resolve_project(root: &Path, cache_root: &Path, mode: ResolveMode) -> Result<Resolution> {
        let reporter = Reporter::silent();
        super::resolve_project(root, cache_root, mode, &reporter, None, None)
    }

    fn sync_in_dir(
        cwd: &Path,
        cache_root: &Path,
        locked: bool,
        allow_high_sensitivity: bool,
    ) -> Result<SyncSummary> {
        let reporter = Reporter::silent();
        super::sync_in_dir_with_adapters(
            cwd,
            cache_root,
            locked,
            allow_high_sensitivity,
            &[],
            false,
            &reporter,
        )
    }

    fn sync_in_dir_frozen(
        cwd: &Path,
        cache_root: &Path,
        allow_high_sensitivity: bool,
    ) -> Result<SyncSummary> {
        let reporter = Reporter::silent();
        super::sync_in_dir_with_adapters_frozen(
            cwd,
            cache_root,
            allow_high_sensitivity,
            &[],
            false,
            &reporter,
        )
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
            false,
            &reporter,
        )
    }

    struct StubManagedCollisionResolver {
        choice: ManagedCollisionChoice,
    }

    impl ManagedCollisionResolver for StubManagedCollisionResolver {
        fn resolve(
            &mut self,
            _project_root: &Path,
            _collision: &ManagedCollision,
        ) -> Result<ManagedCollisionChoice> {
            Ok(self.choice)
        }
    }

    fn sync_in_dir_with_collision_choice(
        cwd: &Path,
        cache_root: &Path,
        choice: ManagedCollisionChoice,
    ) -> Result<SyncSummary> {
        let reporter = Reporter::silent();
        let mut resolver = StubManagedCollisionResolver { choice };
        super::sync_in_dir_with_adapters_mode_and_collision_resolution(
            cwd,
            cache_root,
            SyncMode::Normal,
            false,
            &Adapter::ALL,
            false,
            ExecutionMode::Apply,
            None,
            Some(&mut resolver),
            &reporter,
        )
    }

    fn doctor_in_dir(cwd: &Path, cache_root: &Path) -> Result<DoctorSummary> {
        let reporter = Reporter::silent();
        super::doctor_in_dir(cwd, cache_root, &reporter)
    }

    fn resolve_project_from_existing_lockfile_in_dir(
        cwd: &Path,
        cache_root: &Path,
        adapters: &[Adapter],
    ) -> Result<(Resolution, Lockfile)> {
        let reporter = Reporter::silent();
        super::resolve_project_from_existing_lockfile_in_dir(
            cwd,
            cache_root,
            Adapters::from_slice(adapters),
            &reporter,
        )
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
            AddDependencyOptions {
                git_ref: tag.map(RequestedGitRef::Tag),
                version_req: None,
                kind: DependencyKind::Dependency,
                adapters,
                components,
                sync_on_launch: false,
            },
            &reporter,
        )
    }

    fn add_dependency_in_dir_with_git_ref(
        project_root: &Path,
        cache_root: &Path,
        url: &str,
        git_ref: RequestedGitRef<'_>,
        adapters: &[Adapter],
        components: &[DependencyComponent],
    ) -> Result<AddSummary> {
        let reporter = Reporter::silent();
        add_dependency_in_dir_with_adapters_impl(
            project_root,
            cache_root,
            url,
            AddDependencyOptions {
                git_ref: Some(git_ref),
                version_req: None,
                kind: DependencyKind::Dependency,
                adapters,
                components,
                sync_on_launch: false,
            },
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

    fn sync_all_result(project_root: &Path, cache_root: &Path) -> Result<SyncSummary> {
        sync_in_dir_with_adapters(project_root, cache_root, false, false, &Adapter::ALL)
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
        display_path(path)
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
                .contains(&".claude/skills/checks".into())
        );
    }

    #[test]
    fn resolves_local_path_dependencies_with_configured_content_roots() {
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
        write_manifest(
            &temp.path().join("vendor/shared"),
            r#"
content_roots = ["nodus-development"]
"#,
        );
        write_skill(
            &temp
                .path()
                .join("vendor/shared/nodus-development/skills/checks"),
            "Checks",
        );

        sync_all(temp.path(), cache.path());

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let dependency = resolution
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(dependency, "checks");

        assert!(
            temp.path()
                .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".cursor/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".opencode/skills/{managed_skill_id}/SKILL.md"))
                .exists()
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
        let dependency_package = lockfile
            .packages
            .iter()
            .find(|package| package.alias != "root")
            .unwrap();
        assert_eq!(dependency_package.version_tag.as_deref(), Some("v0.1.0"));

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
        assert!(manifest.contains("branch = \"main\""));
    }

    #[test]
    fn add_dependency_tracks_an_explicit_branch() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        init_git_repo(repo.path());
        rename_current_branch(repo.path(), "main");
        tag_repo(repo.path(), "v0.1.0");

        add_dependency_in_dir_with_git_ref(
            temp.path(),
            cache.path(),
            &repo.path().to_string_lossy(),
            RequestedGitRef::Branch("main"),
            &Adapter::ALL,
            &[],
        )
        .unwrap();

        let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
        assert!(manifest.contains("branch = \"main\""));
        assert!(!manifest.contains("tag = "));
    }

    #[test]
    fn add_dependency_pins_an_explicit_revision() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        init_git_repo(repo.path());
        tag_repo(repo.path(), "v0.1.0");
        let revision = crate::git::current_rev(repo.path()).unwrap();

        add_dependency_in_dir_with_git_ref(
            temp.path(),
            cache.path(),
            &repo.path().to_string_lossy(),
            RequestedGitRef::Revision(revision.as_str()),
            &Adapter::ALL,
            &[],
        )
        .unwrap();

        let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
        assert!(manifest.contains(&format!("revision = \"{revision}\"")));
        assert!(!manifest.contains("tag = "));
        assert!(!manifest.contains("branch = "));
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
      "version": "2.34.0",
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
        assert_eq!(wrapper_package.version_tag.as_deref(), Some("2.34.0"));
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
                .join(format!(".agents/skills/{managed_skill_id}/SKILL.md"))
                .exists()
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
        assert!(
            temp.path()
                .join(format!(".claude/commands/{managed_command_file}"))
                .exists()
        );
    }

    #[test]
    fn add_dependency_writes_marketplace_version_alongside_default_branch() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();

        let wrapper = TempDir::new().unwrap();
        write_marketplace(
            wrapper.path(),
            r#"{
  "plugins": [
    {
      "name": "Axiom",
      "version": "2.34.0",
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
        write_claude_plugin_json(
            &wrapper.path().join(".claude-plugin/plugins/axiom"),
            "2.34.0",
        );
        init_git_repo(wrapper.path());
        rename_current_branch(wrapper.path(), "main");

        add_dependency_in_dir_with_adapters(
            temp.path(),
            cache.path(),
            &wrapper.path().to_string_lossy(),
            None,
            &Adapter::ALL,
            &[],
        )
        .unwrap();

        let manifest = load_root_from_dir(temp.path()).unwrap();
        let dependency = manifest.manifest.dependencies.values().next().unwrap();
        assert_eq!(dependency.tag, None);
        assert_eq!(dependency.branch.as_deref(), Some("main"));
        assert!(dependency.version.is_none());
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
    fn root_resolution_includes_dev_dependencies() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[dev-dependencies]
tooling = { path = "vendor/tooling" }
"#,
        );
        write_skill(
            &temp.path().join("vendor/tooling/skills/tooling"),
            "Tooling",
        );

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();

        assert!(
            resolution
                .packages
                .iter()
                .any(|package| package.alias == "tooling")
        );
        let lockfile = resolution
            .to_lockfile(Adapters::from_slice(&Adapter::ALL))
            .unwrap();
        let root_package = lockfile
            .packages
            .iter()
            .find(|package| package.alias == "root")
            .unwrap();
        assert_eq!(root_package.dependencies, vec!["tooling"]);
    }

    #[test]
    fn consumed_packages_do_not_export_dev_dependencies() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[dependencies]
wrapper = { path = "vendor/wrapper" }
"#,
        );
        write_file(
            &temp.path().join("vendor/wrapper/nodus.toml"),
            r#"
[dependencies]
shared = { path = "vendor/shared" }

[dev-dependencies]
tooling = { path = "vendor/tooling" }
"#,
        );
        write_skill(
            &temp
                .path()
                .join("vendor/wrapper/vendor/shared/skills/shared"),
            "Shared",
        );
        write_skill(
            &temp
                .path()
                .join("vendor/wrapper/vendor/tooling/skills/tooling"),
            "Tooling",
        );

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();

        assert!(
            resolution
                .packages
                .iter()
                .any(|package| package.alias == "shared")
        );
        assert!(
            !resolution
                .packages
                .iter()
                .any(|package| package.alias == "tooling")
        );
        let lockfile = resolution
            .to_lockfile(Adapters::from_slice(&Adapter::ALL))
            .unwrap();
        let wrapper_package = lockfile
            .packages
            .iter()
            .find(|package| package.alias == "wrapper")
            .unwrap();
        assert_eq!(wrapper_package.dependencies, vec!["shared"]);
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
        let managed_copilot_agent_file = namespaced_file_name(dependency, "shared", "agent.md");
        let managed_command_file = namespaced_file_name(dependency, "build", "md");
        let managed_claude_rule_file = namespaced_file_name(dependency, "default", "md");
        let managed_cursor_rule_file = namespaced_file_name(dependency, "default", "mdc");

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
        assert!(
            temp.path()
                .join(format!(".claude/rules/{managed_claude_rule_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".codex/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".github/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".github/agents/{managed_copilot_agent_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".agents/commands/{managed_command_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".cursor/commands/{managed_command_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".cursor/rules/{managed_cursor_rule_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".cursor/skills/{managed_skill_id}/SKILL.md"))
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
                    .join(format!(".github/skills/{managed_skill_id}/SKILL.md"))
            )
            .unwrap()
            .contains(&format!("name: {managed_skill_id}"))
        );
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
    fn sync_filters_github_copilot_outputs_by_selected_components() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[adapters]
enabled = ["copilot"]

[dependencies]
shared = { path = "vendor/shared", components = ["skills"] }
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
        let managed_agent_file = namespaced_file_name(dependency, "shared", "agent.md");

        assert!(
            temp.path()
                .join(format!(".github/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            !temp
                .path()
                .join(format!(".github/agents/{managed_agent_file}"))
                .exists()
        );
    }

    #[test]
    fn sync_rewrites_github_copilot_skill_name_to_managed_id() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[adapters]
enabled = ["copilot"]

[dependencies]
shared = { path = "vendor/shared" }
"#,
        );
        write_file(
            &temp.path().join("vendor/shared/skills/review/SKILL.md"),
            "---\nname: shared-review\ndescription: Example review skill.\n---\n# Review\n",
        );

        sync_all(temp.path(), cache.path());

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let dependency = resolution
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(dependency, "review");

        assert!(
            fs::read_to_string(
                temp.path()
                    .join(format!(".github/skills/{managed_skill_id}/SKILL.md"))
            )
            .unwrap()
            .contains(&format!("name: {managed_skill_id}"))
        );
        assert!(!temp.path().join(".github/.gitignore").exists());
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
            temp.path()
                .join(format!(".codex/skills/{managed_skill_id}/SKILL.md"))
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
    fn sync_does_not_publish_root_assets_by_default() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");
        write_file(&temp.path().join("rules/default.rules"), "allow = []\n");

        sync_all(temp.path(), cache.path());

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let root_package = resolution
            .packages
            .iter()
            .find(|package| matches!(package.source, PackageSource::Root))
            .unwrap();
        let managed_skill_id = namespaced_skill_id(root_package, "review");
        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();

        assert!(
            !temp
                .path()
                .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            !temp
                .path()
                .join(format!(".codex/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            !lockfile
                .managed_files
                .contains(&".claude/skills/review".into())
        );
        assert!(
            !lockfile
                .managed_files
                .contains(&".codex/skills/review".into())
        );
    }

    #[test]
    fn sync_publishes_root_assets_when_enabled() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
publish_root = true
"#,
        );
        write_skill(&temp.path().join("skills/review"), "Review");
        write_file(&temp.path().join("rules/default.rules"), "allow = []\n");

        sync_all(temp.path(), cache.path());

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let root_package = resolution
            .packages
            .iter()
            .find(|package| matches!(package.source, PackageSource::Root))
            .unwrap();
        let managed_skill_id = namespaced_skill_id(root_package, "review");
        let managed_claude_rule_file = namespaced_file_name(root_package, "default", "md");
        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();

        assert!(
            temp.path()
                .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".cursor/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".opencode/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".claude/rules/{managed_claude_rule_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".codex/skills/{managed_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            lockfile
                .managed_files
                .contains(&".claude/skills/review".into())
        );
        assert!(
            lockfile
                .managed_files
                .contains(&".codex/skills/review".into())
        );
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
        let managed_command_file = namespaced_file_name(dependency, "build", "md");
        let codex_gitignore = fs::read_to_string(temp.path().join(".codex/.gitignore")).unwrap();
        let agents_gitignore = fs::read_to_string(temp.path().join(".agents/.gitignore")).unwrap();
        let cursor_gitignore = fs::read_to_string(temp.path().join(".cursor/.gitignore")).unwrap();

        assert!(codex_gitignore.contains("# Managed by nodus"));
        assert!(codex_gitignore.contains(".gitignore"));
        let (_, suffix) = managed_skill_id.rsplit_once('_').unwrap();
        assert!(codex_gitignore.contains(&format!("skills/*_{suffix}/")));
        let (_, command_suffix) = managed_command_file
            .trim_end_matches(".md")
            .rsplit_once('_')
            .unwrap();
        assert!(agents_gitignore.contains("# Managed by nodus"));
        assert!(agents_gitignore.contains(".gitignore"));
        assert!(agents_gitignore.contains(&format!("skills/*_{suffix}/")));
        assert!(agents_gitignore.contains(&format!("commands/*_{command_suffix}.md")));
        assert!(cursor_gitignore.contains("# Managed by nodus"));
        assert!(cursor_gitignore.contains(".gitignore"));
        assert!(cursor_gitignore.contains(&format!("skills/*_{suffix}/")));
        assert!(cursor_gitignore.contains(&format!("commands/*_{command_suffix}.md")));
        assert!(cursor_gitignore.contains(&format!("rules/*_{suffix}.mdc")));
    }

    #[test]
    fn sync_merges_direct_managed_runtime_root_gitignore_with_generated_outputs() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "config/.gitignore"
target = ".claude/.gitignore"
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
        write_file(
            &temp.path().join("vendor/shared/config/.gitignore"),
            ".DS_Store\n",
        );

        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Claude])
            .unwrap();
        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Claude])
            .unwrap();

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let dependency = resolution
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(dependency, "review");
        let (_, suffix) = managed_skill_id.rsplit_once('_').unwrap();
        let gitignore = fs::read_to_string(temp.path().join(".claude/.gitignore")).unwrap();
        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();

        assert!(gitignore.contains("# Managed by nodus"));
        assert!(gitignore.contains(".gitignore"));
        assert!(gitignore.contains(".DS_Store"));
        assert!(gitignore.contains(&format!("skills/*_{suffix}/")));
        assert!(
            lockfile
                .managed_files
                .contains(&".claude/.gitignore".into())
        );
    }

    #[test]
    fn sync_emits_mcp_json_from_dependency_manifests() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies.firebase]
path = "vendor/firebase"
"#,
        );
        write_file(
            &temp.path().join("vendor/firebase/nodus.toml"),
            r#"
[mcp_servers.firebase]
command = "npx"
args = ["-y", "firebase-tools", "mcp", "--dir", "."]

[mcp_servers.firebase.env]
IS_FIREBASE_MCP = "true"
"#,
        );

        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex])
            .unwrap();

        let mcp_config = fs::read_to_string(temp.path().join(".mcp.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&mcp_config).unwrap();
        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
        let firebase_package = lockfile
            .packages
            .iter()
            .find(|package| package.alias == "firebase")
            .unwrap();

        assert_eq!(firebase_package.mcp_servers, vec!["firebase"]);
        assert!(lockfile.managed_files.contains(&String::from(".mcp.json")));
        assert_eq!(
            json["mcpServers"]["firebase__firebase"]["command"].as_str(),
            Some("npx")
        );
        assert_eq!(
            json["mcpServers"]["firebase__firebase"]["env"]["IS_FIREBASE_MCP"].as_str(),
            Some("true")
        );
    }

    #[test]
    fn sync_emits_url_backed_mcp_servers() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies.figma]
path = "vendor/figma"
"#,
        );
        write_file(
            &temp.path().join("vendor/figma/nodus.toml"),
            r#"
[mcp_servers.figma]
url = "http://127.0.0.1:3845/mcp"
"#,
        );

        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex])
            .unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(temp.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            json["mcpServers"]["figma__figma"]["url"].as_str(),
            Some("http://127.0.0.1:3845/mcp")
        );
        assert!(json["mcpServers"]["figma__figma"]["command"].is_null());
    }

    #[test]
    fn sync_omits_disabled_mcp_servers() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies.xcode]
path = "vendor/xcode"
"#,
        );
        write_file(
            &temp.path().join("vendor/xcode/nodus.toml"),
            r#"
[mcp_servers.xcode]
command = "xcrun"
args = ["mcpbridge"]
enabled = false
"#,
        );

        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex])
            .unwrap();

        assert!(!temp.path().join(".mcp.json").exists());
        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
        let xcode_package = lockfile
            .packages
            .iter()
            .find(|package| package.alias == "xcode")
            .unwrap();
        assert_eq!(xcode_package.mcp_servers, vec!["xcode"]);
        assert!(!lockfile.managed_files.contains(&String::from(".mcp.json")));
    }

    #[test]
    fn sync_merges_unmanaged_mcp_entries_with_managed_outputs() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies.firebase]
path = "vendor/firebase"
"#,
        );
        write_file(
            &temp.path().join("vendor/firebase/nodus.toml"),
            r#"
[mcp_servers.firebase]
command = "npx"
"#,
        );
        write_file(
            &temp.path().join(".mcp.json"),
            r#"{
  "mcpServers": {
    "local": {
      "command": "node"
    }
  }
}
"#,
        );

        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex])
            .unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(temp.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            json["mcpServers"]["local"]["command"].as_str(),
            Some("node")
        );
        assert_eq!(
            json["mcpServers"]["firebase__firebase"]["command"].as_str(),
            Some("npx")
        );
    }

    #[test]
    fn sync_prunes_stale_managed_mcp_entries_without_touching_unmanaged_ones() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies.firebase]
path = "vendor/firebase"
"#,
        );
        write_file(
            &temp.path().join("vendor/firebase/nodus.toml"),
            r#"
[mcp_servers.firebase]
command = "npx"
"#,
        );

        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex])
            .unwrap();
        write_file(
            &temp.path().join(".mcp.json"),
            r#"{
  "mcpServers": {
    "firebase__firebase": {
      "command": "npx"
    },
    "local": {
      "command": "node"
    }
  }
}
"#,
        );
        write_manifest(temp.path(), "");

        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex])
            .unwrap();

        let mcp_path = temp.path().join(".mcp.json");
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&mcp_path).unwrap()).unwrap();
        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();

        assert!(json["mcpServers"].get("firebase__firebase").is_none());
        assert_eq!(
            json["mcpServers"]["local"]["command"].as_str(),
            Some("node")
        );
        assert!(!lockfile.managed_files.contains(&String::from(".mcp.json")));
    }

    #[test]
    fn doctor_rejects_invalid_managed_mcp_json() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies.firebase]
path = "vendor/firebase"
"#,
        );
        write_file(
            &temp.path().join("vendor/firebase/nodus.toml"),
            r#"
[mcp_servers.firebase]
command = "npx"
"#,
        );

        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex])
            .unwrap();
        write_file(&temp.path().join(".mcp.json"), "{");

        let error = doctor_in_dir(temp.path(), cache.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("failed to parse MCP config"));
    }

    #[test]
    fn sync_recreates_missing_lockfile_for_existing_runtime_outputs() {
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

        sync_all(temp.path(), cache.path());
        fs::remove_file(temp.path().join(LOCKFILE_NAME)).unwrap();

        sync_all(temp.path(), cache.path());

        assert!(temp.path().join(LOCKFILE_NAME).exists());
    }

    #[test]
    fn sync_upgrades_legacy_lockfile_and_prunes_legacy_runtime_outputs() {
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
            &temp.path().join("vendor/shared/commands/build.txt"),
            "cargo test\n",
        );

        sync_in_dir_with_adapters(
            temp.path(),
            cache.path(),
            false,
            false,
            &[Adapter::Claude, Adapter::OpenCode],
        )
        .unwrap();

        let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let dependency = resolution
            .packages
            .iter()
            .find(|package| package.alias == "shared")
            .unwrap();
        let managed_skill_id = namespaced_skill_id(dependency, "review");
        let managed_agent_file = namespaced_file_name(dependency, "security", "md");
        let managed_command_file = namespaced_file_name(dependency, "build", "md");

        fs::rename(
            temp.path()
                .join(format!(".claude/agents/{managed_agent_file}")),
            temp.path().join(".claude/agents/security.md"),
        )
        .unwrap();
        fs::rename(
            temp.path()
                .join(format!(".claude/commands/{managed_command_file}")),
            temp.path().join(".claude/commands/build.md"),
        )
        .unwrap();
        fs::rename(
            temp.path()
                .join(format!(".opencode/agents/{managed_agent_file}")),
            temp.path().join(".opencode/agents/security.md"),
        )
        .unwrap();
        fs::rename(
            temp.path()
                .join(format!(".opencode/commands/{managed_command_file}")),
            temp.path().join(".opencode/commands/build.md"),
        )
        .unwrap();
        fs::rename(
            temp.path()
                .join(format!(".opencode/skills/{managed_skill_id}")),
            temp.path().join(".opencode/skills/review"),
        )
        .unwrap();

        let current_lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
        Lockfile {
            version: 4,
            packages: current_lockfile.packages,
            managed_files: current_lockfile.managed_files,
        }
        .write(&temp.path().join(LOCKFILE_NAME))
        .unwrap();

        sync_in_dir_with_adapters(
            temp.path(),
            cache.path(),
            false,
            false,
            &[Adapter::Claude, Adapter::OpenCode],
        )
        .unwrap();

        let upgraded_lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();

        assert_eq!(upgraded_lockfile.version, Lockfile::current_version());
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
                .join(format!(".opencode/skills/{managed_skill_id}"))
                .exists()
        );
        assert!(!temp.path().join(".claude/agents/security.md").exists());
        assert!(!temp.path().join(".claude/commands/build.md").exists());
        assert!(!temp.path().join(".opencode/agents/security.md").exists());
        assert!(!temp.path().join(".opencode/commands/build.md").exists());
        assert!(!temp.path().join(".opencode/skills/review").exists());
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
    fn sync_persists_launch_hook_configuration() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");

        let reporter = Reporter::silent();
        super::sync_in_dir_with_adapters(
            temp.path(),
            cache.path(),
            false,
            false,
            &[Adapter::Codex],
            true,
            &reporter,
        )
        .unwrap();

        let manifest = load_root_from_dir(temp.path()).unwrap();
        assert!(manifest.manifest.sync_on_launch_enabled());
    }

    #[test]
    fn sync_emits_startup_sync_files_for_supported_adapters() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[adapters]
enabled = ["claude", "opencode"]

[launch_hooks]
sync_on_startup = true
"#,
        );

        sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

        assert!(temp.path().join(".claude/hooks/nodus-sync.sh").exists());
        assert!(temp.path().join(".claude/settings.local.json").exists());
        assert!(temp.path().join(".opencode/plugins/nodus-sync.js").exists());
        assert!(temp.path().join(".opencode/scripts/nodus-sync.sh").exists());

        let claude_settings =
            fs::read_to_string(temp.path().join(".claude/settings.local.json")).unwrap();
        let opencode_plugin =
            fs::read_to_string(temp.path().join(".opencode/plugins/nodus-sync.js")).unwrap();

        assert!(claude_settings.contains("\"SessionStart\""));
        assert!(claude_settings.contains("\"startup\""));
        assert!(opencode_plugin.contains(".opencode/scripts/nodus-sync.sh"));
    }

    #[test]
    fn sync_warns_when_launch_hooks_are_unsupported_for_selected_adapters() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[adapters]
enabled = ["agents", "codex", "cursor"]

[launch_hooks]
sync_on_startup = true
"#,
        );

        let buffer = SharedBuffer::default();
        let reporter = Reporter::sink(ColorMode::Never, buffer.clone());
        super::sync_in_dir_with_adapters(
            temp.path(),
            cache.path(),
            false,
            false,
            &[],
            false,
            &reporter,
        )
        .unwrap();

        let output = buffer.contents();
        assert!(output.contains("launch sync is not emitted for `agents`"));
        assert!(output.contains("launch sync is not emitted for `codex`"));
        assert!(output.contains("launch sync is not emitted for `cursor`"));
    }

    #[test]
    fn sync_rejects_launch_hook_persistence_with_locked_flag() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");
        fs::create_dir_all(temp.path().join(".codex")).unwrap();
        sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

        let reporter = Reporter::silent();
        let error = super::sync_in_dir_with_adapters(
            temp.path(),
            cache.path(),
            true,
            false,
            &[],
            true,
            &reporter,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("launch hook configuration"));
    }

    #[test]
    fn sync_frozen_requires_existing_lockfile() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[adapters]
enabled = ["claude"]
"#,
        );

        let error = sync_in_dir_frozen(temp.path(), cache.path(), false)
            .unwrap_err()
            .to_string();

        assert!(error.contains("`--frozen` requires an existing nodus.lock"));
    }

    #[test]
    fn sync_frozen_installs_branch_dependencies_from_locked_revision() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();

        let repo = TempDir::new().unwrap();
        write_file(
            &repo.path().join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: First revision.\n---\n# Review\nfirst\n",
        );
        init_git_repo(repo.path());
        rename_current_branch(repo.path(), "main");

        write_manifest(
            temp.path(),
            &format!(
                r#"
[adapters]
enabled = ["claude"]

[dependencies]
review_pkg = {{ url = "{}", branch = "main" }}
"#,
                toml_path_value(repo.path())
            ),
        );

        sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

        let initial_lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
        let initial_rev = initial_lockfile
            .packages
            .iter()
            .find(|package| package.alias == "review_pkg")
            .and_then(|package| package.source.rev.clone())
            .unwrap();
        let initial_resolution =
            resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let initial_dependency = initial_resolution
            .packages
            .iter()
            .find(|package| package.alias == "review_pkg")
            .unwrap();
        let initial_skill_id = namespaced_skill_id(initial_dependency, "review");
        let initial_skill_path = temp
            .path()
            .join(format!(".claude/skills/{initial_skill_id}/SKILL.md"));
        assert!(initial_skill_path.exists());
        assert!(
            fs::read_to_string(&initial_skill_path)
                .unwrap()
                .contains("first")
        );

        write_file(
            &repo.path().join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Second revision.\n---\n# Review\nsecond\n",
        );
        commit_all(repo.path(), "advance");

        sync_in_dir_frozen(temp.path(), cache.path(), false).unwrap();

        let frozen_lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
        let frozen_rev = frozen_lockfile
            .packages
            .iter()
            .find(|package| package.alias == "review_pkg")
            .and_then(|package| package.source.rev.clone())
            .unwrap();
        assert_eq!(frozen_rev, initial_rev);
        assert!(initial_skill_path.exists());
        assert!(
            fs::read_to_string(&initial_skill_path)
                .unwrap()
                .contains("first")
        );

        sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

        let updated_lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
        let updated_rev = updated_lockfile
            .packages
            .iter()
            .find(|package| package.alias == "review_pkg")
            .and_then(|package| package.source.rev.clone())
            .unwrap();
        assert_ne!(updated_rev, initial_rev);

        let updated_resolution =
            resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
        let updated_dependency = updated_resolution
            .packages
            .iter()
            .find(|package| package.alias == "review_pkg")
            .unwrap();
        let updated_skill_id = namespaced_skill_id(updated_dependency, "review");
        let updated_skill_path = temp
            .path()
            .join(format!(".claude/skills/{updated_skill_id}/SKILL.md"));
        assert_ne!(updated_skill_id, initial_skill_id);
        assert!(!initial_skill_path.exists());
        assert!(updated_skill_path.exists());
        assert!(
            fs::read_to_string(&updated_skill_path)
                .unwrap()
                .contains("second")
        );
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
        assert!(temp.path().join(".opencode/skills").exists());

        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Claude])
            .unwrap();

        let manifest = load_root_from_dir(temp.path()).unwrap();
        assert_eq!(
            manifest.manifest.enabled_adapters().unwrap(),
            [Adapter::Claude].as_slice()
        );
        assert!(temp.path().join(".claude/skills").exists());
        assert!(temp.path().join(".claude/.gitignore").exists());
        assert!(!temp.path().join(".codex/skills").exists());
        assert!(!temp.path().join(".codex/.gitignore").exists());
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
                .contains(&".github/skills/iframe-ad".into())
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
    fn sync_writes_direct_managed_file_targets() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
        write_file(
            &temp.path().join("vendor/shared/prompts/review.md"),
            "Use the review prompt.\n",
        );

        sync_all(temp.path(), cache.path());

        assert_eq!(
            fs::read_to_string(temp.path().join(".github/prompts/review.md")).unwrap(),
            "Use the review prompt.\n"
        );

        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
        assert!(
            lockfile
                .managed_files
                .contains(&".github/prompts/review.md".into())
        );
    }

    #[test]
    fn sync_writes_and_prunes_direct_managed_directory_targets() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "templates"
target = "docs/templates"
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
        write_file(
            &temp.path().join("vendor/shared/templates/review.md"),
            "review template\n",
        );
        write_file(
            &temp.path().join("vendor/shared/templates/nested/tips.md"),
            "tips\n",
        );
        write_file(&temp.path().join("docs/templates/user.md"), "keep me\n");

        sync_all(temp.path(), cache.path());

        assert_eq!(
            fs::read_to_string(temp.path().join("docs/templates/review.md")).unwrap(),
            "review template\n"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("docs/templates/nested/tips.md")).unwrap(),
            "tips\n"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("docs/templates/user.md")).unwrap(),
            "keep me\n"
        );

        fs::remove_file(temp.path().join("vendor/shared/templates/nested/tips.md")).unwrap();
        sync_all(temp.path(), cache.path());

        assert!(temp.path().join("docs/templates/review.md").exists());
        assert!(!temp.path().join("docs/templates/nested/tips.md").exists());
        assert_eq!(
            fs::read_to_string(temp.path().join("docs/templates/user.md")).unwrap(),
            "keep me\n"
        );
    }

    #[test]
    fn sync_prunes_direct_managed_targets_when_mapping_is_removed() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
        write_file(
            &temp.path().join("vendor/shared/prompts/review.md"),
            "Use the review prompt.\n",
        );
        write_manifest(
            temp.path(),
            r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
        );

        sync_all(temp.path(), cache.path());
        assert!(temp.path().join(".github/prompts/review.md").exists());

        write_manifest(
            temp.path(),
            r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
        );
        sync_all(temp.path(), cache.path());

        assert!(!temp.path().join(".github/prompts/review.md").exists());
    }

    #[test]
    fn sync_rejects_unmanaged_collision_on_direct_managed_target() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
        write_file(
            &temp.path().join("vendor/shared/prompts/review.md"),
            "Use the review prompt.\n",
        );
        write_file(
            &temp.path().join(".github/prompts/review.md"),
            "user-owned prompt\n",
        );

        let error = sync_all_result(temp.path(), cache.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("refusing to overwrite unmanaged file"));
        assert!(error.contains(".github/prompts/review.md"));
        assert!(error.contains("remove the managed mapping from `nodus.toml`"));
    }

    #[test]
    fn sync_can_adopt_unmanaged_collision_on_direct_managed_target() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
        write_file(
            &temp.path().join("vendor/shared/prompts/review.md"),
            "Use the review prompt.\n",
        );
        write_file(
            &temp.path().join(".github/prompts/review.md"),
            "user-owned prompt\n",
        );

        sync_in_dir_with_collision_choice(temp.path(), cache.path(), ManagedCollisionChoice::Adopt)
            .unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join(".github/prompts/review.md")).unwrap(),
            "Use the review prompt.\n"
        );
        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
        assert!(
            lockfile
                .managed_files
                .contains(&".github/prompts/review.md".into())
        );
    }

    #[test]
    fn sync_can_remove_managed_mapping_after_unmanaged_collision() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
        write_file(
            &temp.path().join("vendor/shared/prompts/review.md"),
            "Use the review prompt.\n",
        );
        write_file(
            &temp.path().join(".github/prompts/review.md"),
            "user-owned prompt\n",
        );

        sync_in_dir_with_collision_choice(
            temp.path(),
            cache.path(),
            ManagedCollisionChoice::RemoveMapping,
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join(".github/prompts/review.md")).unwrap(),
            "user-owned prompt\n"
        );
        let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
        assert!(!manifest.contains("[[dependencies.shared.managed]]"));
        let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
        assert!(
            !lockfile
                .managed_files
                .contains(&".github/prompts/review.md".into())
        );
    }

    #[test]
    fn sync_can_cancel_after_unmanaged_collision_prompt() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
        write_file(
            &temp.path().join("vendor/shared/prompts/review.md"),
            "Use the review prompt.\n",
        );
        write_file(
            &temp.path().join(".github/prompts/review.md"),
            "user-owned prompt\n",
        );

        let error = sync_in_dir_with_collision_choice(
            temp.path(),
            cache.path(),
            ManagedCollisionChoice::Cancel,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("cancelled `nodus sync`"));
        assert!(error.contains(".github/prompts/review.md"));
    }

    #[test]
    fn sync_rejects_overlapping_direct_managed_targets() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts"
target = "docs/prompts"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = "docs/prompts/review.md"
"#,
        );
        write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
        write_file(
            &temp.path().join("vendor/shared/prompts/review.md"),
            "Use the review prompt.\n",
        );

        let error = sync_all_result(temp.path(), cache.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("overlapping target roots"));
    }

    #[test]
    fn sync_rejects_nested_dependency_managed_paths() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_manifest(
            temp.path(),
            r#"
[dependencies]
wrapper = { path = "vendor/wrapper" }
"#,
        );
        write_file(
            &temp.path().join("vendor/wrapper/nodus.toml"),
            r#"
[dependencies.leaf]
path = "vendor/leaf"

[[dependencies.leaf.managed]]
source = "prompts/review.md"
target = "docs/review.md"
"#,
        );
        write_skill(
            &temp.path().join("vendor/wrapper/skills/wrapper"),
            "Wrapper",
        );
        write_skill(
            &temp.path().join("vendor/wrapper/vendor/leaf/skills/leaf"),
            "Leaf",
        );
        write_file(
            &temp
                .path()
                .join("vendor/wrapper/vendor/leaf/prompts/review.md"),
            "Use the review prompt.\n",
        );

        let error = sync_all_result(temp.path(), cache.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("supported only for direct dependencies in the root manifest"));
    }

    #[test]
    fn sync_frozen_keeps_direct_managed_files_at_locked_git_revision() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        write_file(&repo.path().join("prompts/review.md"), "first revision\n");
        init_git_repo(repo.path());
        rename_current_branch(repo.path(), "main");

        write_manifest(
            temp.path(),
            &format!(
                r#"
[adapters]
enabled = ["codex"]

[dependencies.review_pkg]
url = "{}"
branch = "main"

[[dependencies.review_pkg.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
                toml_path_value(repo.path())
            ),
        );

        sync_in_dir(temp.path(), cache.path(), false, false).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join(".github/prompts/review.md")).unwrap(),
            "first revision\n"
        );

        write_file(&repo.path().join("prompts/review.md"), "second revision\n");
        commit_all(repo.path(), "advance");

        sync_in_dir_frozen(temp.path(), cache.path(), false).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join(".github/prompts/review.md")).unwrap(),
            "first revision\n"
        );

        sync_in_dir(temp.path(), cache.path(), false, false).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join(".github/prompts/review.md")).unwrap(),
            "second revision\n"
        );
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
        assert!(error.contains("run `nodus sync`"));
        assert!(error.contains("run `nodus doctor`"));
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
    fn recover_runtime_owned_paths_includes_copilot_assets_only() {
        let project_root = Path::new("/tmp/project");
        let desired_paths = [
            project_root.join(".claude/skills/review_abc123"),
            project_root.join(".github/skills/review_abc123"),
            project_root.join(".github/agents/security_abc123.agent.md"),
            project_root.join(".github/prompts/review.md"),
        ]
        .into_iter()
        .collect::<HashSet<_>>();

        let recovered = recover_runtime_owned_paths(project_root, &desired_paths);

        assert!(recovered.contains(&project_root.join(".claude/skills/review_abc123")));
        assert!(recovered.contains(&project_root.join(".github/skills/review_abc123")));
        assert!(recovered.contains(&project_root.join(".github/agents/security_abc123.agent.md")));
        assert!(!recovered.contains(&project_root.join(".github/prompts/review.md")));
    }

    #[test]
    fn prune_empty_parent_dirs_stops_at_github_root() {
        let temp = TempDir::new().unwrap();
        let skill_dir = temp.path().join(".github/skills/review_abc123");
        let skill_file = skill_dir.join("SKILL.md");
        write_file(&skill_file, "# Review\n");

        fs::remove_file(&skill_file).unwrap();
        prune_empty_parent_dirs(&skill_file, temp.path()).unwrap();

        assert!(temp.path().join(".github").exists());
        assert!(!temp.path().join(".github/skills").exists());
        assert!(!skill_dir.exists());
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
                .join(format!(".github/skills/{shared_skill_id}/SKILL.md"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".github/skills/{other_skill_id}/SKILL.md"))
                .exists()
        );
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
        let shared_copilot_agent_file = namespaced_file_name(shared, "security", "agent.md");
        let other_copilot_agent_file = namespaced_file_name(other, "security", "agent.md");
        let shared_command_file = namespaced_file_name(shared, "build", "md");
        let other_command_file = namespaced_file_name(other, "build", "md");
        let shared_claude_rule_file = namespaced_file_name(shared, "default", "md");
        let other_claude_rule_file = namespaced_file_name(other, "default", "md");

        assert_ne!(shared_agent_file, other_agent_file);
        assert_ne!(shared_copilot_agent_file, other_copilot_agent_file);
        assert_ne!(shared_command_file, other_command_file);
        assert_ne!(shared_claude_rule_file, other_claude_rule_file);

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
                .join(format!(".github/agents/{shared_copilot_agent_file}"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!(".github/agents/{other_copilot_agent_file}"))
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
        assert!(error.contains("run `nodus sync`"));
        assert!(error.contains("run `nodus doctor`"));
    }

    #[test]
    fn existing_lockfile_resolution_accepts_lockfile_drift_for_baseline_checks() {
        let temp = TempDir::new().unwrap();
        let cache = cache_dir();
        write_skill(&temp.path().join("skills/review"), "Review");
        sync_all(temp.path(), cache.path());

        write_skill(&temp.path().join("skills/renamed"), "Renamed");

        let (resolution, lockfile) =
            resolve_project_from_existing_lockfile_in_dir(temp.path(), cache.path(), &Adapter::ALL)
                .unwrap();
        assert!(!resolution.packages.is_empty());
        assert!(!lockfile.packages.is_empty());
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
        let output_plan =
            build_output_plan(temp.path(), &package_roots, Adapters::CODEX, None, false).unwrap();
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
