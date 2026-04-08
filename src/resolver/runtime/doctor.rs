use std::collections::HashSet;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use rayon::prelude::*;
use serde::Serialize;

use super::resolve::{resolve_project, validate_git_package};
use super::support::{
    build_sync_execution_plan, execute_sync_plan, find_managed_collision, find_unmanaged_collision,
    load_owned_paths, managed_merge_paths, managed_path_is_owned,
    planned_workspace_marketplace_files, recover_runtime_owned_paths_from_disk,
    remove_path_and_empty_parents, unmanaged_collision_guidance, validate_state_consistency,
};
use super::{Resolution, ResolveMode, SyncMode, lockfile_out_of_date_message};
use crate::adapters::{Adapters, ManagedFile, build_output_plan};
use crate::execution::ExecutionMode;
use crate::lockfile::{LOCKFILE_NAME, Lockfile};
use crate::manifest::{LoadedManifest, load_root_from_dir};
use crate::paths::display_path;
use crate::report::Reporter;
use crate::selection::resolve_adapter_selection;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorMode {
    Repair,
    Check,
    Force,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorRunMode {
    Preview,
    Apply,
    ApplyForce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Healthy,
    Fixed,
    Blocked,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorFindingKind {
    Informational,
    SafeAutoFix,
    RiskyFix,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorFinding {
    pub kind: DoctorFindingKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorActionRecord {
    pub message: String,
}

impl DoctorActionRecord {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorSummary {
    pub mode: DoctorRunMode,
    pub package_count: usize,
    pub warnings: Vec<String>,
    pub status: DoctorStatus,
    pub findings: Vec<DoctorFinding>,
    pub applied_actions: Vec<DoctorActionRecord>,
}

#[derive(Debug, Clone)]
struct DoctorInspection {
    package_count: usize,
    warnings: Vec<String>,
    findings: Vec<DoctorFinding>,
    original_root: LoadedManifest,
    working_root: LoadedManifest,
    lockfile_path: PathBuf,
    expected_lockfile: Lockfile,
    runtime_root: PathBuf,
    owned_paths: HashSet<PathBuf>,
    desired_paths: HashSet<PathBuf>,
    planned_files: Vec<ManagedFile>,
    sync_summary: super::SyncSummary,
    has_existing_lockfile: bool,
    has_missing_managed_files: bool,
    invalid_owned_outputs: Vec<PathBuf>,
    risky_actions: Vec<DoctorAction>,
}

struct LockfileInspection<'a> {
    cwd: &'a Path,
    resolution: &'a Resolution,
    selected_adapters: Adapters,
    lockfile_path: &'a Path,
    existing_lockfile: Option<&'a Lockfile>,
    owned_paths: &'a HashSet<PathBuf>,
    planned_files: &'a [ManagedFile],
    desired_paths: &'a HashSet<PathBuf>,
}

struct LockfileInspectionResult {
    findings: Vec<DoctorFinding>,
    risky_actions: Vec<DoctorAction>,
    has_missing_managed_files: bool,
}

#[derive(Debug, Clone)]
struct DoctorPlan {
    package_count: usize,
    warnings: Vec<String>,
    findings: Vec<DoctorFinding>,
    safe_actions: Vec<DoctorAction>,
    risky_actions: Vec<DoctorAction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DoctorAction {
    RebuildManagedOutputs,
    RemoveConflictingManagedPath { path: PathBuf, reason: String },
}

trait DoctorPrompt {
    fn confirm(&mut self, action: &DoctorAction) -> Result<bool>;
}

struct TtyDoctorPrompt;

impl DoctorPrompt for TtyDoctorPrompt {
    fn confirm(&mut self, action: &DoctorAction) -> Result<bool> {
        if !should_prompt_for_doctor_action() {
            return Ok(false);
        }
        let stdin = io::stdin();
        let mut input = stdin.lock();
        let stderr = io::stderr();
        let mut output = stderr.lock();
        prompt_for_doctor_action(action, &mut input, &mut output)
    }
}

pub fn doctor_in_dir_with_mode(
    cwd: &Path,
    cache_root: &Path,
    mode: DoctorMode,
    reporter: &Reporter,
) -> Result<DoctorSummary> {
    let inspection = inspect_doctor_state(cwd, cache_root, reporter)?;
    let plan = build_doctor_plan(&inspection)?;
    execute_doctor_plan(&inspection, plan, mode, reporter)
}

fn inspect_doctor_state(
    cwd: &Path,
    cache_root: &Path,
    reporter: &Reporter,
) -> Result<DoctorInspection> {
    let root = load_root_from_dir(cwd)?;
    let selection = resolve_adapter_selection(cwd, &root.manifest, &[], false)?;
    let selected_adapters = Adapters::from_slice(&selection.adapters);
    reporter.status(
        "Checking",
        "manifest, lockfile, shared store, and managed outputs",
    )?;

    let resolution = resolve_project(cwd, cache_root, ResolveMode::Doctor, reporter, None, None)?;
    resolution
        .packages
        .par_iter()
        .map(|package| validate_git_package(package, cache_root))
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<Result<Vec<_>>>()?;

    let package_roots = resolution
        .packages
        .iter()
        .map(|package| (package.clone(), package.root.clone()))
        .collect::<Vec<_>>();
    let lockfile_path = cwd.join(LOCKFILE_NAME);
    let existing_lockfile = if lockfile_path.exists() {
        Some(Lockfile::read(&lockfile_path)?)
    } else {
        None
    };
    let desired_paths = resolution.managed_paths(cwd, selected_adapters)?;
    let mut owned_paths = load_owned_paths(cwd, existing_lockfile.as_ref())?;
    let workspace_marketplace_files = planned_workspace_marketplace_files(&root, cwd)?;
    let mut invalid_owned_outputs = Vec::new();
    let output_plan = match build_output_plan(
        cwd,
        &package_roots,
        selected_adapters,
        existing_lockfile.as_ref(),
        true,
        Some(cache_root),
    ) {
        Ok(plan) => plan,
        Err(error) => {
            let Some(path) = invalid_owned_output_path(&error, cwd, &owned_paths) else {
                return Err(error);
            };
            invalid_owned_outputs.push(path);
            build_output_plan(
                cwd,
                &package_roots,
                selected_adapters,
                existing_lockfile.as_ref(),
                false,
                Some(cache_root),
            )?
        }
    };
    let mut planned_files = output_plan.files;
    let mut desired_paths = desired_paths;
    desired_paths.extend(
        workspace_marketplace_files
            .iter()
            .map(|file| file.path.clone()),
    );
    planned_files.extend(workspace_marketplace_files);
    let managed_file_count = planned_files.len();
    if existing_lockfile.is_none() {
        owned_paths.extend(recover_runtime_owned_paths_from_disk(
            cwd,
            &desired_paths,
            &planned_files,
        ));
    }
    let mut warnings = resolution
        .warnings
        .iter()
        .chain(output_plan.warnings.iter())
        .cloned()
        .collect::<Vec<_>>();
    warnings.sort();
    warnings.dedup();

    let lockfile_inspection = LockfileInspection {
        cwd,
        resolution: &resolution,
        selected_adapters,
        lockfile_path: &lockfile_path,
        existing_lockfile: existing_lockfile.as_ref(),
        owned_paths: &owned_paths,
        planned_files: &planned_files,
        desired_paths: &desired_paths,
    }
    .inspect()?;

    let mut inspection = DoctorInspection {
        package_count: resolution.packages.len(),
        warnings,
        findings: lockfile_inspection.findings,
        original_root: root.clone(),
        working_root: root,
        lockfile_path,
        expected_lockfile: resolution.to_lockfile(selected_adapters, cwd)?,
        runtime_root: cwd.to_path_buf(),
        owned_paths,
        desired_paths,
        planned_files,
        sync_summary: super::SyncSummary {
            package_count: resolution.packages.len(),
            adapters: selection.adapters,
            managed_file_count,
        },
        has_existing_lockfile: existing_lockfile.is_some(),
        has_missing_managed_files: lockfile_inspection.has_missing_managed_files,
        invalid_owned_outputs,
        risky_actions: lockfile_inspection.risky_actions,
    };
    let mut drift_findings = Vec::new();
    classify_output_drift(&inspection, &mut drift_findings);
    inspection.findings.extend(drift_findings);
    Ok(inspection)
}

impl LockfileInspection<'_> {
    fn inspect(&self) -> Result<LockfileInspectionResult> {
        let mut findings = Vec::new();
        let mut risky_actions = Vec::new();
        let mut has_missing_managed_files = false;

        if let Some(existing_lockfile) = self.existing_lockfile {
            let expected_lockfile = self
                .resolution
                .to_lockfile(self.selected_adapters, self.cwd)?;
            if *existing_lockfile != expected_lockfile {
                findings.push(DoctorFinding {
                    kind: DoctorFindingKind::SafeAutoFix,
                    message: lockfile_out_of_date_message(),
                });
            }
        } else {
            findings.push(DoctorFinding {
                kind: DoctorFindingKind::SafeAutoFix,
                message: format!(
                    "missing {}",
                    self.lockfile_path.file_name().unwrap().to_string_lossy()
                ),
            });
        }

        if let Some(collision) =
            find_unmanaged_collision(self.planned_files, self.owned_paths, self.cwd)
        {
            let message = if let Some(managed_collision) =
                find_managed_collision(self.cwd, self.resolution, &collision)
            {
                unmanaged_collision_guidance(self.cwd, &managed_collision, SyncMode::Normal)
            } else {
                format!(
                    "refusing to overwrite unmanaged file {}",
                    crate::paths::display_path(&collision.path)
                )
            };
            findings.push(DoctorFinding {
                kind: DoctorFindingKind::RiskyFix,
                message: message.clone(),
            });
            risky_actions.push(DoctorAction::RemoveConflictingManagedPath {
                path: collision.path,
                reason: message,
            });
        }
        if self.existing_lockfile.is_none() {
            for merge_path in unmanaged_missing_lockfile_merge_collisions(
                self.cwd,
                self.owned_paths,
                self.planned_files,
            ) {
                let message = format!(
                    "refusing to overwrite unmanaged file {}",
                    display_path(&merge_path)
                );
                findings.push(DoctorFinding {
                    kind: DoctorFindingKind::RiskyFix,
                    message: message.clone(),
                });
                risky_actions.push(DoctorAction::RemoveConflictingManagedPath {
                    path: merge_path,
                    reason: message,
                });
            }
        }

        if let Err(error) =
            validate_state_consistency(self.owned_paths, self.desired_paths, self.planned_files)
        {
            let message = error.to_string();
            if message.starts_with("managed file is missing from disk") {
                has_missing_managed_files = true;
            }
            findings.push(classify_state_consistency_finding(message));
        }

        Ok(LockfileInspectionResult {
            findings,
            risky_actions,
            has_missing_managed_files,
        })
    }
}

fn classify_state_consistency_finding(message: String) -> DoctorFinding {
    let kind = if message.starts_with("managed file is missing from disk")
        || message.starts_with("stale managed state entry for")
    {
        DoctorFindingKind::SafeAutoFix
    } else {
        DoctorFindingKind::Manual
    };
    DoctorFinding { kind, message }
}

fn classify_output_drift(inspection: &DoctorInspection, findings: &mut Vec<DoctorFinding>) {
    if inspection.has_missing_managed_files || inspection.has_invalid_owned_output() {
        findings.push(DoctorFinding::safe_auto_fix(
            "managed outputs drifted from the declared project state",
        ));
    }
}

fn build_doctor_plan(inspection: &DoctorInspection) -> Result<DoctorPlan> {
    let mut safe_actions = Vec::new();
    let can_repair = inspection.findings.iter().all(|finding| {
        matches!(
            finding.kind,
            DoctorFindingKind::Informational
                | DoctorFindingKind::SafeAutoFix
                | DoctorFindingKind::RiskyFix
        )
    });
    if can_repair && !inspection.findings.is_empty() {
        safe_actions.push(DoctorAction::RebuildManagedOutputs);
    }

    Ok(DoctorPlan {
        package_count: inspection.package_count,
        warnings: inspection.warnings.clone(),
        findings: inspection.findings.clone(),
        safe_actions,
        risky_actions: inspection.risky_actions.clone(),
    })
}

fn execute_doctor_plan(
    inspection: &DoctorInspection,
    plan: DoctorPlan,
    mode: DoctorMode,
    reporter: &Reporter,
) -> Result<DoctorSummary> {
    for warning in &plan.warnings {
        reporter.warning(warning)?;
    }

    if let Some(finding) = plan
        .findings
        .iter()
        .find(|finding| matches!(finding.kind, DoctorFindingKind::Manual))
    {
        bail!("{}", finding.message);
    }

    match mode {
        DoctorMode::Check => Ok(plan.into_summary(mode, Vec::new())),
        DoctorMode::Repair => {
            let mut prompt = TtyDoctorPrompt;
            let mut applied_actions = Vec::new();
            let risky_actions = plan.risky_actions().cloned().collect::<Vec<_>>();
            for action in &risky_actions {
                if !prompt.confirm(action)? {
                    return Ok(plan.into_blocked_summary(mode));
                }
                applied_actions.push(apply_risky_action(
                    action,
                    &inspection.runtime_root,
                    reporter,
                )?);
            }
            if plan.needs_safe_repair() {
                applied_actions.extend(execute_safe_repairs(inspection, reporter)?);
                return Ok(plan.into_summary(mode, applied_actions));
            }
            Ok(plan.into_summary(mode, applied_actions))
        }
        DoctorMode::Force => {
            let mut applied_actions = Vec::new();
            let risky_actions = plan.risky_actions().cloned().collect::<Vec<_>>();
            for action in &risky_actions {
                applied_actions.push(apply_risky_action(
                    action,
                    &inspection.runtime_root,
                    reporter,
                )?);
            }
            if plan.needs_safe_repair() {
                applied_actions.extend(execute_safe_repairs(inspection, reporter)?);
            }
            Ok(plan.into_summary(mode, applied_actions))
        }
    }
}

fn execute_safe_repairs(
    inspection: &DoctorInspection,
    reporter: &Reporter,
) -> Result<Vec<DoctorActionRecord>> {
    let plan = build_sync_execution_plan(
        &inspection.original_root,
        &inspection.working_root,
        &inspection.lockfile_path,
        &inspection.expected_lockfile,
        &inspection.runtime_root,
        &inspection.owned_paths,
        &inspection.desired_paths,
        &inspection.planned_files,
        inspection.warnings.clone(),
        inspection.sync_summary.clone(),
        SyncMode::Normal,
    )?;
    execute_sync_plan(&plan, ExecutionMode::Apply, reporter)?;

    let mut records = Vec::new();
    if inspection.has_existing_lockfile {
        records.push(DoctorActionRecord::new(
            "rewrote managed outputs from the existing lockfile",
        ));
    } else {
        records.push(DoctorActionRecord::new(
            "rewrote managed outputs and regenerated nodus.lock",
        ));
    }
    for path in &inspection.invalid_owned_outputs {
        records.push(DoctorActionRecord::new(format!(
            "rewrote managed output {}",
            display_path(path)
        )));
    }
    Ok(records)
}

fn doctor_status(
    findings: &[DoctorFinding],
    applied_actions: &[DoctorActionRecord],
) -> DoctorStatus {
    if !applied_actions.is_empty() {
        DoctorStatus::Fixed
    } else if findings.is_empty() {
        DoctorStatus::Healthy
    } else {
        DoctorStatus::Blocked
    }
}

fn unmanaged_missing_lockfile_merge_collisions(
    runtime_root: &Path,
    owned_paths: &HashSet<PathBuf>,
    planned_files: &[ManagedFile],
) -> Vec<PathBuf> {
    let managed_merge_paths = managed_merge_paths(runtime_root);
    let mut collisions = planned_files
        .iter()
        .map(|file| &file.path)
        .filter(|path| managed_merge_paths.contains(*path))
        .filter(|path| path.exists() && !managed_path_is_owned(path, owned_paths))
        .cloned()
        .collect::<Vec<_>>();
    collisions.sort();
    collisions
}

fn invalid_owned_output_path(
    error: &anyhow::Error,
    runtime_root: &Path,
    owned_paths: &HashSet<PathBuf>,
) -> Option<PathBuf> {
    [
        (
            runtime_root.join(".claude/settings.local.json"),
            "failed to parse existing",
        ),
        (runtime_root.join(".mcp.json"), "failed to parse MCP config"),
        (
            runtime_root.join("opencode.json"),
            "failed to parse OpenCode config",
        ),
        (
            runtime_root.join(".codex/config.toml"),
            "failed to parse Codex config",
        ),
    ]
    .into_iter()
    .find_map(|(path, prefix)| {
        if error.to_string().contains(prefix) && managed_path_is_owned(&path, owned_paths) {
            Some(path)
        } else {
            None
        }
    })
}

impl DoctorInspection {
    fn has_invalid_owned_output(&self) -> bool {
        !self.invalid_owned_outputs.is_empty()
    }
}

impl DoctorFinding {
    fn safe_auto_fix(message: impl Into<String>) -> Self {
        Self {
            kind: DoctorFindingKind::SafeAutoFix,
            message: message.into(),
        }
    }
}

impl DoctorPlan {
    fn into_blocked_summary(self, mode: DoctorMode) -> DoctorSummary {
        self.into_summary(mode, Vec::new())
    }

    fn into_summary(
        self,
        mode: DoctorMode,
        applied_actions: Vec<DoctorActionRecord>,
    ) -> DoctorSummary {
        let status = doctor_status(&self.findings, &applied_actions);
        DoctorSummary {
            mode: doctor_run_mode(mode),
            package_count: self.package_count,
            warnings: self.warnings,
            status,
            findings: self.findings,
            applied_actions,
        }
    }

    fn needs_safe_repair(&self) -> bool {
        self.safe_actions
            .iter()
            .any(|action| matches!(action, DoctorAction::RebuildManagedOutputs))
    }

    fn risky_actions(&self) -> impl Iterator<Item = &DoctorAction> {
        self.risky_actions.iter()
    }
}

fn doctor_run_mode(mode: DoctorMode) -> DoctorRunMode {
    match mode {
        DoctorMode::Check => DoctorRunMode::Preview,
        DoctorMode::Repair => DoctorRunMode::Apply,
        DoctorMode::Force => DoctorRunMode::ApplyForce,
    }
}

fn should_prompt_for_doctor_action() -> bool {
    !cfg!(test) && io::stdin().is_terminal() && io::stderr().is_terminal()
}

fn prompt_for_doctor_action(
    action: &DoctorAction,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<bool> {
    match action {
        DoctorAction::RemoveConflictingManagedPath { path, reason } => {
            writeln!(output, "Nodus needs to remove {}.", display_path(path))?;
            writeln!(output, "{reason}")?;
            write!(output, "Continue? [y/N] ")?;
            output.flush()?;

            let mut line = String::new();
            if input.read_line(&mut line)? == 0 {
                return Ok(false);
            }
            Ok(matches!(
                line.trim().to_ascii_lowercase().as_str(),
                "y" | "yes"
            ))
        }
        DoctorAction::RebuildManagedOutputs => Ok(true),
    }
}

fn apply_risky_action(
    action: &DoctorAction,
    runtime_root: &Path,
    reporter: &Reporter,
) -> Result<DoctorActionRecord> {
    match action {
        DoctorAction::RemoveConflictingManagedPath { path, .. } => {
            reporter.status("Removing", path.display())?;
            remove_path_and_empty_parents(path, runtime_root)?;
            Ok(DoctorActionRecord::new(format!(
                "removed conflicting managed subtree {}",
                display_path(path)
            )))
        }
        DoctorAction::RebuildManagedOutputs => {
            unreachable!("safe doctor action routed incorrectly")
        }
    }
}
