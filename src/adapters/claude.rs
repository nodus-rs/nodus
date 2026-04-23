use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use crate::adapters::{
    ArtifactKind, ManagedArtifactNames, ManagedFile, ManagedHookSpec, managed_artifact_path,
    managed_skill_root,
};
use crate::agent_format::markdown_from_codex_agent_toml;
use crate::hashing::blake3_hex;
use crate::manifest::{AgentEntry, FileEntry, SkillEntry};
use crate::manifest::{HookEvent, HookHandlerType, HookSessionSource, HookTool};
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
    agent: &AgentEntry,
) -> Result<ManagedFile> {
    let target_path = managed_artifact_path(
        names,
        project_root,
        crate::adapters::Adapter::Claude,
        ArtifactKind::Agent,
        package,
        &agent.id,
    )
    .expect("claude agent path");
    let source_path = snapshot_root.join(&agent.path);
    let contents = fs::read(&source_path)
        .with_context(|| format!("failed to read snapshot file {}", source_path.display()))?;
    let contents = if agent.is_toml() {
        markdown_from_codex_agent_toml(
            &contents,
            &format!("Claude agent source {}", source_path.display()),
        )?
    } else {
        contents
    };
    Ok(ManagedFile {
        path: target_path,
        contents,
    })
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
    hooks: &[ManagedHookSpec],
    plugin_packages: &[(&ResolvedPackage, &Path)],
    merge_existing: bool,
) -> Result<(Vec<ManagedFile>, Vec<String>)> {
    let settings_path = project_root.join(".claude/settings.json");
    let mut files = hooks
        .iter()
        .map(|hook| ManagedFile {
            path: project_root.join(managed_script_relative_path(hook)),
            contents: hook_script_contents(hook),
        })
        .collect::<Vec<_>>();
    let mut entries = hooks
        .iter()
        .map(|hook| ManagedSettingsEntry {
            event: event_name(hook).to_string(),
            entry: hook_entry(hook),
        })
        .collect::<Vec<_>>();
    let mut warnings = Vec::new();

    for (package, snapshot_root) in plugin_packages {
        if package
            .manifest
            .claude_plugin_hook_compat_sources()
            .is_empty()
        {
            continue;
        }

        // Claude plugin hook configs are adapter-specific compatibility imports.
        // We materialize a plugin-like root under `.nodus/packages/...` so
        // `${CLAUDE_PLUGIN_ROOT}` commands keep working without treating these
        // configs as portable Nodus hooks.
        files.extend(copy_package_files(
            plugin_install_root(project_root, package),
            package,
            snapshot_root,
        )?);

        let (package_entries, package_scripts, package_warnings) =
            plugin_hook_entries(project_root, package, snapshot_root)?;
        entries.extend(package_entries);
        files.extend(package_scripts);
        warnings.extend(package_warnings);
    }

    if !entries.is_empty() {
        files.push(ManagedFile {
            path: settings_path.clone(),
            contents: settings_contents(&settings_path, merge_existing, &entries)?,
        });
    }

    Ok((files, warnings))
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

fn copy_package_files(
    target_root: impl AsRef<Path>,
    package: &ResolvedPackage,
    source_root: impl AsRef<Path>,
) -> Result<Vec<ManagedFile>> {
    let target_root = target_root.as_ref();
    let source_root = source_root.as_ref();
    let mut files = Vec::new();

    for path in package.manifest.package_files()? {
        let relative = strip_path_prefix(&path, &package.manifest.root)
            .with_context(|| format!("failed to make {} relative", path.display()))?;
        files.push(copy_file(
            target_root.join(relative),
            source_root.join(relative),
        )?);
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

#[derive(Debug)]
struct ManagedSettingsEntry {
    event: String,
    entry: Value,
}

fn hook_script_contents(hook: &ManagedHookSpec) -> Vec<u8> {
    debug_assert!(matches!(
        hook.hook.handler.handler_type,
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

fn plugin_hook_entries(
    project_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
) -> Result<(Vec<ManagedSettingsEntry>, Vec<ManagedFile>, Vec<String>)> {
    let mut entries = Vec::new();
    let mut files = Vec::new();
    let mut warnings = Vec::new();

    for source in package.manifest.claude_plugin_hook_compat_sources() {
        let config = match source {
            crate::manifest::ClaudePluginHookCompatSource::Inline(config) => config.clone(),
            crate::manifest::ClaudePluginHookCompatSource::Path(path) => {
                serde_json::from_slice(&fs::read(snapshot_root.join(path)).with_context(|| {
                    format!(
                        "failed to read Claude plugin hook config {}",
                        path.display()
                    )
                })?)
                .with_context(|| {
                    format!(
                        "failed to parse Claude plugin hook config {}",
                        path.display()
                    )
                })?
            }
        };

        let Some(hooks) = config.get("hooks").and_then(Value::as_object) else {
            warnings.push(format!(
                "skipping unsupported Claude plugin hook config for `{}`: expected a top-level `hooks` object",
                package.alias
            ));
            continue;
        };

        for (event, event_entries) in hooks {
            let Some(event_entries) = event_entries.as_array() else {
                warnings.push(format!(
                    "skipping unsupported Claude plugin hook event `{event}` for `{}`: expected an array of hook entries",
                    package.alias
                ));
                continue;
            };

            for (entry_index, entry) in event_entries.iter().enumerate() {
                let Some(entry_object) = entry.as_object() else {
                    warnings.push(format!(
                        "skipping unsupported Claude plugin hook entry `{event}[{entry_index}]` for `{}`: expected an object",
                        package.alias
                    ));
                    continue;
                };
                let Some(hook_actions) = entry_object.get("hooks").and_then(Value::as_array) else {
                    warnings.push(format!(
                        "skipping unsupported Claude plugin hook entry `{event}[{entry_index}]` for `{}`: expected a `hooks` array",
                        package.alias
                    ));
                    continue;
                };

                let mut managed_actions = Vec::new();
                for (action_index, action) in hook_actions.iter().enumerate() {
                    let Some(action_object) = action.as_object() else {
                        warnings.push(format!(
                            "skipping unsupported Claude plugin hook action `{event}[{entry_index}].hooks[{action_index}]` for `{}`: expected an object",
                            package.alias
                        ));
                        continue;
                    };
                    let Some(action_type) = action_object.get("type").and_then(Value::as_str)
                    else {
                        warnings.push(format!(
                            "skipping unsupported Claude plugin hook action `{event}[{entry_index}].hooks[{action_index}]` for `{}`: missing `type`",
                            package.alias
                        ));
                        continue;
                    };
                    if action_type != "command" {
                        warnings.push(format!(
                            "skipping unsupported Claude plugin hook action `{event}[{entry_index}].hooks[{action_index}]` for `{}`: only `command` hooks are supported",
                            package.alias
                        ));
                        continue;
                    }
                    let Some(command) = action_object.get("command").and_then(Value::as_str) else {
                        warnings.push(format!(
                            "skipping unsupported Claude plugin hook action `{event}[{entry_index}].hooks[{action_index}]` for `{}`: missing `command`",
                            package.alias
                        ));
                        continue;
                    };

                    let script_stem = managed_plugin_script_stem(
                        package,
                        event,
                        entry_index,
                        action_index,
                        command,
                    );
                    let script_relative_path = format!(".claude/hooks/{script_stem}.sh");
                    files.push(ManagedFile {
                        path: project_root.join(&script_relative_path),
                        contents: plugin_hook_script_contents(package, command),
                    });

                    let mut managed_action = action_object.clone();
                    managed_action.insert(
                        "command".to_string(),
                        Value::String(format!(
                            "sh {}",
                            shell_quote(&format!("./{script_relative_path}"))
                        )),
                    );
                    managed_actions.push(Value::Object(managed_action));
                }

                if managed_actions.is_empty() {
                    continue;
                }

                let mut managed_entry = serde_json::Map::new();
                if let Some(matcher) = entry_object.get("matcher") {
                    managed_entry.insert("matcher".to_string(), matcher.clone());
                }
                managed_entry.insert("hooks".to_string(), Value::Array(managed_actions));
                entries.push(ManagedSettingsEntry {
                    event: event.to_string(),
                    entry: Value::Object(managed_entry),
                });
            }
        }
    }

    Ok((entries, files, warnings))
}

fn plugin_hook_script_contents(package: &ResolvedPackage, command: &str) -> Vec<u8> {
    format!(
        r#"#!/bin/sh
set -eu

project_root="${{CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}}"
export CLAUDE_PLUGIN_ROOT="$project_root/{plugin_root}"
export CLAUDE_PLUGIN_DATA="$project_root/{plugin_data}"

exec sh -lc {command}
"#,
        plugin_root = plugin_install_root_relative(package),
        plugin_data = plugin_data_root_relative(package),
        command = shell_quote(command),
    )
    .into_bytes()
}

fn plugin_install_root(project_root: &Path, package: &ResolvedPackage) -> std::path::PathBuf {
    project_root.join(plugin_install_root_relative(package))
}

fn plugin_install_root_relative(package: &ResolvedPackage) -> String {
    format!(".nodus/packages/{}/claude-plugin", package.alias)
}

fn plugin_data_root_relative(package: &ResolvedPackage) -> String {
    format!(".nodus/packages/{}/claude-plugin-data", package.alias)
}

fn managed_plugin_script_stem(
    package: &ResolvedPackage,
    event: &str,
    entry_index: usize,
    action_index: usize,
    command: &str,
) -> String {
    let digest = blake3_hex(
        format!(
            "{}:{event}:{entry_index}:{action_index}:{command}",
            package.alias
        )
        .as_bytes(),
    );
    format!("nodus-plugin-hook-{}-{}", package.alias, &digest[..8])
}

fn settings_contents(
    path: &Path,
    merge_existing: bool,
    entries: &[ManagedSettingsEntry],
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
    for managed_entry in entries {
        array_field(hooks_object, &managed_entry.event, path)?.push(managed_entry.entry.clone());
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
    for entries in hooks.values_mut().filter_map(Value::as_array_mut) {
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
                        .is_some_and(is_managed_hook_command)
            })
        })
}

fn is_managed_hook_command(command: &str) -> bool {
    command.contains("./.claude/hooks/nodus-hook-")
        || command.contains("./.claude/hooks/nodus-plugin-hook-")
}

fn managed_hook_command(hook: &ManagedHookSpec) -> String {
    format!(
        "sh {}",
        shell_quote(&format!("./{}", managed_script_relative_path(hook)))
    )
}

fn managed_script_relative_path(hook: &ManagedHookSpec) -> String {
    format!(".claude/hooks/{}.sh", managed_script_stem(hook))
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
        HookEvent::UserPromptSubmit => "UserPromptSubmit",
        HookEvent::PreToolUse => "PreToolUse",
        HookEvent::PostToolUse => "PostToolUse",
        HookEvent::Stop => "Stop",
        HookEvent::SubagentStop => "SubagentStop",
        HookEvent::SessionEnd => "SessionEnd",
        HookEvent::PermissionRequest => unreachable!("unsupported hook event for Claude"),
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
        HookEvent::UserPromptSubmit
        | HookEvent::Stop
        | HookEvent::SubagentStop
        | HookEvent::SessionEnd => None,
        HookEvent::PermissionRequest => unreachable!("unsupported hook event for Claude"),
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'"'"'"#))
}
