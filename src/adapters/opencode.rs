use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::adapters::{
    ArtifactKind, ManagedArtifactNames, ManagedFile, ManagedHookSpec,
    hook_tool_matchers_for_adapter, managed_artifact_path, managed_skill_id, managed_skill_root,
};
use crate::agent_format::markdown_from_codex_agent_toml;
use crate::hashing::blake3_hex;
use crate::manifest::{AgentEntry, FileEntry, SkillEntry};
use crate::manifest::{HookEvent, HookHandlerType, HookSessionSource};
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
        crate::adapters::Adapter::OpenCode,
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
        crate::adapters::Adapter::OpenCode,
        ArtifactKind::Agent,
        package,
        &agent.id,
    )
    .expect("opencode agent path");
    let source_path = snapshot_root.join(&agent.path);
    let contents = fs::read(&source_path)
        .with_context(|| format!("failed to read snapshot file {}", source_path.display()))?;
    let contents = if agent.is_toml() {
        markdown_from_codex_agent_toml(
            &contents,
            &format!("OpenCode agent source {}", source_path.display()),
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

pub fn hook_files(project_root: &Path, hooks: &[ManagedHookSpec]) -> Vec<ManagedFile> {
    let mut files = hooks
        .iter()
        .map(|hook| ManagedFile {
            path: project_root.join(managed_script_relative_path(hook)),
            contents: hook_script_contents(hook),
        })
        .collect::<Vec<_>>();
    files.push(ManagedFile {
        path: project_root.join(".opencode/plugins/nodus-hooks.js"),
        contents: plugin_contents(hooks),
    });
    files
}

pub fn plugin_hook_files(
    project_root: &Path,
    plugin_packages: &[(&ResolvedPackage, &Path)],
) -> Result<Vec<ManagedFile>> {
    let mut files = Vec::new();

    for (package, snapshot_root) in plugin_packages {
        if package.manifest.manifest.opencode_plugin_hooks.is_empty() {
            continue;
        }

        files.extend(copy_package_files(
            plugin_install_root(project_root, package),
            package,
            snapshot_root,
        )?);

        for path in package
            .manifest
            .manifest
            .normalized_opencode_plugin_hooks()?
        {
            files.push(ManagedFile {
                path: project_root.join(plugin_wrapper_relative_path(package, &path)),
                contents: plugin_wrapper_contents(package, &path),
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

    for path in package.package_files()? {
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

fn plugin_install_root(project_root: &Path, package: &ResolvedPackage) -> std::path::PathBuf {
    project_root.join(plugin_install_root_relative(package))
}

fn plugin_install_root_relative(package: &ResolvedPackage) -> String {
    format!(".nodus/packages/{}/opencode-plugin", package.alias)
}

fn plugin_wrapper_relative_path(package: &ResolvedPackage, path: &Path) -> String {
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("plugin")
        .chars()
        .map(|character| match character {
            'a'..='z' | '0'..='9' => character,
            'A'..='Z' => character.to_ascii_lowercase(),
            _ => '-',
        })
        .collect::<String>();
    let digest = blake3_hex(format!("{}:{}", package.alias, display_path_js(path)).as_bytes());
    format!(
        ".opencode/plugins/nodus-{}-{name}-{}.js",
        package.alias,
        &digest[..8]
    )
}

fn plugin_wrapper_contents(package: &ResolvedPackage, path: &Path) -> Vec<u8> {
    let import_path = format!(
        "../../{}/{}",
        plugin_install_root_relative(package),
        display_path_js(path)
    );
    format!("export {{ default }} from {};\n", js_string(&import_path)).into_bytes()
}

fn display_path_js(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
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

fn hook_script_contents(hook: &ManagedHookSpec) -> Vec<u8> {
    debug_assert!(matches!(
        hook.hook.handler.handler_type,
        HookHandlerType::Command
    ));
    format!(
        r#"#!/bin/sh
set -eu

project_root="${{1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}}"
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

fn plugin_contents(hooks: &[ManagedHookSpec]) -> Vec<u8> {
    let session_start_hooks = hooks
        .iter()
        .filter(|hook| matches!(hook.hook.event, HookEvent::SessionStart))
        .filter(|hook| session_start_matches(hook, HookSessionSource::Startup))
        .map(|hook| {
            format!(
                "      await runHook(ctx, root, {}, {{ event: \"session_start\", source: \"startup\", input }});",
                hook_js_config(hook)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let stop_hooks = hooks
        .iter()
        .filter(|hook| matches!(hook.hook.event, HookEvent::Stop))
        .map(|hook| {
            format!(
                "      await runHook(ctx, root, {}, {{ event: \"stop\", input }});",
                hook_js_config(hook)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let pre_tool_hooks = hooks
        .iter()
        .filter(|hook| matches!(hook.hook.event, HookEvent::PreToolUse))
        .map(|hook| {
            format!(
                "      await runToolHook(ctx, root, {}, input, output, \"pre_tool_use\");",
                hook_js_config(hook)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let post_tool_hooks = hooks
        .iter()
        .filter(|hook| matches!(hook.hook.event, HookEvent::PostToolUse))
        .map(|hook| {
            format!(
                "      await runToolHook(ctx, root, {}, input, output, \"post_tool_use\");",
                hook_js_config(hook)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"const SCRIPT_TIMEOUT = 10_000;

async function runScript(root, scriptPath, payload) {{
  const process = Bun.spawn(["sh", scriptPath, root], {{
    stdin: new Blob([JSON.stringify(payload)]),
    stdout: "inherit",
    stderr: "pipe",
  }});
  const exitCode = await process.exited;
  if (exitCode !== 0) {{
    const stderr = await new Response(process.stderr).text();
    throw new Error(stderr || `hook exited with code ${{exitCode}}`);
  }}
}}

async function runHook(ctx, root, hook, payload) {{
  try {{
    await runScript(root, `${{root}}/${{hook.script}}`, payload);
  }} catch (error) {{
    console.error(`nodus hook ${{hook.id}} failed`, error);
    if (hook.blocking) throw error;
  }}
}}

async function runToolHook(ctx, root, hook, input, output, eventName) {{
  const toolName = String(input?.tool ?? "").toLowerCase();
  if (hook.toolNames.length > 0 && !hook.toolNames.includes(toolName)) return;
  await runHook(ctx, root, hook, {{ event: eventName, input, output }});
}}

function plugin(ctx) {{
  const root = ctx.worktree ?? ctx.directory;
  return {{
    "session.created": async (input) => {{
{session_start_hooks}
    }},
    "session.idle": async (input) => {{
{stop_hooks}
    }},
    "tool.execute.before": async (input, output) => {{
{pre_tool_hooks}
    }},
    "tool.execute.after": async (input, output) => {{
{post_tool_hooks}
    }},
  }};
}}

export default plugin;
"#
    )
    .into_bytes()
}

fn session_start_matches(hook: &ManagedHookSpec, source: HookSessionSource) -> bool {
    hook.hook
        .matcher
        .as_ref()
        .map(|matcher| matcher.sources.is_empty() || matcher.sources.contains(&source))
        .unwrap_or(true)
}

fn hook_js_config(hook: &ManagedHookSpec) -> String {
    let tool_names = hook_tool_matchers_for_adapter(&hook.hook, crate::adapters::Adapter::OpenCode)
        .into_iter()
        .map(js_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{{ id: {id}, blocking: {blocking}, script: {script}, toolNames: [{tool_names}] }}",
        id = js_string(&hook.hook.id),
        blocking = if hook.hook.blocking { "true" } else { "false" },
        script = js_string(&managed_script_relative_path(hook)),
        tool_names = tool_names,
    )
}

fn managed_script_relative_path(hook: &ManagedHookSpec) -> String {
    format!(".opencode/scripts/{}.sh", managed_script_stem(hook))
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'"'"'"#))
}

fn js_string(value: &str) -> String {
    format!("{value:?}")
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
