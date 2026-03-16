use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::resolver::{PackageSource, ResolvedPackage};

pub mod claude;
pub mod codex;
pub mod opencode;

#[derive(Debug, Clone)]
pub struct ManagedFile {
    pub path: PathBuf,
    pub contents: Vec<u8>,
}

pub fn namespaced_skill_id(package: &ResolvedPackage, skill_id: &str) -> String {
    format!("{skill_id}_{}", package_short_id(package))
}

fn package_short_id(package: &ResolvedPackage) -> String {
    match &package.source {
        PackageSource::Git { rev, .. } => short_source_id(rev),
        PackageSource::Path { .. } | PackageSource::Root => short_source_id(
            package
                .digest
                .strip_prefix("sha256:")
                .unwrap_or(&package.digest),
        ),
    }
}

fn short_source_id(value: &str) -> String {
    let short = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(6)
        .collect::<String>()
        .to_ascii_lowercase();

    if short.is_empty() {
        "local0".into()
    } else {
        short
    }
}

pub fn build_managed_files(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
) -> Result<Vec<ManagedFile>> {
    let mut planned = BTreeMap::<PathBuf, Vec<u8>>::new();
    let mut opencode_instructions = Vec::new();

    for (package, snapshot_root) in packages {
        merge_files(
            &mut planned,
            claude::managed_files(project_root, package, snapshot_root)?,
        )?;
        merge_files(
            &mut planned,
            codex::managed_files(project_root, package, snapshot_root)?,
        )?;
        let open_code = opencode::managed_files(project_root, package, snapshot_root)?;
        opencode_instructions.extend(open_code.instructions);
        merge_files(&mut planned, open_code.files)?;
    }

    if !opencode_instructions.is_empty() {
        opencode_instructions.sort();
        opencode_instructions.dedup();
        planned.insert(
            project_root.join("opencode.json"),
            opencode::render_config(&opencode_instructions)
                .context("failed to render OpenCode config")?,
        );
    }

    Ok(planned
        .into_iter()
        .map(|(path, contents)| ManagedFile { path, contents })
        .collect())
}

fn merge_files(target: &mut BTreeMap<PathBuf, Vec<u8>>, files: Vec<ManagedFile>) -> Result<()> {
    for file in files {
        match target.get(&file.path) {
            Some(existing) if existing != &file.contents => {
                bail!("multiple packages want to manage {}", file.path.display());
            }
            Some(_) => {}
            None => {
                target.insert(file.path, file.contents);
            }
        }
    }
    Ok(())
}
