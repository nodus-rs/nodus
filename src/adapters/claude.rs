use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::adapters::{ManagedFile, namespaced_file_name, namespaced_skill_id};
use crate::manifest::{FileEntry, SkillEntry};
use crate::resolver::ResolvedPackage;

pub fn skill_files(
    project_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
    skill: &SkillEntry,
) -> Result<Vec<ManagedFile>> {
    copy_directory(
        project_root
            .join(".claude/skills")
            .join(namespaced_skill_id(package, &skill.id)),
        snapshot_root.join(&skill.path),
    )
}

pub fn agent_file(
    project_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
    agent: &FileEntry,
) -> Result<ManagedFile> {
    copy_file(
        project_root
            .join(".claude/agents")
            .join(namespaced_file_name(package, &agent.id, "md")),
        snapshot_root.join(&agent.path),
    )
}

pub fn command_file(
    project_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
    command: &FileEntry,
) -> Result<ManagedFile> {
    copy_file(
        project_root
            .join(".claude/commands")
            .join(namespaced_file_name(package, &command.id, "md")),
        snapshot_root.join(&command.path),
    )
}

pub fn rule_file(
    project_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
    rule: &FileEntry,
) -> Result<ManagedFile> {
    copy_file(
        project_root
            .join(".claude/rules")
            .join(namespaced_file_name(package, &rule.id, "md")),
        snapshot_root.join(&rule.path),
    )
}

fn copy_directory(
    target_root: impl AsRef<Path>,
    source_root: impl AsRef<Path>,
) -> Result<Vec<ManagedFile>> {
    let target_root = target_root.as_ref();
    let source_root = source_root.as_ref();
    let mut files = Vec::new();

    for entry in walkdir::WalkDir::new(source_root) {
        let entry = entry?;
        if entry.file_type().is_file() {
            let relative = entry
                .path()
                .strip_prefix(source_root)
                .with_context(|| format!("failed to make {} relative", entry.path().display()))?;
            files.push(ManagedFile {
                path: target_root.join(relative),
                contents: fs::read(entry.path()).with_context(|| {
                    format!("failed to read snapshot file {}", entry.path().display())
                })?,
            });
        }
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn copy_file(target_path: impl AsRef<Path>, source_path: impl AsRef<Path>) -> Result<ManagedFile> {
    let target_path = target_path.as_ref();
    let source_path = source_path.as_ref();
    Ok(ManagedFile {
        path: target_path.to_path_buf(),
        contents: fs::read(source_path)
            .with_context(|| format!("failed to read snapshot file {}", source_path.display()))?,
    })
}
