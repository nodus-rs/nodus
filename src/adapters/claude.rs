use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::adapters::{
    ArtifactKind, ManagedArtifactNames, ManagedFile, managed_artifact_path, managed_skill_root,
};
use crate::manifest::{FileEntry, SkillEntry};
use crate::resolver::ResolvedPackage;

pub fn skill_files(
    names: &ManagedArtifactNames,
    project_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
    skill: &SkillEntry,
) -> Result<Vec<ManagedFile>> {
    copy_directory(
        managed_skill_root(
            names,
            project_root,
            crate::adapters::Adapter::Claude,
            package,
            &skill.id,
        ),
        snapshot_root.join(&skill.path),
    )
}

pub fn agent_file(
    names: &ManagedArtifactNames,
    project_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
    agent: &FileEntry,
) -> Result<ManagedFile> {
    copy_file(
        managed_artifact_path(
            names,
            project_root,
            crate::adapters::Adapter::Claude,
            ArtifactKind::Agent,
            package,
            &agent.id,
        )
        .expect("claude agent path"),
        snapshot_root.join(&agent.path),
    )
}

pub fn command_file(
    names: &ManagedArtifactNames,
    project_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
    command: &FileEntry,
) -> Result<ManagedFile> {
    copy_file(
        managed_artifact_path(
            names,
            project_root,
            crate::adapters::Adapter::Claude,
            ArtifactKind::Command,
            package,
            &command.id,
        )
        .expect("claude command path"),
        snapshot_root.join(&command.path),
    )
}

pub fn rule_file(
    names: &ManagedArtifactNames,
    project_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
    rule: &FileEntry,
) -> Result<ManagedFile> {
    copy_file(
        managed_artifact_path(
            names,
            project_root,
            crate::adapters::Adapter::Claude,
            ArtifactKind::Rule,
            package,
            &rule.id,
        )
        .expect("claude rule path"),
        snapshot_root.join(&rule.path),
    )
}

pub fn sync_on_startup_files(project_root: &Path) -> Vec<ManagedFile> {
    vec![
        ManagedFile {
            path: project_root.join(".claude/hooks/nodus-sync.sh"),
            contents: sync_script_contents("CLAUDE_PROJECT_DIR"),
        },
        ManagedFile {
            path: project_root.join(".claude/settings.local.json"),
            contents: br#"{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup",
        "hooks": [
          {
            "type": "command",
            "command": "./.claude/hooks/nodus-sync.sh"
          }
        ]
      }
    ]
  }
}
"#
            .to_vec(),
        },
    ]
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

fn sync_script_contents(project_dir_env: &str) -> Vec<u8> {
    format!(
        r#"#!/bin/sh
set -eu

project_root="${{{project_dir_env}:-$(pwd)}}"

if ! command -v nodus >/dev/null 2>&1; then
  echo "nodus not found on PATH; skipping startup sync" >&2
  exit 0
fi

cd "$project_root"
if ! nodus sync >/dev/null 2>&1; then
  echo "nodus sync failed in $project_root" >&2
fi
"#
    )
    .into_bytes()
}
