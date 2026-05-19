mod doctor;
mod install_digest;
mod resolve;
mod support;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

pub use self::doctor::{
    DoctorActionRecord, DoctorFinding, DoctorFindingKind, DoctorMode, DoctorStatus, DoctorSummary,
    doctor_in_dir_with_mode,
};
use self::install_digest::install_digest_from_disk;
use self::resolve::{ResolveProjectOptions, resolve_project};
use self::support::{
    build_sync_execution_plan, enforce_capabilities, execute_sync_plan, find_managed_collision,
    find_runtime_output_collision, find_unmanaged_collision, load_owned_paths,
    recover_runtime_owned_paths, recover_runtime_owned_paths_from_disk,
    unmanaged_collision_guidance,
};
#[cfg(test)]
use self::support::{prune_empty_parent_dirs, write_managed_files};
use crate::adapters::{
    Adapter, Adapters, ManagedFile, OutputPlan, OutputPlanOptions, PackageOwnedPaths,
    build_output_plan_with_options,
};
use crate::execution::ExecutionMode;
use crate::hashing::content_digest;
use crate::install_paths::{InstallPaths, InstallScope};
use crate::lockfile::{
    LOCKFILE_NAME, LockedPackage, LockedSource, Lockfile, compact_owned_runtime_adapter_ownership,
    locked_runtime_adapter_owned_paths,
};
use crate::manifest::{
    DependencyComponent, LoadedManifest, ManagedPlacement, PackageRole,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DependencyFailureMode {
    Graceful,
    Strict,
}

#[derive(Clone, Copy)]
struct SyncExecutionOptions<'a> {
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &'a [Adapter],
    sync_on_launch: bool,
    execution_mode: ExecutionMode,
    dependency_failure_mode: DependencyFailureMode,
    /// When true, skip the v10 `install_digest` drift fast-path even if all
    /// preconditions hold (lockfile is current schema, all pins are exact,
    /// every package has an `install_digest`). Slice 4 added the fast-path for
    /// the common "nothing changed on disk" case; this flag is the escape
    /// hatch for users who want to force a full re-render.
    force_rebuild: bool,
}

impl<'a> SyncExecutionOptions<'a> {
    fn new(
        allow_high_sensitivity: bool,
        force: bool,
        adapters: &'a [Adapter],
        sync_on_launch: bool,
        execution_mode: ExecutionMode,
        dependency_failure_mode: DependencyFailureMode,
        force_rebuild: bool,
    ) -> Self {
        Self {
            allow_high_sensitivity,
            force,
            adapters,
            sync_on_launch,
            execution_mode,
            dependency_failure_mode,
            force_rebuild,
        }
    }
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
    manifest_write: Option<PlannedFileWrite>,
    removals: Vec<PathBuf>,
    managed_writes: Vec<ManagedFile>,
    external_writes: Vec<ManagedFile>,
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
    RuntimeOutput,
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
    sync_in_dir_with_adapters_full(
        cwd,
        cache_root,
        locked,
        allow_high_sensitivity,
        force,
        adapters,
        sync_on_launch,
        false,
        reporter,
    )
}

/// `sync_in_dir_with_adapters` plus the v10 fast-path opt-out.
///
/// Slice 4 added the `install_digest` drift fast-path that lets `nodus sync`
/// exit early when the lockfile and disk agree. Pass `force_rebuild = true` to
/// skip that check and always run a full resolve + render. The CLI surfaces
/// this as `--no-fast-path`; library callers default to `false` so they keep
/// the speedup.
#[allow(clippy::too_many_arguments)]
pub fn sync_in_dir_with_adapters_full(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    force_rebuild: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_with_failure_mode(
        cwd,
        cache_root,
        locked,
        SyncExecutionOptions::new(
            allow_high_sensitivity,
            force,
            adapters,
            sync_on_launch,
            ExecutionMode::Apply,
            DependencyFailureMode::Graceful,
            force_rebuild,
        ),
        reporter,
    )
}

#[allow(clippy::too_many_arguments, dead_code)]
pub fn sync_in_dir_with_adapters_strict(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_strict_full(
        cwd,
        cache_root,
        locked,
        allow_high_sensitivity,
        force,
        adapters,
        sync_on_launch,
        false,
        reporter,
    )
}

/// `sync_in_dir_with_adapters_strict` plus the v10 fast-path opt-out.
#[allow(clippy::too_many_arguments)]
pub fn sync_in_dir_with_adapters_strict_full(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    force_rebuild: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_with_failure_mode(
        cwd,
        cache_root,
        locked,
        SyncExecutionOptions::new(
            allow_high_sensitivity,
            force,
            adapters,
            sync_on_launch,
            ExecutionMode::Apply,
            DependencyFailureMode::Strict,
            force_rebuild,
        ),
        reporter,
    )
}

fn sync_in_dir_with_adapters_with_failure_mode(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    options: SyncExecutionOptions<'_>,
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
        options.allow_high_sensitivity,
        options.force,
        options.adapters,
        options.sync_on_launch,
        options.execution_mode,
        None,
        options.dependency_failure_mode,
        options.force_rebuild,
        reporter,
    )
}

#[allow(dead_code)]
pub fn sync_in_dir_with_adapters_frozen(
    cwd: &Path,
    cache_root: &Path,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_frozen_full(
        cwd,
        cache_root,
        allow_high_sensitivity,
        force,
        adapters,
        sync_on_launch,
        false,
        reporter,
    )
}

/// `sync_in_dir_with_adapters_frozen` plus the v10 fast-path opt-out.
#[allow(clippy::too_many_arguments)]
pub fn sync_in_dir_with_adapters_frozen_full(
    cwd: &Path,
    cache_root: &Path,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    force_rebuild: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_frozen_with_failure_mode(
        cwd,
        cache_root,
        SyncExecutionOptions::new(
            allow_high_sensitivity,
            force,
            adapters,
            sync_on_launch,
            ExecutionMode::Apply,
            DependencyFailureMode::Graceful,
            force_rebuild,
        ),
        reporter,
    )
}

#[allow(dead_code)]
pub fn sync_in_dir_with_adapters_frozen_strict(
    cwd: &Path,
    cache_root: &Path,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_frozen_strict_full(
        cwd,
        cache_root,
        allow_high_sensitivity,
        force,
        adapters,
        sync_on_launch,
        false,
        reporter,
    )
}

/// `sync_in_dir_with_adapters_frozen_strict` plus the v10 fast-path opt-out.
#[allow(clippy::too_many_arguments)]
pub fn sync_in_dir_with_adapters_frozen_strict_full(
    cwd: &Path,
    cache_root: &Path,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    force_rebuild: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_frozen_with_failure_mode(
        cwd,
        cache_root,
        SyncExecutionOptions::new(
            allow_high_sensitivity,
            force,
            adapters,
            sync_on_launch,
            ExecutionMode::Apply,
            DependencyFailureMode::Strict,
            force_rebuild,
        ),
        reporter,
    )
}

fn sync_in_dir_with_adapters_frozen_with_failure_mode(
    cwd: &Path,
    cache_root: &Path,
    options: SyncExecutionOptions<'_>,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    let install_paths = InstallPaths::project(cwd);
    sync_in_dir_with_adapters_mode(
        &install_paths,
        cache_root,
        SyncMode::Frozen,
        options.allow_high_sensitivity,
        options.force,
        options.adapters,
        options.sync_on_launch,
        options.execution_mode,
        None,
        options.dependency_failure_mode,
        options.force_rebuild,
        reporter,
    )
}

#[allow(clippy::too_many_arguments, dead_code)]
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
    sync_in_dir_with_adapters_dry_run_full(
        cwd,
        cache_root,
        locked,
        allow_high_sensitivity,
        force,
        adapters,
        sync_on_launch,
        false,
        reporter,
    )
}

/// `sync_in_dir_with_adapters_dry_run` plus the v10 fast-path opt-out.
#[allow(clippy::too_many_arguments)]
pub fn sync_in_dir_with_adapters_dry_run_full(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    force_rebuild: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_with_failure_mode(
        cwd,
        cache_root,
        locked,
        SyncExecutionOptions::new(
            allow_high_sensitivity,
            force,
            adapters,
            sync_on_launch,
            ExecutionMode::DryRun,
            DependencyFailureMode::Graceful,
            force_rebuild,
        ),
        reporter,
    )
}

#[allow(clippy::too_many_arguments, dead_code)]
pub fn sync_in_dir_with_adapters_strict_dry_run(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_strict_dry_run_full(
        cwd,
        cache_root,
        locked,
        allow_high_sensitivity,
        force,
        adapters,
        sync_on_launch,
        false,
        reporter,
    )
}

/// `sync_in_dir_with_adapters_strict_dry_run` plus the v10 fast-path opt-out.
#[allow(clippy::too_many_arguments)]
pub fn sync_in_dir_with_adapters_strict_dry_run_full(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    force_rebuild: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_with_failure_mode(
        cwd,
        cache_root,
        locked,
        SyncExecutionOptions::new(
            allow_high_sensitivity,
            force,
            adapters,
            sync_on_launch,
            ExecutionMode::DryRun,
            DependencyFailureMode::Strict,
            force_rebuild,
        ),
        reporter,
    )
}

#[allow(dead_code)]
pub fn sync_in_dir_with_adapters_frozen_dry_run(
    cwd: &Path,
    cache_root: &Path,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_frozen_dry_run_full(
        cwd,
        cache_root,
        allow_high_sensitivity,
        force,
        adapters,
        sync_on_launch,
        false,
        reporter,
    )
}

/// `sync_in_dir_with_adapters_frozen_dry_run` plus the v10 fast-path opt-out.
#[allow(clippy::too_many_arguments)]
pub fn sync_in_dir_with_adapters_frozen_dry_run_full(
    cwd: &Path,
    cache_root: &Path,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    force_rebuild: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_frozen_with_failure_mode(
        cwd,
        cache_root,
        SyncExecutionOptions::new(
            allow_high_sensitivity,
            force,
            adapters,
            sync_on_launch,
            ExecutionMode::DryRun,
            DependencyFailureMode::Graceful,
            force_rebuild,
        ),
        reporter,
    )
}

#[allow(dead_code)]
pub fn sync_in_dir_with_adapters_frozen_strict_dry_run(
    cwd: &Path,
    cache_root: &Path,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_frozen_strict_dry_run_full(
        cwd,
        cache_root,
        allow_high_sensitivity,
        force,
        adapters,
        sync_on_launch,
        false,
        reporter,
    )
}

/// `sync_in_dir_with_adapters_frozen_strict_dry_run` plus the v10 fast-path
/// opt-out.
#[allow(clippy::too_many_arguments)]
pub fn sync_in_dir_with_adapters_frozen_strict_dry_run_full(
    cwd: &Path,
    cache_root: &Path,
    allow_high_sensitivity: bool,
    force: bool,
    adapters: &[Adapter],
    sync_on_launch: bool,
    force_rebuild: bool,
    reporter: &Reporter,
) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_frozen_with_failure_mode(
        cwd,
        cache_root,
        SyncExecutionOptions::new(
            allow_high_sensitivity,
            force,
            adapters,
            sync_on_launch,
            ExecutionMode::DryRun,
            DependencyFailureMode::Strict,
            force_rebuild,
        ),
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
    dependency_failure_mode: DependencyFailureMode,
    force_rebuild: bool,
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
        dependency_failure_mode,
        force_rebuild,
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
    dependency_failure_mode: DependencyFailureMode,
    force_rebuild: bool,
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
    let legacy_launch_hook_config = root.manifest.uses_legacy_launch_hook_config();
    if legacy_launch_hook_config && sync_mode.checks_lockfile() {
        bail!(
            "legacy manifest field `launch_hooks.sync_on_startup` must be migrated before running {}; rerun plain `nodus sync` to rewrite `nodus.toml` with [[hooks]]",
            sync_mode.flag(),
        );
    }
    if legacy_launch_hook_config {
        reporter.note(
            "migrating legacy manifest field `launch_hooks.sync_on_startup` to `[[hooks]]`",
        )?;
    }
    if has_root_override || selection.should_persist || sync_on_launch || legacy_launch_hook_config
    {
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

    // ---- v10 install_digest drift fast-path ----------------------------
    //
    // Slice 4: when the lockfile is v10, all packages are exactly pinned,
    // each package's `install_digest` is populated, and the on-disk state
    // matches every recorded digest, we can skip the full resolve + render
    // and return a synthetic `SyncSummary` immediately. This is the common
    // case at the start of an editor session ("`nodus sync` on a clean
    // repo") and shaves seconds off the wall time.
    //
    // The fast-path gate is intentionally conservative: any condition we
    // can't cheaply verify (branch-tracking deps, missing digests, root
    // manifest mutation in flight, opt-out flag) falls through to the
    // full sync loop below. `--frozen` is the one mode where a failing
    // gate becomes an error instead of a fallthrough, since the user has
    // explicitly opted into "trust the lockfile".
    let manifest_mutation_pending = has_root_override
        || selection.should_persist
        || sync_on_launch
        || legacy_launch_hook_config;
    let attempt_fast_path = !force_rebuild
        && !manifest_mutation_pending
        && existing_lockfile
            .as_ref()
            .is_some_and(Lockfile::uses_current_schema);
    if attempt_fast_path {
        let lockfile = existing_lockfile
            .as_ref()
            .expect("attempt_fast_path implies existing_lockfile is Some");
        match evaluate_fast_path(lockfile, &install_paths.runtime_root, sync_mode, cache_root)? {
            FastPathOutcome::Hit => {
                reporter.note(format!("{LOCKFILE_NAME} is in sync; no work to do"))?;
                let summary = SyncSummary {
                    package_count: lockfile.packages.len(),
                    adapters: selection.adapters.clone(),
                    managed_file_count: count_owned_files(lockfile),
                };
                return Ok(summary);
            }
            FastPathOutcome::Miss(reason) => {
                if sync_mode.installs_from_lockfile() {
                    bail!(
                        "{LOCKFILE_NAME} is out of date for {}: {reason}. Rerun plain `nodus sync` to repair the lockfile and managed outputs.",
                        sync_mode.flag(),
                    );
                }
                // For non-frozen modes the miss reason is debug-level
                // information only — fall through to the full resolve
                // loop which will repair any drift.
            }
        }
    } else if sync_mode.installs_from_lockfile() && force_rebuild {
        // `--frozen` requires the lockfile to be trusted. An explicit
        // `--no-fast-path` (force_rebuild) flag contradicts that intent —
        // bail before doing any work rather than silently honoring one
        // flag and ignoring the other.
        bail!(
            "{} cannot be combined with `--no-fast-path`",
            sync_mode.flag(),
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
            ResolveProjectOptions::new(
                existing_lockfile.as_ref(),
                existing_lockfile
                    .as_ref()
                    .filter(|_| sync_mode.installs_from_lockfile()),
                Some(&root),
                dependency_failure_mode,
            ),
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
        let codex_native_plugins_auto_enabled = selected_adapters.contains(Adapter::Codex);
        let output_plan = build_output_plan_with_options(
            &install_paths.runtime_root,
            &package_snapshots,
            selected_adapters,
            existing_lockfile.as_ref(),
            OutputPlanOptions {
                merge_existing_mcp: true,
                codex_native_plugins_auto_enabled,
            },
        )?;
        let planned_files = output_plan.files.clone();
        let external_files = output_plan.external_files.clone();
        let desired_paths = resolution.managed_paths_with_options(
            &install_paths.runtime_root,
            selected_adapters,
            codex_native_plugins_auto_enabled,
        )?;
        let lockfile = resolution.to_lockfile_with_options(
            selected_adapters,
            &install_paths.runtime_root,
            codex_native_plugins_auto_enabled,
        )?;
        let mut owned_paths =
            load_owned_paths(&install_paths.runtime_root, existing_lockfile.as_ref())?;
        if existing_lockfile.is_none() {
            owned_paths.exact.extend(recover_runtime_owned_paths(
                &install_paths.runtime_root,
                &desired_paths,
            ));
        }
        owned_paths
            .exact
            .extend(recover_runtime_owned_paths_from_disk(
                &install_paths.runtime_root,
                &desired_paths,
                &planned_files,
            ));
        owned_paths
            .exact
            .extend(adopted_owned_paths.iter().cloned());

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
            )
            .or_else(|| find_runtime_output_collision(&planned_files, &unmanaged_collision)) else {
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
                    let adopted_path = match managed_collision.source {
                        ManagedCollisionSource::RuntimeOutput => {
                            reporter.note(format!(
                                "adopting managed runtime output {}",
                                display_path(&managed_collision.collision_path)
                            ))?;
                            managed_collision.collision_path.clone()
                        }
                        _ => {
                            let ownership_root = install_paths
                                .runtime_root
                                .join(&managed_collision.ownership_root);
                            reporter.note(format!(
                                "adopting managed target {}",
                                display_path(&ownership_root)
                            ))?;
                            ownership_root
                        }
                    };
                    adopted_owned_paths.insert(adopted_path);
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
                    let target = match managed_collision.source {
                        ManagedCollisionSource::RuntimeOutput => {
                            managed_collision.collision_path.clone()
                        }
                        _ => install_paths
                            .runtime_root
                            .join(&managed_collision.ownership_root),
                    };
                    bail!(
                        "cancelled {} because managed target {} collides with existing unmanaged path {}",
                        sync_mode.flag(),
                        display_path(&target),
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
            external_files,
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
        DependencyFailureMode::Graceful,
        false,
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
    resolve_project(
        root,
        cache_root,
        ResolveMode::Sync,
        reporter,
        ResolveProjectOptions::new(None, None, None, DependencyFailureMode::Graceful),
    )
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
        ResolveProjectOptions::new(
            Some(&lockfile),
            Some(&lockfile),
            None,
            DependencyFailureMode::Strict,
        ),
    )?;

    Ok((resolution, lockfile))
}

impl Resolution {
    fn managed_migrations(&self) -> &[ManagedMappingMigration] {
        &self.managed_migrations
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn to_lockfile(
        &self,
        selected_adapters: Adapters,
        runtime_root: &Path,
    ) -> Result<Lockfile> {
        self.to_lockfile_with_options(selected_adapters, runtime_root, false)
    }

    pub fn to_lockfile_with_options(
        &self,
        selected_adapters: Adapters,
        runtime_root: &Path,
        codex_native_plugins_auto_enabled: bool,
    ) -> Result<Lockfile> {
        // Build the output plan ONCE. We feed it twice: once to attribute
        // per-package ownership (subtrees/prefixes/files), and once more (via
        // `output_plan.files`) to compute each package's `install_digest`.
        let package_roots = self
            .packages
            .iter()
            .map(|package| (package.clone(), package.root.clone()))
            .collect::<Vec<_>>();
        let output_plan = build_output_plan_with_options(
            runtime_root,
            &package_roots,
            selected_adapters,
            None,
            OutputPlanOptions {
                merge_existing_mcp: false,
                codex_native_plugins_auto_enabled,
            },
        )?;

        // BTreeMap (not HashMap) so attribute_file_to_package iterates aliases
        // in deterministic alphabetical order. With a HashMap, two packages
        // with overlapping ownership claims would attribute differently across
        // runs and silently shift install_digest contents, breaking the
        // byte-identical-idempotent guarantee.
        let mut per_package_owned: BTreeMap<String, PackageOwnedPaths> = output_plan
            .managed_files_by_package
            .iter()
            .cloned()
            .map(|owned| (owned.alias.clone(), owned))
            .collect();

        let per_package_install_digests =
            install_digests_by_package(runtime_root, &output_plan, &per_package_owned, &[])?;

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

            let owned = per_package_owned.remove(&package.alias).unwrap_or_default();
            let install_digest = per_package_install_digests
                .get(&package.alias)
                .cloned()
                .or_else(|| Some(content_digest(&[])));

            packages.push(LockedPackage {
                alias: package.alias.clone(),
                name: package
                    .manifest
                    .effective_name_for_role(package_role == PackageRole::Root),
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
                    package.manifest.discovered.unique_agent_ids().into_iter(),
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
                mcp_servers: emitted_artifact_ids(
                    package,
                    DependencyComponent::Mcp,
                    package.manifest.manifest.mcp_servers.keys(),
                ),
                dependencies,
                capabilities: package.manifest.manifest.capabilities.clone(),
                owned_subtrees: owned.subtrees,
                owned_prefixes: owned.prefixes,
                owned_runtime_adapters: Vec::new(),
                owned_files: owned.files,
                install_digest,
            });
        }

        compact_owned_runtime_adapter_ownership(&mut packages);

        Ok(Lockfile::new(packages))
    }

    #[allow(dead_code)]
    pub fn managed_paths(
        &self,
        runtime_root: &Path,
        selected_adapters: Adapters,
    ) -> Result<HashSet<PathBuf>> {
        self.managed_paths_with_options(runtime_root, selected_adapters, false)
    }

    pub fn managed_paths_with_options(
        &self,
        runtime_root: &Path,
        selected_adapters: Adapters,
        codex_native_plugins_auto_enabled: bool,
    ) -> Result<HashSet<PathBuf>> {
        // v10 lockfiles no longer populate `legacy_managed_files`, so
        // `Lockfile::managed_paths` returns an empty set on v10 input. Derive
        // the owned root paths directly from the per-package ownership view:
        // subtree roots, exact files, prefix dirs. Doctor and sync consume
        // this list to decide which on-disk paths they may inspect / write /
        // adopt.
        let lockfile = self.to_lockfile_with_options(
            selected_adapters,
            runtime_root,
            codex_native_plugins_auto_enabled,
        )?;
        let owned = lockfile.owned_set(runtime_root)?;
        let mut paths: HashSet<PathBuf> = owned.exact;
        paths.extend(owned.subtrees.iter().cloned());
        paths.extend(owned.prefixes.iter().map(|rule| rule.dir.clone()));

        // For each subtree we own, surface the immediate sub-directories
        // that hold one-artifact-per-subdir (skill folders inside a native
        // plugin: `.nodus/packages/<alias>/<runtime>-plugin/skills/`, agent
        // folders in similar positions). The pre-Slice-3 behavior compressed
        // these subdirs into `desired_paths` via `derivable_runtime_artifact_entries`
        // so `recover_runtime_owned_paths_from_disk` could match an
        // exactly-equivalent pre-written directory tree without needing the
        // whole plugin folder to already exist. We mirror that here by
        // recording any direct child of a subtree that the output plan plans
        // to populate, so the on-disk adoption logic keeps working through
        // the schema bump.
        let package_roots = self
            .packages
            .iter()
            .map(|package| (package.clone(), package.root.clone()))
            .collect::<Vec<_>>();
        let output_plan = build_output_plan_with_options(
            runtime_root,
            &package_roots,
            selected_adapters,
            None,
            OutputPlanOptions {
                merge_existing_mcp: false,
                codex_native_plugins_auto_enabled,
            },
        )?;
        for owned_subtree in &owned.subtrees {
            for file in &output_plan.files {
                let Some(rest) = file.path.strip_prefix(owned_subtree).ok() else {
                    continue;
                };
                // Capture only the IMMEDIATE child directory (e.g. `skills`,
                // `agents`, `commands`, `.codex-plugin` inside a plugin
                // folder). Deeper paths are still owned via the subtree.
                if let Some(first) = rest.components().next() {
                    let child = owned_subtree.join(first.as_os_str());
                    if child != file.path {
                        paths.insert(child);
                    }
                }
            }
        }
        Ok(paths)
    }
}

/// Outcome of evaluating the v10 install_digest drift fast-path.
///
/// `Hit` means the lockfile and disk agree exactly — the caller can return a
/// synthetic `SyncSummary` without doing any further work. `Miss(reason)`
/// surfaces a human-readable explanation of which gate condition failed; the
/// caller logs it under `--frozen` (where missing the fast-path is fatal) and
/// silently falls through under normal sync.
enum FastPathOutcome {
    Hit,
    Miss(String),
}

/// Decide whether the v10 install_digest drift fast-path can short-circuit a
/// sync.
///
/// The lockfile is already known to be v10 and the caller has already filtered
/// out modes that mutate the consumer manifest. This function checks the
/// per-package preconditions:
///
/// - **Freshness gate** (skipped under `--frozen`): every git source is
///   pinned to a `rev` and not tracking a `branch`. Branch-tracked deps can
///   have moved upstream, so the fast-path can't safely skip a re-resolve.
///   `--frozen` opts out of upstream-freshness checking by definition (it
///   uses the recorded `rev` verbatim), so this gate is bypassed there.
/// - **Integrity gate**: every package carries an `install_digest` and the
///   recomputed digest from disk matches it.
///
/// Any failure short-circuits with a descriptive `Miss`. The cost of the
/// disk-walk is bounded by the union of `owned_*` paths the lockfile names,
/// which is exactly the set the full resolve would re-render anyway.
fn evaluate_fast_path(
    lockfile: &Lockfile,
    project_root: &Path,
    sync_mode: SyncMode,
    cache_root: &Path,
) -> Result<FastPathOutcome> {
    let bypass_freshness_gate = sync_mode.installs_from_lockfile();
    let lockfile_mtime = if bypass_freshness_gate {
        None
    } else {
        std::fs::metadata(project_root.join(LOCKFILE_NAME))
            .and_then(|metadata| metadata.modified())
            .ok()
    };
    for package in &lockfile.packages {
        if !bypass_freshness_gate {
            // Source-pin freshness gate. Float-y deps (branch tracking) can
            // change upstream between syncs; the disk content might match the
            // lockfile but the lockfile itself could be stale. Always
            // re-resolve those in non-frozen modes. Path deps are similarly
            // open-ended (the user can edit local files at any time), so we
            // check that nothing under the path source root is newer than
            // the lockfile as a cheap freshness proxy.
            match package.source.kind.as_str() {
                "path" => {
                    if let Some(lockfile_mtime) = lockfile_mtime {
                        let source_root = package
                            .source
                            .path
                            .as_deref()
                            .map(|raw| project_root.join(raw))
                            .unwrap_or_else(|| project_root.to_path_buf());
                        if path_dep_source_is_newer(&source_root, lockfile_mtime, project_root) {
                            return Ok(FastPathOutcome::Miss(format!(
                                "package `{}` has on-disk source newer than the lockfile",
                                package.alias
                            )));
                        }
                    } else {
                        return Ok(FastPathOutcome::Miss(format!(
                            "package `{}` is a path dependency but the lockfile mtime could not be read",
                            package.alias
                        )));
                    }
                }
                "git" => {
                    if package.source.rev.is_none() {
                        return Ok(FastPathOutcome::Miss(format!(
                            "package `{}` has no pinned git revision",
                            package.alias
                        )));
                    }
                    if package.source.branch.is_some() {
                        return Ok(FastPathOutcome::Miss(format!(
                            "package `{}` tracks branch `{}`; upstream may have moved",
                            package.alias,
                            package.source.branch.as_deref().unwrap_or(""),
                        )));
                    }
                }
                other => {
                    return Ok(FastPathOutcome::Miss(format!(
                        "package `{}` has unrecognized source kind `{}`",
                        package.alias, other
                    )));
                }
            }
        }

        // install_digest gate. Slice 3 always stamps a digest on v10
        // emissions (defaulting to `content_digest(&[])` for empty packages),
        // so `None` here means the lockfile was hand-edited or upgraded from
        // a pre-Slice-3 schema by a different tool.
        let Some(recorded) = package.install_digest.as_deref() else {
            return Ok(FastPathOutcome::Miss(format!(
                "package `{}` has no recorded install_digest",
                package.alias
            )));
        };

        // Disk-digest gate. `Ok(None)` means an `owned_files` entry is
        // missing on disk — drift, fall back to full sync.
        let Some(disk_digest) = install_digest_from_disk(project_root, lockfile, package)? else {
            return Ok(FastPathOutcome::Miss(format!(
                "package `{}` has an owned file missing on disk",
                package.alias
            )));
        };

        if disk_digest != recorded {
            return Ok(FastPathOutcome::Miss(format!(
                "package `{}` install_digest mismatch (disk drift)",
                package.alias
            )));
        }

        // Cache-presence gate. `nodus clean` plus a stale lockfile leaves
        // disk consistent but the shared cache empty; downstream commands
        // (`doctor`, `update`) need the cache present. Fall through so the
        // full resolve repopulates it.
        let snapshot_path = crate::store::snapshot_path(cache_root, &package.digest)?;
        if !snapshot_path.exists() {
            return Ok(FastPathOutcome::Miss(format!(
                "package `{}` snapshot is missing from the shared cache",
                package.alias
            )));
        }
    }

    Ok(FastPathOutcome::Hit)
}

/// Cheap freshness probe for path-dep sources.
///
/// Walks the source root and returns `true` if any non-runtime file's mtime
/// is strictly newer than `lockfile_mtime`. We skip everything under
/// `project_root/.nodus`, `.claude`, `.codex`, etc. — those are the runtime
/// outputs Nodus writes during sync, which would always be at least as new
/// as the lockfile and would trip every fast-path check otherwise.
///
/// mtime-based detection is a heuristic, not a proof. False positives (mtime
/// bumped by an unrelated tool like a git checkout) cause an unneeded full
/// sync, which is correct-but-slow. False negatives (someone restored a
/// snapshot to an older mtime) cause a missed sync, which the user can
/// recover from via `nodus sync --no-fast-path`. The trade is acceptable
/// because the alternative — recomputing every path dep's source digest —
/// duplicates the bulk of a full resolve and erases the fast-path benefit.
fn path_dep_source_is_newer(
    source_root: &Path,
    lockfile_mtime: std::time::SystemTime,
    project_root: &Path,
) -> bool {
    use walkdir::WalkDir;

    // Names at the top of `project_root` we know Nodus writes during sync.
    // When the path-dep source root equals the project root (the common
    // "consumer = root package" case) we have to filter these out or the
    // freshness probe always trips on Nodus's own outputs.
    let nodus_owned_top_level = [
        ".nodus",
        ".claude",
        ".claude-plugin",
        ".codex",
        ".cursor",
        ".github",
        ".opencode",
        ".agents",
        "nodus.lock",
    ];
    let canonical_project_root = std::fs::canonicalize(project_root).ok();
    for entry in WalkDir::new(source_root).follow_links(false) {
        let Ok(entry) = entry else {
            // Walk errors don't disqualify the fast-path on their own —
            // the integrity gate's disk reads will surface real errors.
            continue;
        };
        let path = entry.path();
        // Skip Nodus-managed top-level dirs at the project root.
        if let Some(canonical_project_root) = canonical_project_root.as_ref()
            && let Ok(rel) = path.strip_prefix(canonical_project_root)
            && let Some(first) = rel.components().next()
            && let Some(first_str) = first.as_os_str().to_str()
            && nodus_owned_top_level.contains(&first_str)
        {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(project_root)
            && let Some(first) = rel.components().next()
            && let Some(first_str) = first.as_os_str().to_str()
            && nodus_owned_top_level.contains(&first_str)
        {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified > lockfile_mtime {
            return true;
        }
    }
    false
}

/// Approximate the `managed_file_count` summary field on a fast-path hit.
///
/// The pre-fast-path code derived this from the rendered `planned_files`
/// vector. On the fast-path we never render — we use the lockfile's
/// per-package `owned_*` rules instead. Counting subtrees / prefix rules / exact
/// files gives the user a sensible number for the "managed files" summary
/// line without forcing a disk walk just to count.
fn count_owned_files(lockfile: &Lockfile) -> usize {
    let names =
        crate::adapters::ManagedArtifactNames::from_locked_packages(lockfile.packages.iter());
    lockfile
        .packages
        .iter()
        .map(|package| {
            let runtime_owned_count = package
                .owned_runtime_adapters
                .iter()
                .map(|adapter| {
                    let paths = locked_runtime_adapter_owned_paths(&names, package, *adapter);
                    paths.files.len() + paths.subtrees.len()
                })
                .sum::<usize>();
            package.owned_files.len()
                + package.owned_subtrees.len()
                + package.owned_prefixes.len()
                + runtime_owned_count
        })
        .sum()
}

/// Compute per-package `install_digest` (`blake3:<hex>`) from the output plan.
///
/// Each emitted file is attributed to the owning package by consulting
/// `per_package_owned` (the same per-package ownership rules we emit into the
/// lockfile). Files are sorted by `target_relative_path` before hashing so the
/// digest is stable across equivalent resolutions. The digest covers
/// `(target_relative_path, contents)` for every attributed file.
///
/// Packages with no attributed files don't appear in the returned map; the
/// caller stamps `content_digest(&[])` on them so v10 lockfiles always carry a
/// digest (Slice 4's drift fast-path needs a stable empty-install baseline).
///
/// `extra_files` is a compatibility input for files outside the ordinary
/// output plan that still appear in a package ownership view. Most managed
/// outputs, including native marketplace JSONs, should already be in
/// `output_plan.files`.
fn install_digests_by_package(
    runtime_root: &Path,
    output_plan: &OutputPlan,
    per_package_owned: &BTreeMap<String, PackageOwnedPaths>,
    extra_files: &[ManagedFile],
) -> Result<HashMap<String, String>> {
    let mut per_package_entries: BTreeMap<String, BTreeMap<PathBuf, Vec<u8>>> = BTreeMap::new();

    let all_files = output_plan.files.iter().chain(extra_files.iter());
    for file in all_files {
        let target_relative = file
            .path
            .strip_prefix(runtime_root)
            .unwrap_or(&file.path)
            .to_path_buf();
        let Some(alias) = attribute_file_to_package(&target_relative, per_package_owned) else {
            // Unattributed files exist in the on-disk plan but aren't part of
            // any package's ownership view. They don't contribute to a
            // per-package install_digest.
            continue;
        };
        per_package_entries
            .entry(alias)
            .or_default()
            .insert(target_relative, file.contents.clone());
    }

    let mut digests = HashMap::with_capacity(per_package_entries.len());
    for (alias, entries) in per_package_entries {
        let entries_for_digest: Vec<(String, Vec<u8>)> = entries
            .into_iter()
            .map(|(path, contents)| (display_path(&path), contents))
            .collect();
        let digest_input: Vec<(&str, &[u8])> = entries_for_digest
            .iter()
            .map(|(path, contents)| (path.as_str(), contents.as_slice()))
            .collect();
        digests.insert(alias, content_digest(&digest_input));
    }
    Ok(digests)
}

/// Return the package alias that owns `target_relative` according to the
/// per-package categorization we've already built. Mirrors
/// `OwnedSet::contains` (subtree starts_with, exact path match, prefix dir +
/// stem prefix) but returns the alias instead of a boolean so the install
/// digest computation can bucket files per package.
fn attribute_file_to_package(
    target_relative: &Path,
    per_package_owned: &BTreeMap<String, PackageOwnedPaths>,
) -> Option<String> {
    // Subtree match wins: a file living under a package's owned subtree is
    // attributed to that package regardless of whether another package also
    // declares an exact file match (the latter would be redundant).
    //
    // BTreeMap iteration is alphabetically deterministic — overlapping claims
    // resolve in a stable order so install_digest distribution stays
    // byte-identical across runs.
    for (alias, owned) in per_package_owned {
        if owned
            .subtrees
            .iter()
            .any(|subtree| target_relative.starts_with(Path::new(subtree)))
        {
            return Some(alias.clone());
        }
    }
    for (alias, owned) in per_package_owned {
        if owned.files.iter().any(|file| {
            let owned = Path::new(file);
            target_relative == owned || target_relative.starts_with(owned)
        }) {
            return Some(alias.clone());
        }
    }
    for (alias, owned) in per_package_owned {
        if owned.prefixes.iter().any(|rule| {
            target_relative.parent() == Some(Path::new(&rule.dir))
                && target_relative
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&rule.prefix))
        }) {
            return Some(alias.clone());
        }
    }
    None
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
                .map(|member| member.alias),
        );
    }

    dependencies.sort();
    dependencies.dedup();
    Ok(dependencies)
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
