use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use rayon::prelude::*;
use serde::Serialize;

use super::resolve::{resolve_project, validate_git_package};
use super::support::{
    build_sync_execution_plan, execute_sync_plan, find_managed_collision, find_unmanaged_collision,
    load_owned_paths, managed_path_is_owned, recover_runtime_owned_dirs_from_disk,
    unmanaged_collision_guidance, validate_state_consistency,
};
use super::{lockfile_out_of_date_message, Resolution, ResolveMode, SyncMode};
use crate::adapters::{build_output_plan, Adapters, ManagedFile};
use crate::execution::ExecutionMode;
use crate::lockfile::{Lockfile, LOCKFILE_NAME};
use crate::manifest::{load_root_from_dir, LoadedManifest};
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
}

#[derive(Debug, Clone)]
struct DoctorPlan {
    package_count: usize,
    warnings: Vec<String>,
    findings: Vec<DoctorFinding>,
    actions: Vec<DoctorAction>,
    applied_actions: Vec<DoctorActionRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DoctorAction {
    RebuildManagedOutputs,
    RewriteManagedFile { path: PathBuf },
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
    let mut invalid_owned_outputs = Vec::new();
    let output_plan = match build_output_plan(
        cwd,
        &package_roots,
        selected_adapters,
        existing_lockfile.as_ref(),
        true,
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
            )?
        }
    };
    if existing_lockfile.is_none() {
        owned_paths.extend(recover_runtime_owned_dirs_from_disk(
            cwd,
            &desired_paths,
            &output_plan.files,
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

    let mut findings = Vec::new();
    let has_missing_managed_files = inspect_lockfile_state(
        cwd,
        &resolution,
        selected_adapters,
        &lockfile_path,
        existing_lockfile.as_ref(),
        &owned_paths,
        &output_plan.files,
        &desired_paths,
        &mut findings,
    )?;

    let mut inspection = DoctorInspection {
        package_count: resolution.packages.len(),
        warnings,
        findings,
        original_root: root.clone(),
        working_root: root,
        lockfile_path,
        expected_lockfile: resolution.to_lockfile(selected_adapters, cwd)?,
        runtime_root: cwd.to_path_buf(),
        owned_paths,
        desired_paths,
        planned_files: output_plan.files,
        sync_summary: super::SyncSummary {
            package_count: resolution.packages.len(),
            adapters: selection.adapters,
            managed_file_count: output_plan.managed_files.len(),
        },
        has_existing_lockfile: existing_lockfile.is_some(),
        has_missing_managed_files,
        invalid_owned_outputs,
    };
    let mut drift_findings = Vec::new();
    classify_output_drift(&inspection, &mut drift_findings);
    inspection.findings.extend(drift_findings);
    Ok(inspection)
}

fn inspect_lockfile_state(
    cwd: &Path,
    resolution: &Resolution,
    selected_adapters: Adapters,
    lockfile_path: &Path,
    existing_lockfile: Option<&Lockfile>,
    owned_paths: &HashSet<PathBuf>,
    planned_files: &[crate::adapters::ManagedFile],
    desired_paths: &std::collections::HashSet<std::path::PathBuf>,
    findings: &mut Vec<DoctorFinding>,
) -> Result<bool> {
    if let Some(existing_lockfile) = existing_lockfile {
        let expected_lockfile = resolution.to_lockfile(selected_adapters, cwd)?;
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
                lockfile_path.file_name().unwrap().to_string_lossy()
            ),
        });
    };

    if let Some(collision) = find_unmanaged_collision(planned_files, &owned_paths, cwd) {
        let message =
            if let Some(managed_collision) = find_managed_collision(cwd, resolution, &collision) {
                unmanaged_collision_guidance(cwd, &managed_collision, SyncMode::Normal)
            } else {
                format!(
                    "refusing to overwrite unmanaged file {}",
                    crate::paths::display_path(&collision.path)
                )
            };
        findings.push(DoctorFinding {
            kind: DoctorFindingKind::RiskyFix,
            message,
        });
    }
    if existing_lockfile.is_none() {
        if let Some(mcp_path) =
            unmanaged_missing_lockfile_mcp_collision(cwd, owned_paths, planned_files)
        {
            findings.push(DoctorFinding {
                kind: DoctorFindingKind::RiskyFix,
                message: format!(
                    "refusing to overwrite unmanaged file {}",
                    display_path(&mcp_path)
                ),
            });
        }
    }

    let mut has_missing_managed_files = false;
    if let Err(error) = validate_state_consistency(&owned_paths, desired_paths, planned_files) {
        let message = error.to_string();
        if message.starts_with("managed file is missing from disk") {
            has_missing_managed_files = true;
        }
        findings.push(classify_state_consistency_finding(message));
    }

    Ok(has_missing_managed_files)
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
    let mut actions = Vec::new();
    let can_auto_repair = inspection.findings.iter().all(|finding| {
        matches!(
            finding.kind,
            DoctorFindingKind::Informational | DoctorFindingKind::SafeAutoFix
        )
    });
    if can_auto_repair && !inspection.findings.is_empty() {
        actions.push(DoctorAction::RebuildManagedOutputs);
        for path in &inspection.invalid_owned_outputs {
            actions.push(DoctorAction::RewriteManagedFile { path: path.clone() });
        }
    }

    Ok(DoctorPlan {
        package_count: inspection.package_count,
        warnings: inspection.warnings.clone(),
        findings: inspection.findings.clone(),
        actions,
        applied_actions: Vec::new(),
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

    match mode {
        DoctorMode::Force => bail!("doctor force mode is not implemented yet"),
        DoctorMode::Check => Ok(DoctorSummary {
            package_count: plan.package_count,
            warnings: plan.warnings,
            status: doctor_status(&plan.findings, &plan.applied_actions),
            findings: plan.findings,
            applied_actions: plan.applied_actions,
        }),
        DoctorMode::Repair => {
            if !plan.actions.is_empty() {
                let applied_actions = execute_safe_repairs(inspection, reporter)?;
                return Ok(DoctorSummary {
                    package_count: plan.package_count,
                    warnings: plan.warnings,
                    status: doctor_status(&plan.findings, &applied_actions),
                    findings: plan.findings,
                    applied_actions,
                });
            }

            if let Some(finding) = plan
                .findings
                .iter()
                .find(|finding| !matches!(finding.kind, DoctorFindingKind::SafeAutoFix))
                .or_else(|| plan.findings.first())
            {
                bail!("{}", finding.message);
            }

            Ok(DoctorSummary {
                package_count: plan.package_count,
                warnings: plan.warnings,
                status: doctor_status(&plan.findings, &plan.applied_actions),
                findings: plan.findings,
                applied_actions: plan.applied_actions,
            })
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

fn unmanaged_missing_lockfile_mcp_collision(
    runtime_root: &Path,
    owned_paths: &HashSet<PathBuf>,
    planned_files: &[ManagedFile],
) -> Option<PathBuf> {
    let mcp_path = runtime_root.join(".mcp.json");
    planned_files
        .iter()
        .find(|file| file.path == mcp_path)
        .map(|file| &file.path)
        .filter(|path| path.exists() && !managed_path_is_owned(path, owned_paths))
        .cloned()
}

fn invalid_owned_output_path(
    error: &anyhow::Error,
    runtime_root: &Path,
    owned_paths: &HashSet<PathBuf>,
) -> Option<PathBuf> {
    let mcp_path = runtime_root.join(".mcp.json");
    if error.to_string().contains("failed to parse MCP config")
        && managed_path_is_owned(&mcp_path, owned_paths)
    {
        Some(mcp_path)
    } else {
        None
    }
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
