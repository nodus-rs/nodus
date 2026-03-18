use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::discover::{default_manifest_contents, default_skill_contents};
use super::{InitSummary, MANIFEST_FILE};
use crate::execution::{ExecutionMode, PreviewChange};
use crate::report::Reporter;

pub fn scaffold_init_in_dir(root: &Path, reporter: &Reporter) -> Result<InitSummary> {
    scaffold_init_in_dir_mode(root, ExecutionMode::Apply, reporter)
}

pub fn scaffold_init_in_dir_dry_run(root: &Path, reporter: &Reporter) -> Result<InitSummary> {
    scaffold_init_in_dir_mode(root, ExecutionMode::DryRun, reporter)
}

fn scaffold_init_in_dir_mode(
    root: &Path,
    execution_mode: ExecutionMode,
    reporter: &Reporter,
) -> Result<InitSummary> {
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to access {}", root.display()))?;
    let manifest_path = root.join(MANIFEST_FILE);
    if manifest_path.exists() {
        bail!("{} already exists", manifest_path.display());
    }

    let skill_dir = root.join("skills").join("example");
    let skill_file = skill_dir.join("SKILL.md");
    if skill_file.exists() {
        bail!("{} already exists", skill_file.display());
    }

    if execution_mode.is_dry_run() {
        reporter.preview(&PreviewChange::Create(manifest_path.clone()))?;
        reporter.preview(&PreviewChange::Create(skill_file.clone()))?;
    } else {
        fs::create_dir_all(&skill_dir)
            .with_context(|| format!("failed to create {}", skill_dir.display()))?;
        reporter.status("Creating", manifest_path.display())?;
        crate::store::write_atomic(&manifest_path, default_manifest_contents().as_bytes())?;
        reporter.status("Creating", skill_file.display())?;
        crate::store::write_atomic(&skill_file, default_skill_contents().as_bytes())?;
    }

    Ok(InitSummary {
        created_paths: vec![manifest_path, skill_file],
    })
}
