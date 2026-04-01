use std::path::Path;

use anyhow::{Result, bail};
use rayon::prelude::*;
use serde::Serialize;

use super::resolve::{resolve_project, validate_git_package};
use super::support::{
    find_managed_collision, find_unmanaged_collision, load_owned_paths,
    unmanaged_collision_guidance, validate_state_consistency,
};
use super::{ResolveMode, Resolution, SyncMode, lockfile_out_of_date_message};
use crate::adapters::{Adapters, build_output_plan};
use crate::lockfile::{LOCKFILE_NAME, Lockfile};
use crate::manifest::load_root_from_dir;
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
}

#[derive(Debug, Clone)]
struct DoctorPlan {
    package_count: usize,
    warnings: Vec<String>,
    findings: Vec<DoctorFinding>,
    applied_actions: Vec<DoctorActionRecord>,
}

pub fn doctor_in_dir_with_mode(
    cwd: &Path,
    cache_root: &Path,
    mode: DoctorMode,
    reporter: &Reporter,
) -> Result<DoctorSummary> {
    let inspection = inspect_doctor_state(cwd, cache_root, reporter)?;
    let plan = build_doctor_plan(&inspection)?;
    execute_doctor_plan(plan, mode, reporter)
}

fn inspect_doctor_state(cwd: &Path, cache_root: &Path, reporter: &Reporter) -> Result<DoctorInspection> {
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
    let output_plan = build_output_plan(
        cwd,
        &package_roots,
        selected_adapters,
        existing_lockfile.as_ref(),
        true,
    )?;
    let desired_paths = resolution.managed_paths(cwd, selected_adapters)?;
    let mut warnings = resolution
        .warnings
        .iter()
        .chain(output_plan.warnings.iter())
        .cloned()
        .collect::<Vec<_>>();
    warnings.sort();
    warnings.dedup();

    let mut findings = Vec::new();
    inspect_lockfile_state(
        cwd,
        &resolution,
        selected_adapters,
        &lockfile_path,
        existing_lockfile.as_ref(),
        &output_plan.files,
        &desired_paths,
        &mut findings,
    )?;

    Ok(DoctorInspection {
        package_count: resolution.packages.len(),
        warnings,
        findings,
    })
}

fn inspect_lockfile_state(
    cwd: &Path,
    resolution: &Resolution,
    selected_adapters: Adapters,
    lockfile_path: &Path,
    existing_lockfile: Option<&Lockfile>,
    planned_files: &[crate::adapters::ManagedFile],
    desired_paths: &std::collections::HashSet<std::path::PathBuf>,
    findings: &mut Vec<DoctorFinding>,
) -> Result<()> {
    let Some(existing_lockfile) = existing_lockfile else {
        findings.push(DoctorFinding {
            kind: DoctorFindingKind::SafeAutoFix,
            message: format!("missing {}", lockfile_path.file_name().unwrap().to_string_lossy()),
        });
        return Ok(());
    };

    let expected_lockfile = resolution.to_lockfile(selected_adapters, cwd)?;
    if *existing_lockfile != expected_lockfile {
        findings.push(DoctorFinding {
            kind: DoctorFindingKind::SafeAutoFix,
            message: lockfile_out_of_date_message(),
        });
    }

    let owned_paths = load_owned_paths(cwd, Some(existing_lockfile))?;
    if let Some(collision) = find_unmanaged_collision(planned_files, &owned_paths, cwd) {
        let message = if let Some(managed_collision) = find_managed_collision(cwd, resolution, &collision) {
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

    if let Err(error) = validate_state_consistency(&owned_paths, desired_paths, planned_files) {
        findings.push(classify_state_consistency_finding(error.to_string()));
    }

    Ok(())
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

fn build_doctor_plan(inspection: &DoctorInspection) -> Result<DoctorPlan> {
    Ok(DoctorPlan {
        package_count: inspection.package_count,
        warnings: inspection.warnings.clone(),
        findings: inspection.findings.clone(),
        applied_actions: Vec::new(),
    })
}

fn execute_doctor_plan(
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
            if let Some(finding) = plan.findings.first() {
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
