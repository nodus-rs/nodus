use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::adapters::{
    ArtifactKind, ManagedArtifactNames, ManagedFile, ManagedHookSpec,
    effective_session_start_sources, managed_artifact_path, managed_skill_id, managed_skill_root,
};
use crate::agent_format::markdown_from_codex_agent_toml;
use crate::hashing::blake3_hex;
use crate::manifest::SkillEntry;
use crate::manifest::{AgentEntry, HookEvent, HookHandlerType, HookTool};
use crate::paths::strip_path_prefix;
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
        crate::adapters::Adapter::Copilot,
        package,
        &skill.id,
    );
    let mut files = Vec::new();

    for entry in walkdir::WalkDir::new(&source_root) {
        let entry = entry?;
        if entry.file_type().is_file() {
            let relative = entry.path();
            let relative = strip_path_prefix(relative, &source_root)
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
    agent: &AgentEntry,
) -> Result<ManagedFile> {
    let target_path = managed_artifact_path(
        names,
        project_root,
        crate::adapters::Adapter::Copilot,
        ArtifactKind::Agent,
        package,
        &agent.id,
    )
    .expect("copilot agent path");
    let source_path = snapshot_root.join(&agent.path);
    let contents = fs::read(&source_path)
        .with_context(|| format!("failed to read snapshot file {}", source_path.display()))?;
    let contents = if agent.is_toml() {
        markdown_from_codex_agent_toml(
            &contents,
            &format!("GitHub Copilot agent source {}", source_path.display()),
        )?
    } else {
        contents
    };
    Ok(ManagedFile {
        path: target_path,
        contents,
    })
}

pub fn hook_files(project_root: &Path, hooks: &[ManagedHookSpec]) -> Result<Vec<ManagedFile>> {
    let hooks_path = project_root.join(".github/hooks/nodus-hooks.json");
    let mut files = hooks
        .iter()
        .map(|hook| ManagedFile {
            path: project_root.join(managed_script_relative_path(hook)),
            contents: hook_script_contents(hook),
        })
        .collect::<Vec<_>>();
    files.push(ManagedFile {
        path: hooks_path,
        contents: hooks_config_contents(hooks)?,
    });
    Ok(files)
}

fn hooks_config_contents(hooks: &[ManagedHookSpec]) -> Result<Vec<u8>> {
    let mut hooks_object = Map::new();
    for hook in hooks {
        let event = event_name(hook);
        hooks_object
            .entry(event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("hook event entry is always an array")
            .push(hook_entry(hook));
    }

    let mut root = Map::new();
    root.insert("version".into(), json!(1));
    root.insert("hooks".into(), Value::Object(hooks_object));

    let mut contents = serde_json::to_vec_pretty(&Value::Object(root))
        .context("failed to serialize GitHub Copilot hooks")?;
    contents.push(b'\n');
    Ok(contents)
}

fn hook_entry(hook: &ManagedHookSpec) -> Value {
    let mut entry = Map::new();
    entry.insert("type".into(), Value::String("command".into()));
    entry.insert(
        "bash".into(),
        Value::String(format!("./{}", managed_script_relative_path(hook))),
    );
    entry.insert("cwd".into(), Value::String(".".into()));
    if let Some(timeout_sec) = hook.hook.timeout_sec {
        entry.insert("timeoutSec".into(), Value::Number(timeout_sec.into()));
    }
    Value::Object(entry)
}

fn event_name(hook: &ManagedHookSpec) -> &'static str {
    match hook.hook.event {
        HookEvent::SessionStart => "sessionStart",
        HookEvent::UserPromptSubmit => "userPromptSubmitted",
        HookEvent::PreToolUse => "preToolUse",
        HookEvent::PostToolUse => "postToolUse",
        HookEvent::Stop => "agentStop",
        HookEvent::SubagentStop => "subagentStop",
        HookEvent::SessionEnd => "sessionEnd",
        HookEvent::PermissionRequest => unreachable!("unsupported hook event for GitHub Copilot"),
    }
}

fn hook_script_contents(hook: &ManagedHookSpec) -> Vec<u8> {
    debug_assert!(matches!(
        hook.hook.handler.handler_type,
        HookHandlerType::Command
    ));
    format!(
        r#"#!/bin/sh
set -eu

input="$(cat)"

json_string_field() {{
  printf '%s' "$input" | sed -n "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" | head -n 1
}}

if [ {hook_event} = "session_start" ]; then
  source="$(json_string_field source)"
  case "$source" in
    new|startup) nodus_source="startup" ;;
    resume) nodus_source="resume" ;;
    *) exit 0 ;;
  esac
  case {session_sources} in
    *" $nodus_source "*) ;;
    *) exit 0 ;;
  esac
fi

{tool_filter}
project_root="$(json_string_field cwd)"
if [ -z "$project_root" ]; then
  project_root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
fi
if [ {cwd} = "git_root" ]; then
  cd "$project_root"
fi

export NODUS_HOOK_ID={hook_id}
export NODUS_HOOK_EVENT={hook_event}
{timeout_export}
run_nodus_hook() {{
  printf '%s' "$input" | sh -lc {command}
}}

if [ {blocking} = "true" ]; then
  run_nodus_hook
  exit $?
fi

if ! run_nodus_hook; then
  echo "nodus hook {hook_label} failed" >&2
fi
"#,
        hook_event = shell_quote(hook.hook.event.as_str()),
        session_sources = shell_quote(&format!(
            " {} ",
            effective_session_start_sources(&hook.hook, crate::adapters::Adapter::Copilot)
                .into_iter()
                .map(|source| source.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        )),
        tool_filter = tool_filter_script(hook),
        cwd = shell_quote(match hook.hook.handler.cwd {
            crate::manifest::HookWorkingDirectory::GitRoot => "git_root",
            crate::manifest::HookWorkingDirectory::Session => "session",
        }),
        hook_id = shell_quote(&hook.hook.id),
        timeout_export = hook
            .hook
            .timeout_sec
            .map(|timeout_sec| format!(
                "export NODUS_HOOK_TIMEOUT_SEC={}\n",
                shell_quote(&timeout_sec.to_string())
            ))
            .unwrap_or_default(),
        blocking = shell_quote(if hook.hook.blocking { "true" } else { "false" }),
        command = shell_quote(&hook.hook.handler.command),
        hook_label = hook.hook.id,
    )
    .into_bytes()
}

fn tool_filter_script(hook: &ManagedHookSpec) -> String {
    if !matches!(
        hook.hook.event,
        HookEvent::PreToolUse | HookEvent::PostToolUse
    ) {
        return String::new();
    }

    let tool_names = hook
        .hook
        .matcher
        .as_ref()
        .map(|matcher| matcher.tool_names.as_slice())
        .unwrap_or_default();
    if tool_names.is_empty() {
        return String::new();
    }

    let values = tool_names
        .iter()
        .map(|tool_name| match tool_name {
            HookTool::Bash => "bash",
        })
        .collect::<Vec<_>>()
        .join(" ");

    format!(
        r#"tool_name="$(json_string_field toolName | tr '[:upper:]' '[:lower:]')"
case {values} in
  *" $tool_name "*) ;;
  *) exit 0 ;;
esac
"#,
        values = shell_quote(&format!(" {values} ")),
    )
}

fn managed_script_relative_path(hook: &ManagedHookSpec) -> String {
    format!(".github/hooks/{}.sh", managed_script_stem(hook))
}

fn managed_script_stem(hook: &ManagedHookSpec) -> String {
    let sanitized = hook
        .hook
        .id
        .chars()
        .map(|character| match character {
            'a'..='z' | '0'..='9' => character,
            'A'..='Z' => character.to_ascii_lowercase(),
            _ => '-',
        })
        .collect::<String>();
    if hook.emitted_from_root {
        format!(
            "nodus-hook-{sanitized}-{}",
            &blake3_hex(hook.hook.id.as_bytes())[..8]
        )
    } else {
        let package = hook
            .package_alias
            .chars()
            .map(|character| match character {
                'a'..='z' | '0'..='9' => character,
                'A'..='Z' => character.to_ascii_lowercase(),
                _ => '-',
            })
            .collect::<String>();
        format!(
            "nodus-hook-{package}-{sanitized}-{}",
            &blake3_hex(format!("{}:{}", hook.package_alias, hook.hook.id).as_bytes())[..8]
        )
    }
}

pub(crate) fn rewrite_skill_name(contents: &[u8], skill_id: &str) -> Result<Vec<u8>> {
    let contents =
        String::from_utf8(contents.to_vec()).context("GitHub Copilot skills must be UTF-8")?;
    let mut lines = split_lines_preserving_endings(&contents);

    if lines.first().map(|line| trim_line_ending(line)) != Some("---") {
        bail!(
            "GitHub Copilot skill {} is missing YAML frontmatter",
            skill_id
        );
    }

    let Some(frontmatter_end) = lines
        .iter()
        .skip(1)
        .position(|line| trim_line_ending(line) == "---")
    else {
        bail!(
            "GitHub Copilot skill {} is missing a closing frontmatter fence",
            skill_id
        );
    };
    let frontmatter_end = frontmatter_end + 1;

    if let Some(name_index) = lines
        .iter()
        .take(frontmatter_end)
        .position(|line| trim_line_ending(line).trim_start().starts_with("name:"))
    {
        lines[name_index] = rewrite_frontmatter_name_line(&lines[name_index], skill_id);
    } else {
        lines.insert(
            frontmatter_end,
            inserted_frontmatter_name_line(&lines, frontmatter_end, skill_id),
        );
    }
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

fn inserted_frontmatter_name_line(lines: &[String], frontmatter_end: usize, name: &str) -> String {
    format!(
        "name: {name}{}",
        preferred_line_ending(lines, frontmatter_end)
    )
}

fn preferred_line_ending(lines: &[String], anchor: usize) -> &str {
    line_ending(lines.get(anchor).map(String::as_str).unwrap_or_default())
        .or_else(|| {
            anchor
                .checked_sub(1)
                .and_then(|index| lines.get(index))
                .and_then(|line| line_ending(line))
        })
        .unwrap_or("\n")
}

fn line_ending(line: &str) -> Option<&str> {
    if line.ends_with("\r\n") {
        Some("\r\n")
    } else if line.ends_with('\n') {
        Some("\n")
    } else {
        None
    }
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'"'"'"#))
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

    #[test]
    fn inserts_missing_skill_name_into_frontmatter() {
        let contents = b"---\ndescription: Example\n---\n# Review\n".as_slice();
        let rewritten = rewrite_skill_name(contents, "review").unwrap();
        let rewritten = String::from_utf8(rewritten).unwrap();

        assert!(rewritten.contains("name: review\n"));
        assert!(rewritten.contains("description: Example\n"));
    }
}
