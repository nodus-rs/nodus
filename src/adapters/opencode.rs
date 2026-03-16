use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::adapters::{ManagedFile, namespaced_file_name, namespaced_skill_id};
use crate::manifest::{FileEntry, SkillEntry};
use crate::resolver::ResolvedPackage;

pub fn skill_files(
    project_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
    skill: &SkillEntry,
) -> Result<Vec<ManagedFile>> {
    let source_root = snapshot_root.join(&skill.path);
    let managed_skill_id = namespaced_skill_id(package, &skill.id);
    let target_root = project_root
        .join(".opencode/skills")
        .join(&managed_skill_id);
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
    project_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
    agent: &FileEntry,
) -> Result<ManagedFile> {
    copy_file(
        project_root
            .join(".opencode/agents")
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
            .join(".opencode/commands")
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
            .join(".opencode/rules")
            .join(namespaced_file_name(package, &rule.id, "md")),
        snapshot_root.join(&rule.path),
    )
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

fn rewrite_skill_name(contents: &[u8], skill_id: &str) -> Result<Vec<u8>> {
    let contents = String::from_utf8(contents.to_vec()).context("OpenCode skills must be UTF-8")?;
    let original_has_trailing_newline = contents.ends_with('\n');
    let mut lines = contents.lines().map(str::to_string).collect::<Vec<_>>();

    if lines.first().map(String::as_str) != Some("---") {
        bail!("OpenCode skill {} is missing YAML frontmatter", skill_id);
    }

    let Some(frontmatter_end) = lines.iter().skip(1).position(|line| line == "---") else {
        bail!(
            "OpenCode skill {} is missing a closing frontmatter fence",
            skill_id
        );
    };
    let frontmatter_end = frontmatter_end + 1;

    let Some(name_index) = lines
        .iter()
        .take(frontmatter_end)
        .position(|line| line.trim_start().starts_with("name:"))
    else {
        bail!(
            "OpenCode skill {} is missing a frontmatter `name`",
            skill_id
        );
    };

    lines[name_index] = format!("name: {}", skill_id);

    let mut rewritten = lines.join("\n");
    if original_has_trailing_newline {
        rewritten.push('\n');
    }
    Ok(rewritten.into_bytes())
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
}
