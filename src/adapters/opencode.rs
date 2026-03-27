use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::adapters::{
    ArtifactKind, ManagedArtifactNames, ManagedFile, managed_artifact_path, managed_skill_id,
    managed_skill_root,
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
    let source_root = snapshot_root.join(&skill.path);
    let managed_skill_id = managed_skill_id(names, package, &skill.id);
    let target_root = managed_skill_root(
        names,
        project_root,
        crate::adapters::Adapter::OpenCode,
        package,
        &skill.id,
    );
    let mut files = Vec::new();

    for entry in walkdir::WalkDir::new(&source_root) {
        let entry = entry?;
        if entry.file_type().is_file() {
            let relative = entry
                .path()
                .strip_prefix(&source_root)
                .with_context(|| format!("failed to make {} relative", entry.path().display()))?;
            let contents = fs::read(entry.path()).with_context(|| {
                format!("failed to read snapshot file {}", entry.path().display())
            })?;
            let contents = if relative == Path::new("SKILL.md") {
                rewrite_skill_name(&contents, &managed_skill_id)?
            } else {
                contents
            };
            files.push(ManagedFile {
                path: target_root.join(relative),
                contents,
            });
        }
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
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
            crate::adapters::Adapter::OpenCode,
            ArtifactKind::Agent,
            package,
            &agent.id,
        )
        .expect("opencode agent path"),
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
            crate::adapters::Adapter::OpenCode,
            ArtifactKind::Command,
            package,
            &command.id,
        )
        .expect("opencode command path"),
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
            crate::adapters::Adapter::OpenCode,
            ArtifactKind::Rule,
            package,
            &rule.id,
        )
        .expect("opencode rule path"),
        snapshot_root.join(&rule.path),
    )
}

pub fn sync_on_startup_files(project_root: &Path) -> Vec<ManagedFile> {
    vec![
        ManagedFile {
            path: project_root.join(".opencode/scripts/nodus-sync.sh"),
            contents: sync_script_contents(),
        },
        ManagedFile {
            path: project_root.join(".opencode/plugins/nodus-sync.js"),
            contents:
                br#"export default async function nodusSyncPlugin({ $, directory, worktree }) {
  const root = worktree ?? directory;

  try {
    await $`sh ${`${root}/.opencode/scripts/nodus-sync.sh`}`;
  } catch (error) {
    console.error("nodus sync hook failed", error);
  }

  return {};
}
"#
                .to_vec(),
        },
    ]
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

pub(crate) fn rewrite_skill_name(contents: &[u8], skill_id: &str) -> Result<Vec<u8>> {
    let contents = String::from_utf8(contents.to_vec()).context("OpenCode skills must be UTF-8")?;
    let mut lines = split_lines_preserving_endings(&contents);

    if lines.first().map(|line| trim_line_ending(line)) != Some("---") {
        bail!("OpenCode skill {} is missing YAML frontmatter", skill_id);
    }

    let Some(frontmatter_end) = lines
        .iter()
        .skip(1)
        .position(|line| trim_line_ending(line) == "---")
    else {
        bail!(
            "OpenCode skill {} is missing a closing frontmatter fence",
            skill_id
        );
    };
    let frontmatter_end = frontmatter_end + 1;

    let Some(name_index) = lines
        .iter()
        .take(frontmatter_end)
        .position(|line| trim_line_ending(line).trim_start().starts_with("name:"))
    else {
        bail!(
            "OpenCode skill {} is missing a frontmatter `name`",
            skill_id
        );
    };

    lines[name_index] = rewrite_frontmatter_name_line(&lines[name_index], skill_id);
    Ok(lines.concat().into_bytes())
}

fn split_lines_preserving_endings(contents: &str) -> Vec<String> {
    if contents.is_empty() {
        Vec::new()
    } else {
        contents.split_inclusive('\n').map(str::to_string).collect()
    }
}

fn trim_line_ending(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

fn rewrite_frontmatter_name_line(line: &str, name: &str) -> String {
    let leading = line
        .chars()
        .take_while(|character| character.is_ascii_whitespace())
        .collect::<String>();
    let newline = if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    };

    format!("{leading}name: {name}{newline}")
}

fn sync_script_contents() -> Vec<u8> {
    br#"#!/bin/sh
set -eu

project_root="${1:-$(pwd)}"

if ! command -v nodus >/dev/null 2>&1; then
  echo "nodus not found on PATH; skipping startup sync" >&2
  exit 0
fi

cd "$project_root"
if ! nodus sync >/dev/null 2>&1; then
  echo "nodus sync failed in $project_root" >&2
fi
"#
    .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_skill_name_to_match_runtime_id() {
        let contents = b"---\nname: Review\ndescription: Example\n---\n# Review\n".as_slice();
        let rewritten = rewrite_skill_name(contents, "review").unwrap();
        let rewritten = String::from_utf8(rewritten).unwrap();
        assert!(rewritten.contains("name: review"));
        assert!(rewritten.contains("description: Example"));
        assert!(rewritten.ends_with('\n'));
    }

    #[test]
    fn preserves_crlf_when_rewriting_skill_name() {
        let contents =
            b"---\r\nname: Review\r\ndescription: Example\r\n---\r\n# Review\r\n".as_slice();
        let rewritten = rewrite_skill_name(contents, "review").unwrap();
        let rewritten = String::from_utf8(rewritten).unwrap();

        assert!(rewritten.contains("name: review\r\n"));
        assert!(rewritten.contains("description: Example\r\n"));
        assert!(rewritten.ends_with("\r\n"));
    }
}
