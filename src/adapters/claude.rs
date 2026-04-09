use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use crate::adapters::{
    ArtifactKind, ManagedArtifactNames, ManagedFile, managed_artifact_path, managed_skill_root,
};
use crate::hashing::blake3_hex;
use crate::manifest::{FileEntry, SkillEntry};
use crate::manifest::{HookEvent, HookHandlerType, HookSessionSource, HookSpec, HookTool};
use crate::paths::strip_path_prefix;
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

pub fn hook_files(
    project_root: &Path,
    hooks: &[HookSpec],
    merge_existing: bool,
) -> Result<Vec<ManagedFile>> {
    let settings_path = project_root.join(".claude/settings.json");
    let mut files = hooks
        .iter()
        .map(|hook| ManagedFile {
            path: project_root.join(managed_script_relative_path(hook)),
            contents: hook_script_contents(hook),
        })
        .collect::<Vec<_>>();
    files.push(ManagedFile {
        path: settings_path.clone(),
        contents: settings_contents(&settings_path, merge_existing, hooks)?,
    });
    Ok(files)
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
            let relative = entry.path();
            let relative = strip_path_prefix(relative, source_root)
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

fn hook_script_contents(hook: &HookSpec) -> Vec<u8> {
    debug_assert!(matches!(
        hook.handler.handler_type,
        HookHandlerType::Command
    ));
    format!(
        r#"#!/bin/sh
set -eu

project_root="${{CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}}"
if [ {cwd} = "git_root" ]; then
  cd "$project_root"
fi

export NODUS_HOOK_ID={hook_id}
export NODUS_HOOK_EVENT={hook_event}
{timeout_export}
if [ {blocking} = "true" ]; then
  exec sh -lc {command}
fi

if ! sh -lc {command}; then
  echo "nodus hook {hook_label} failed" >&2
fi
"#,
        cwd = shell_quote(match hook.handler.cwd {
            crate::manifest::HookWorkingDirectory::GitRoot => "git_root",
            crate::manifest::HookWorkingDirectory::Session => "session",
        }),
        hook_id = shell_quote(&hook.id),
        hook_event = shell_quote(hook.event.as_str()),
        timeout_export = hook
            .timeout_sec
            .map(|timeout_sec| format!(
                "export NODUS_HOOK_TIMEOUT_SEC={}\n",
                shell_quote(&timeout_sec.to_string())
            ))
            .unwrap_or_default(),
        blocking = shell_quote(if hook.blocking { "true" } else { "false" }),
        command = shell_quote(&hook.handler.command),
        hook_label = hook.id,
    )
    .into_bytes()
}

fn settings_contents(
    path: &Path,
    merge_existing: bool,
    hook_specs: &[HookSpec],
) -> Result<Vec<u8>> {
    let mut root = if merge_existing && path.exists() {
        serde_json::from_slice::<Value>(
            &fs::read(path)
                .with_context(|| format!("failed to read existing {}", path.display()))?,
        )
        .with_context(|| format!("failed to parse existing {}", path.display()))?
    } else {
        Value::Object(Map::new())
    };

    let root_object = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} must contain a JSON object", path.display()))?;
    let hooks_object = object_field(root_object, "hooks", path)?;
    remove_managed_hook_entries(hooks_object);
    for hook in hook_specs {
        array_field(hooks_object, event_name(hook), path)?.push(hook_entry(hook));
    }

    let mut contents =
        serde_json::to_vec_pretty(&root).context("failed to serialize Claude settings")?;
    contents.push(b'\n');
    Ok(contents)
}

fn object_field<'a>(
    object: &'a mut Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<&'a mut Map<String, Value>> {
    let value = object
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    value.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "{} field `{key}` must contain a JSON object",
            path.display()
        )
    })
}

fn array_field<'a>(
    object: &'a mut Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<&'a mut Vec<Value>> {
    let value = object
        .entry(key.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    value.as_array_mut().ok_or_else(|| {
        anyhow::anyhow!("{} field `{key}` must contain a JSON array", path.display())
    })
}

fn hook_entry(hook: &HookSpec) -> Value {
    let hook_value = json!({
        "type": "command",
        "command": managed_hook_command(hook),
    });
    if let Some(matcher) = matcher_string(hook) {
        json!({
            "matcher": matcher,
            "hooks": [hook_value],
        })
    } else {
        json!({
            "hooks": [hook_value],
        })
    }
}

fn remove_managed_hook_entries(hooks: &mut Map<String, Value>) {
    for event in ["SessionStart", "PreToolUse", "PostToolUse", "Stop"] {
        let Some(entries) = hooks.get_mut(event).and_then(Value::as_array_mut) else {
            continue;
        };
        entries.retain(|entry| !entry_is_managed(entry));
    }
}

fn entry_is_managed(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("type").and_then(Value::as_str) == Some("command")
                    && hook
                        .get("command")
                        .and_then(Value::as_str)
                        .is_some_and(|command| command.starts_with("./.claude/hooks/nodus-hook-"))
            })
        })
}

fn managed_hook_command(hook: &HookSpec) -> String {
    format!("./{}", managed_script_relative_path(hook))
}

fn managed_script_relative_path(hook: &HookSpec) -> String {
    format!(".claude/hooks/{}.sh", managed_script_stem(hook))
}

fn managed_script_stem(hook: &HookSpec) -> String {
    let sanitized = hook
        .id
        .chars()
        .map(|character| match character {
            'a'..='z' | '0'..='9' => character,
            'A'..='Z' => character.to_ascii_lowercase(),
            _ => '-',
        })
        .collect::<String>();
    format!(
        "nodus-hook-{sanitized}-{}",
        &blake3_hex(hook.id.as_bytes())[..8]
    )
}

fn event_name(hook: &HookSpec) -> &'static str {
    match hook.event {
        HookEvent::SessionStart => "SessionStart",
        HookEvent::PreToolUse => "PreToolUse",
        HookEvent::PostToolUse => "PostToolUse",
        HookEvent::Stop => "Stop",
    }
}

fn matcher_string(hook: &HookSpec) -> Option<String> {
    match hook.event {
        HookEvent::SessionStart => {
            let matcher = hook
                .matcher
                .as_ref()
                .map(|matcher| matcher.sources.as_slice())
                .unwrap_or_default();
            let sources = if matcher.is_empty() {
                vec![HookSessionSource::Startup, HookSessionSource::Resume]
            } else {
                matcher.to_vec()
            };
            Some(
                sources
                    .into_iter()
                    .map(|source| source.as_str())
                    .collect::<Vec<_>>()
                    .join("|"),
            )
        }
        HookEvent::PreToolUse | HookEvent::PostToolUse => {
            let matcher = hook
                .matcher
                .as_ref()
                .map(|matcher| matcher.tool_names.as_slice())
                .unwrap_or_default();
            if matcher.is_empty() {
                Some("*".to_string())
            } else {
                Some(
                    matcher
                        .iter()
                        .map(|tool_name| match tool_name {
                            HookTool::Bash => "Bash",
                        })
                        .collect::<Vec<_>>()
                        .join("|"),
                )
            }
        }
        HookEvent::Stop => None,
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'"'"'"#))
}
