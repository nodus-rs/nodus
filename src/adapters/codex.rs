use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::adapters::{
    ArtifactKind, ManagedArtifactNames, ManagedFile, ManagedHookSpec, managed_artifact_id,
    managed_artifact_path, managed_skill_root,
};
use crate::agent_format::{
    default_codex_agent_description, emitted_codex_agent_toml,
    emitted_codex_agent_toml_from_markdown,
};
use crate::hashing::blake3_hex;
use crate::lockfile::LockedPackage;
use crate::manifest::{AgentEntry, FileEntry, SkillEntry};
use crate::manifest::{HookEvent, HookHandlerType, HookSessionSource, HookTool};
use crate::paths::strip_path_prefix;
use crate::resolver::ResolvedPackage;

pub const SYNTHETIC_COMMAND_SKILL_PREFIX: &str = "__cmd_";
const SYNTHETIC_COMMAND_BODY_MARKER: &str = "<!-- nodus:command-body -->";

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
            crate::adapters::Adapter::Codex,
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
    agent: &AgentEntry,
) -> Result<ManagedFile> {
    let target_path = managed_artifact_path(
        names,
        project_root,
        crate::adapters::Adapter::Codex,
        ArtifactKind::Agent,
        package,
        &agent.id,
    )
    .expect("codex agent path");
    let source_path = snapshot_root.join(&agent.path);
    let source_contents = fs::read(&source_path)
        .with_context(|| format!("failed to read snapshot file {}", source_path.display()))?;
    let managed_name = managed_artifact_id(names, package, ArtifactKind::Agent, &agent.id);
    let contents = if agent.is_toml() {
        let runtime_name = (managed_name != agent.id).then_some(managed_name.as_str());
        emitted_codex_agent_toml(
            &source_contents,
            runtime_name,
            &format!("Codex agent source {}", source_path.display()),
        )?
    } else {
        emitted_codex_agent_toml_from_markdown(
            &source_contents,
            &managed_name,
            &default_codex_agent_description(&agent.id),
            &format!("Codex agent source {}", source_path.display()),
        )?
    };

    Ok(ManagedFile {
        path: target_path,
        contents,
    })
}

pub fn command_skill_file(
    names: &ManagedArtifactNames,
    project_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
    command: &FileEntry,
) -> Result<ManagedFile> {
    let skill_id = synthetic_command_skill_id(names, package, &command.id);
    let source_path = snapshot_root.join(&command.path);
    let source_contents = fs::read(&source_path)
        .with_context(|| format!("failed to read snapshot file {}", source_path.display()))?;
    let contents = emitted_command_skill_markdown(
        &source_contents,
        &skill_id,
        &command.id,
        &format!("Codex command source {}", source_path.display()),
    )?;

    Ok(ManagedFile {
        path: managed_skill_root(
            names,
            project_root,
            crate::adapters::Adapter::Codex,
            package,
            &skill_id,
        )
        .join("SKILL.md"),
        contents,
    })
}

pub fn synthetic_command_skill_id(
    names: &ManagedArtifactNames,
    package: &ResolvedPackage,
    command_id: &str,
) -> String {
    synthetic_command_skill_id_from_managed_command_id(&managed_artifact_id(
        names,
        package,
        ArtifactKind::Command,
        command_id,
    ))
}

pub fn synthetic_locked_command_skill_id(
    names: &ManagedArtifactNames,
    package: &LockedPackage,
    command_id: &str,
) -> String {
    synthetic_command_skill_id_from_managed_command_id(
        &crate::adapters::locked_managed_artifact_id(
            names,
            package,
            ArtifactKind::Command,
            command_id,
        ),
    )
}

pub fn synthetic_command_skill_id_from_managed_command_id(managed_command_id: &str) -> String {
    format!("{SYNTHETIC_COMMAND_SKILL_PREFIX}{managed_command_id}")
}

pub fn source_command_id_from_synthetic_skill_id(skill_id: &str) -> Result<&str> {
    skill_id.strip_prefix(SYNTHETIC_COMMAND_SKILL_PREFIX).ok_or_else(|| {
        anyhow::anyhow!(
            "synthetic Codex command skill id `{skill_id}` must start with `{SYNTHETIC_COMMAND_SKILL_PREFIX}`"
        )
    })
}

pub fn emitted_command_skill_markdown(
    source: &[u8],
    skill_id: &str,
    command_id: &str,
    source_label: &str,
) -> Result<Vec<u8>> {
    source_command_id_from_synthetic_skill_id(skill_id)?;
    let source = std::str::from_utf8(source)
        .with_context(|| format!("{source_label} must be UTF-8 to emit a Codex skill"))?;

    let mut emitted = format!(
        concat!(
            "---\n",
            "name: {skill_id}\n",
            "description: Synthetic Codex compatibility skill generated from the `{command_id}` command.\n",
            "---\n",
            "# {skill_id}\n\n",
            "This skill is generated by Nodus from the `{command_id}` command for Codex compatibility.\n",
            "Edit the command body below to relay changes back to the source command.\n\n",
            "{marker}\n"
        ),
        skill_id = skill_id,
        command_id = command_id,
        marker = SYNTHETIC_COMMAND_BODY_MARKER,
    )
    .into_bytes();
    emitted.extend_from_slice(source.as_bytes());
    Ok(emitted)
}

pub fn command_body_from_synthetic_skill(
    managed: &[u8],
    skill_id: &str,
    source_label: &str,
) -> Result<Vec<u8>> {
    source_command_id_from_synthetic_skill_id(skill_id)?;
    let managed =
        std::str::from_utf8(managed).with_context(|| format!("{source_label} must be UTF-8"))?;
    let Some(marker_start) = managed.find(SYNTHETIC_COMMAND_BODY_MARKER) else {
        bail!("{source_label} is missing the `{SYNTHETIC_COMMAND_BODY_MARKER}` marker");
    };
    let after_marker = &managed[marker_start + SYNTHETIC_COMMAND_BODY_MARKER.len()..];
    let body_start = if after_marker.starts_with("\r\n") {
        2
    } else if after_marker.starts_with('\n') {
        1
    } else if after_marker.is_empty() {
        0
    } else {
        bail!("{source_label} must place `{SYNTHETIC_COMMAND_BODY_MARKER}` on its own line");
    };
    Ok(after_marker.as_bytes()[body_start..].to_vec())
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

pub fn hook_files(project_root: &Path, hooks: &[ManagedHookSpec]) -> Result<Vec<ManagedFile>> {
    let hooks_path = project_root.join(".codex/hooks.json");
    let mut files = hooks
        .iter()
        .map(|hook| ManagedFile {
            path: project_root.join(managed_script_relative_path(hook)),
            contents: hook_script_contents(hook),
        })
        .collect::<Vec<_>>();
    files.push(ManagedFile {
        path: hooks_path.clone(),
        contents: merged_hooks_contents(&hooks_path, hooks)?,
    });
    Ok(files)
}

fn merged_hooks_contents(path: &Path, hooks: &[ManagedHookSpec]) -> Result<Vec<u8>> {
    let mut root = if path.exists() {
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
    for hook in hooks {
        array_field(hooks_object, event_name(hook), path)?.push(hook_entry(hook));
    }

    let mut contents =
        serde_json::to_vec_pretty(&root).context("failed to serialize Codex hooks")?;
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

fn hook_entry(hook: &ManagedHookSpec) -> Value {
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
                        .is_some_and(|command| command.contains("/.codex/hooks/nodus-hook-"))
            })
        })
}

fn managed_hook_command(hook: &ManagedHookSpec) -> String {
    format!(
        r#"sh "$(git rev-parse --show-toplevel 2>/dev/null || pwd)/{}""#,
        managed_script_relative_path(hook)
    )
}

fn managed_script_relative_path(hook: &ManagedHookSpec) -> String {
    format!(".codex/hooks/{}.sh", managed_script_stem(hook))
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

fn event_name(hook: &ManagedHookSpec) -> &'static str {
    match hook.hook.event {
        HookEvent::SessionStart => "SessionStart",
        HookEvent::PreToolUse => "PreToolUse",
        HookEvent::PostToolUse => "PostToolUse",
        HookEvent::Stop => "Stop",
        HookEvent::UserPromptSubmit | HookEvent::SessionEnd => {
            unreachable!("unsupported hook event for Codex")
        }
    }
}

fn matcher_string(hook: &ManagedHookSpec) -> Option<String> {
    match hook.hook.event {
        HookEvent::SessionStart => {
            let matcher = hook
                .hook
                .matcher
                .as_ref()
                .map(|matcher| matcher.sources.as_slice())
                .unwrap_or_default();
            let sources = if matcher.is_empty() {
                vec![HookSessionSource::Startup, HookSessionSource::Resume]
            } else {
                matcher
                    .iter()
                    .copied()
                    .filter(|source| {
                        matches!(
                            source,
                            HookSessionSource::Startup | HookSessionSource::Resume
                        )
                    })
                    .collect::<Vec<_>>()
            };
            (!sources.is_empty()).then(|| {
                sources
                    .into_iter()
                    .map(|source| source.as_str())
                    .collect::<Vec<_>>()
                    .join("|")
            })
        }
        HookEvent::PreToolUse | HookEvent::PostToolUse => {
            let matcher = hook
                .hook
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
        HookEvent::UserPromptSubmit | HookEvent::SessionEnd => {
            unreachable!("unsupported hook event for Codex")
        }
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

project_root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
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
        cwd = shell_quote(match hook.hook.handler.cwd {
            crate::manifest::HookWorkingDirectory::GitRoot => "git_root",
            crate::manifest::HookWorkingDirectory::Session => "session",
        }),
        hook_id = shell_quote(&hook.hook.id),
        hook_event = shell_quote(hook.hook.event.as_str()),
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'"'"'"#))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_command_skill_round_trips_command_body() {
        let managed = emitted_command_skill_markdown(
            b"# Build\ncargo test\n",
            "__cmd_build",
            "build",
            "Codex command source",
        )
        .unwrap();

        assert_eq!(
            command_body_from_synthetic_skill(&managed, "__cmd_build", "Codex command source")
                .unwrap(),
            b"# Build\ncargo test\n"
        );
    }

    #[test]
    fn synthetic_command_skill_requires_reserved_prefix() {
        let error = emitted_command_skill_markdown(
            b"cargo test\n",
            "build",
            "build",
            "Codex command source",
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains(SYNTHETIC_COMMAND_SKILL_PREFIX));
    }
}
