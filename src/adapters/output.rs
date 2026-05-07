use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use toml::Value as TomlValue;

use super::{
    Adapter, Adapters, ArtifactKind, ManagedArtifactNames, ManagedFile, ManagedHookSpec,
    hook_supported_by_adapter,
};
use crate::lockfile::{Lockfile, managed_mcp_server_name};
use crate::manifest::{DependencyComponent, HookSpec, McpServerConfig};
use crate::paths::{display_path, strip_path_prefix};
use crate::resolver::{PackageSource, ResolvedPackage};

#[derive(Debug, Default)]
pub(crate) struct OutputPlan {
    pub files: Vec<ManagedFile>,
    pub managed_files: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default)]
struct OutputAccumulator {
    files: BTreeMap<PathBuf, Vec<u8>>,
    managed_files: BTreeSet<String>,
    warnings: Vec<String>,
}

#[derive(Debug, Default)]
struct RuntimeGitignoreAccumulator {
    explicit_lines: Vec<String>,
    generated_patterns: BTreeSet<String>,
}

#[derive(Debug, Default)]
struct RuntimeGitignorePlan {
    files: Vec<ManagedFile>,
    consumed_inputs: Vec<PathBuf>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProjectMcpConfig {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: BTreeMap<String, EmittedMcpServerConfig>,
    #[serde(flatten)]
    extra: BTreeMap<String, JsonValue>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProjectOpenCodeConfig {
    #[serde(rename = "mcp", default)]
    mcp_servers: BTreeMap<String, JsonValue>,
    #[serde(flatten)]
    extra: BTreeMap<String, JsonValue>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProjectCodexConfig {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    mcp_servers: BTreeMap<String, TomlValue>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    features: BTreeMap<String, TomlValue>,
    #[serde(flatten)]
    extra: BTreeMap<String, TomlValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EmittedMcpServerConfig {
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    transport_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
}

fn managed_nodus_command() -> String {
    "nodus".to_string()
}

fn managed_nodus_args() -> Vec<String> {
    vec!["mcp".to_string(), "serve".to_string()]
}

fn collected_hooks(packages: &[(ResolvedPackage, PathBuf)]) -> Vec<ManagedHookSpec> {
    let mut hooks = Vec::new();
    let mut seen_ids: BTreeSet<String> = BTreeSet::new();
    let mut push_hook = |package_alias: &str, emitted_from_root: bool, hook: HookSpec| {
        if !seen_ids.insert(hook.id.clone()) {
            return;
        }
        hooks.push(ManagedHookSpec {
            package_alias: package_alias.to_string(),
            emitted_from_root,
            hook,
        });
    };

    if let Some((package, _)) = packages
        .iter()
        .find(|(package, _)| matches!(package.source, PackageSource::Root))
    {
        for hook in package.manifest.manifest.effective_hooks() {
            push_hook(&package.alias, true, hook);
        }
    }

    for (package, _) in packages
        .iter()
        .filter(|(package, _)| !matches!(package.source, PackageSource::Root))
    {
        for hook in package.manifest.manifest.hooks.iter().cloned() {
            push_hook(&package.alias, false, hook);
        }
    }

    hooks
}

pub(crate) fn build_output_plan(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    selected_adapters: Adapters,
    existing_lockfile: Option<&Lockfile>,
    merge_existing_mcp: bool,
) -> Result<OutputPlan> {
    let mut plan = OutputAccumulator::default();
    let managed_names =
        ManagedArtifactNames::from_resolved_packages(packages.iter().map(|(package, _)| package));
    let hooks = collected_hooks(packages);
    let emit_codex_hooks = selected_adapters.contains(Adapter::Codex)
        && hooks
            .iter()
            .any(|hook| hook_targets_adapter(&hook.hook, selected_adapters, Adapter::Codex));

    for (package, snapshot_root) in packages {
        if matches!(package.source, PackageSource::Root) && !package.manifest.manifest.publish_root
        {
            continue;
        }

        warn_if_unsupported(
            &mut plan.warnings,
            package,
            ArtifactKind::Skill,
            package.manifest.discovered.skills.len(),
        );
        warn_if_unsupported(
            &mut plan.warnings,
            package,
            ArtifactKind::Agent,
            package.manifest.discovered.agents.len(),
        );
        warn_if_unsupported(
            &mut plan.warnings,
            package,
            ArtifactKind::Rule,
            package.manifest.discovered.rules.len(),
        );
        warn_if_unsupported(
            &mut plan.warnings,
            package,
            ArtifactKind::Command,
            package.manifest.discovered.commands.len(),
        );

        for skill in &package.manifest.discovered.skills {
            if !package.selects_component(DependencyComponent::Skills) {
                continue;
            }

            if selected_adapters.contains(Adapter::Agents)
                && ArtifactKind::Skill
                    .supported_adapters()
                    .contains(Adapter::Agents)
            {
                merge_files(
                    &mut plan.files,
                    super::agents::skill_files(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        skill,
                    )?,
                )?;
                plan.managed_files
                    .insert(format!(".agents/skills/{}", skill.id));
            }

            if selected_adapters.contains(Adapter::Claude)
                && ArtifactKind::Skill
                    .supported_adapters()
                    .contains(Adapter::Claude)
            {
                merge_files(
                    &mut plan.files,
                    super::claude::skill_files(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        skill,
                    )?,
                )?;
                plan.managed_files
                    .insert(format!(".claude/skills/{}", skill.id));
            }

            if selected_adapters.contains(Adapter::Codex)
                && ArtifactKind::Skill
                    .supported_adapters()
                    .contains(Adapter::Codex)
            {
                merge_files(
                    &mut plan.files,
                    super::codex::skill_files(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        skill,
                    )?,
                )?;
                plan.managed_files
                    .insert(format!(".codex/skills/{}", skill.id));
            }

            if selected_adapters.contains(Adapter::Copilot)
                && ArtifactKind::Skill
                    .supported_adapters()
                    .contains(Adapter::Copilot)
            {
                merge_files(
                    &mut plan.files,
                    super::copilot::skill_files(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        skill,
                    )?,
                )?;
                plan.managed_files
                    .insert(format!(".github/skills/{}", skill.id));
            }

            if selected_adapters.contains(Adapter::Cursor)
                && ArtifactKind::Skill
                    .supported_adapters()
                    .contains(Adapter::Cursor)
            {
                merge_files(
                    &mut plan.files,
                    super::cursor::skill_files(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        skill,
                    )?,
                )?;
                plan.managed_files
                    .insert(format!(".cursor/skills/{}", skill.id));
            }

            if selected_adapters.contains(Adapter::OpenCode)
                && ArtifactKind::Skill
                    .supported_adapters()
                    .contains(Adapter::OpenCode)
            {
                merge_files(
                    &mut plan.files,
                    super::opencode::skill_files(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        skill,
                    )?,
                )?;
                plan.managed_files
                    .insert(format!(".opencode/skills/{}", skill.id));
            }
        }

        if package.selects_component(DependencyComponent::Agents) {
            if selected_adapters.contains(Adapter::Claude)
                && ArtifactKind::Agent
                    .supported_adapters()
                    .contains(Adapter::Claude)
            {
                for agent in package.manifest.discovered.selected_agents(Adapter::Claude) {
                    merge_file(
                        &mut plan.files,
                        super::claude::agent_file(
                            &managed_names,
                            project_root,
                            package,
                            snapshot_root,
                            agent,
                        )?,
                    )?;
                    plan.managed_files
                        .insert(format!(".claude/agents/{}.md", agent.id));
                }
            }

            if selected_adapters.contains(Adapter::Codex)
                && ArtifactKind::Agent
                    .supported_adapters()
                    .contains(Adapter::Codex)
            {
                for agent in package.manifest.discovered.selected_agents(Adapter::Codex) {
                    merge_file(
                        &mut plan.files,
                        super::codex::agent_file(
                            &managed_names,
                            project_root,
                            package,
                            snapshot_root,
                            agent,
                        )?,
                    )?;
                    plan.managed_files
                        .insert(format!(".codex/agents/{}.toml", agent.id));
                }
            }

            if selected_adapters.contains(Adapter::Copilot)
                && ArtifactKind::Agent
                    .supported_adapters()
                    .contains(Adapter::Copilot)
            {
                for agent in package
                    .manifest
                    .discovered
                    .selected_agents(Adapter::Copilot)
                {
                    merge_file(
                        &mut plan.files,
                        super::copilot::agent_file(
                            &managed_names,
                            project_root,
                            package,
                            snapshot_root,
                            agent,
                        )?,
                    )?;
                    plan.managed_files
                        .insert(format!(".github/agents/{}", agent.id));
                }
            }

            if selected_adapters.contains(Adapter::OpenCode)
                && ArtifactKind::Agent
                    .supported_adapters()
                    .contains(Adapter::OpenCode)
            {
                for agent in package
                    .manifest
                    .discovered
                    .selected_agents(Adapter::OpenCode)
                {
                    merge_file(
                        &mut plan.files,
                        super::opencode::agent_file(
                            &managed_names,
                            project_root,
                            package,
                            snapshot_root,
                            agent,
                        )?,
                    )?;
                    plan.managed_files
                        .insert(format!(".opencode/agents/{}.md", agent.id));
                }
            }
        }

        for rule in &package.manifest.discovered.rules {
            if !package.selects_component(DependencyComponent::Rules) {
                continue;
            }

            if selected_adapters.contains(Adapter::Claude)
                && ArtifactKind::Rule
                    .supported_adapters()
                    .contains(Adapter::Claude)
            {
                merge_file(
                    &mut plan.files,
                    super::claude::rule_file(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        rule,
                    )?,
                )?;
                plan.managed_files
                    .insert(format!(".claude/rules/{}.md", rule.id));
            }

            if selected_adapters.contains(Adapter::OpenCode)
                && ArtifactKind::Rule
                    .supported_adapters()
                    .contains(Adapter::OpenCode)
            {
                merge_file(
                    &mut plan.files,
                    super::opencode::rule_file(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        rule,
                    )?,
                )?;
                plan.managed_files
                    .insert(format!(".opencode/rules/{}.md", rule.id));
            }

            if selected_adapters.contains(Adapter::Cursor)
                && ArtifactKind::Rule
                    .supported_adapters()
                    .contains(Adapter::Cursor)
            {
                merge_file(
                    &mut plan.files,
                    super::cursor::rule_file(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        rule,
                    )?,
                )?;
                plan.managed_files
                    .insert(format!(".cursor/rules/{}.mdc", rule.id));
            }
        }

        for command in &package.manifest.discovered.commands {
            if !package.selects_component(DependencyComponent::Commands) {
                continue;
            }

            if selected_adapters.contains(Adapter::Agents)
                && ArtifactKind::Command
                    .supported_adapters()
                    .contains(Adapter::Agents)
            {
                merge_file(
                    &mut plan.files,
                    super::agents::command_file(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        command,
                    )?,
                )?;
                plan.managed_files
                    .insert(format!(".agents/commands/{}.md", command.id));
            }

            if selected_adapters.contains(Adapter::Claude)
                && ArtifactKind::Command
                    .supported_adapters()
                    .contains(Adapter::Claude)
            {
                merge_file(
                    &mut plan.files,
                    super::claude::command_file(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        command,
                    )?,
                )?;
                plan.managed_files
                    .insert(format!(".claude/commands/{}.md", command.id));
            }

            if selected_adapters.contains(Adapter::Codex) {
                let skill_id =
                    super::codex::synthetic_command_skill_id(&managed_names, package, &command.id);
                merge_file(
                    &mut plan.files,
                    super::codex::command_skill_file(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        command,
                    )?,
                )?;
                plan.managed_files
                    .insert(format!(".codex/skills/{skill_id}"));
            }

            if selected_adapters.contains(Adapter::Cursor)
                && ArtifactKind::Command
                    .supported_adapters()
                    .contains(Adapter::Cursor)
            {
                merge_file(
                    &mut plan.files,
                    super::cursor::command_file(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        command,
                    )?,
                )?;
                plan.managed_files
                    .insert(format!(".cursor/commands/{}.md", command.id));
            }

            if selected_adapters.contains(Adapter::OpenCode)
                && ArtifactKind::Command
                    .supported_adapters()
                    .contains(Adapter::OpenCode)
            {
                merge_file(
                    &mut plan.files,
                    super::opencode::command_file(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        command,
                    )?,
                )?;
                plan.managed_files
                    .insert(format!(".opencode/commands/{}.md", command.id));
            }
        }

        merge_files(
            &mut plan.files,
            managed_path_files(project_root, package, snapshot_root)?,
        )?;
        register_managed_paths(project_root, &mut plan.managed_files, package)?;
    }

    if let Some(file) = mcp_config_file(
        project_root,
        packages,
        existing_lockfile,
        merge_existing_mcp,
    )? {
        plan.managed_files
            .insert(display_relative(project_root, &file.path));
        merge_file(&mut plan.files, file)?;
    }
    if selected_adapters.contains(Adapter::Codex)
        && let Some(file) = codex_mcp_config_file(
            project_root,
            packages,
            existing_lockfile,
            merge_existing_mcp,
            emit_codex_hooks,
        )?
    {
        plan.managed_files
            .insert(display_relative(project_root, &file.path));
        merge_file(&mut plan.files, file)?;
    }
    if selected_adapters.contains(Adapter::OpenCode)
        && let Some(file) = opencode_mcp_config_file(
            project_root,
            packages,
            existing_lockfile,
            merge_existing_mcp,
            &mut plan.warnings,
        )?
    {
        plan.managed_files
            .insert(display_relative(project_root, &file.path));
        merge_file(&mut plan.files, file)?;
    }

    let has_claude_plugin_hooks = selected_adapters.contains(Adapter::Claude)
        && packages.iter().any(|(package, _)| {
            !package
                .manifest
                .claude_plugin_hook_compat_sources()
                .is_empty()
        });
    let has_opencode_plugin_hooks = selected_adapters.contains(Adapter::OpenCode)
        && packages
            .iter()
            .any(|(package, _)| !package.manifest.manifest.opencode_plugin_hooks.is_empty());

    if !hooks.is_empty() || has_claude_plugin_hooks || has_opencode_plugin_hooks {
        for file in hook_files(
            project_root,
            packages,
            &hooks,
            selected_adapters,
            merge_existing_mcp,
            &mut plan.warnings,
        )? {
            plan.managed_files
                .insert(display_relative(project_root, &file.path));
            merge_file(&mut plan.files, file)?;
        }
    }

    let gitignores = gitignore_files(project_root, &plan.files)?;
    for consumed in gitignores.consumed_inputs {
        plan.files.remove(&consumed);
    }
    for file in gitignores.files {
        plan.managed_files
            .insert(display_relative(project_root, &file.path));
        merge_file(&mut plan.files, file)?;
    }

    Ok(OutputPlan {
        files: plan
            .files
            .into_iter()
            .map(|(path, contents)| ManagedFile { path, contents })
            .collect(),
        managed_files: plan.managed_files.into_iter().collect(),
        warnings: plan.warnings,
    })
}

fn gitignore_files(
    project_root: &Path,
    files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<RuntimeGitignorePlan> {
    let mut entries = BTreeMap::<PathBuf, RuntimeGitignoreAccumulator>::new();

    for (path, contents) in files {
        if let Some(root) = runtime_root_gitignore(project_root, path)? {
            entries
                .entry(root)
                .or_default()
                .explicit_lines
                .extend(parse_gitignore_lines(path, contents)?);
            continue;
        }

        let Some((root, pattern)) = gitignore_entry(project_root, path)? else {
            continue;
        };
        entries
            .entry(root)
            .or_default()
            .generated_patterns
            .insert(pattern);
    }

    for (root, entry) in &mut entries {
        if entry.generated_patterns.is_empty() || !entry.explicit_lines.is_empty() {
            continue;
        }

        let path = root.join(".gitignore");
        if !path.is_file() {
            continue;
        }

        let contents = fs::read(&path)
            .with_context(|| format!("failed to read existing {}", path.display()))?;
        entry.explicit_lines = parse_gitignore_lines(&path, &contents)?;
    }

    let mut plan = RuntimeGitignorePlan::default();
    for (root, entry) in entries {
        if entry.generated_patterns.is_empty() {
            continue;
        }

        let path = root.join(".gitignore");
        if !entry.explicit_lines.is_empty() {
            plan.consumed_inputs.push(path.clone());
        }
        plan.files.push(ManagedFile {
            path,
            contents: render_gitignore(&entry.explicit_lines, &entry.generated_patterns)
                .into_bytes(),
        });
    }

    Ok(plan)
}

fn managed_path_files(
    project_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
) -> Result<Vec<ManagedFile>> {
    let mut files = Vec::new();

    for mapping in package.managed_paths() {
        for file in &mapping.files {
            let contents =
                fs::read(snapshot_root.join(&file.source_relative)).with_context(|| {
                    format!(
                        "failed to read managed source {} for `{}`",
                        file.source_relative.display(),
                        package.alias
                    )
                })?;
            files.push(ManagedFile {
                path: project_root.join(&file.target_relative),
                contents,
            });
        }
    }

    Ok(files)
}

fn register_managed_paths(
    project_root: &Path,
    managed_files: &mut BTreeSet<String>,
    package: &ResolvedPackage,
) -> Result<()> {
    for mapping in package.managed_paths() {
        validate_direct_managed_root(project_root, managed_files, &mapping.ownership_root)?;
        managed_files.insert(display_relative(
            project_root,
            &project_root.join(&mapping.ownership_root),
        ));
        for file in &mapping.files {
            managed_files.insert(display_relative(
                project_root,
                &project_root.join(&file.target_relative),
            ));
        }
    }

    Ok(())
}

fn mcp_config_file(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    existing_lockfile: Option<&Lockfile>,
    merge_existing_mcp: bool,
) -> Result<Option<ManagedFile>> {
    let path = project_root.join(".mcp.json");
    let previously_managed = previously_managed_mcp_servers(existing_lockfile, ".mcp.json");
    let mut desired_servers = BTreeMap::new();
    for (package, _) in packages {
        if !package_selects_mcp(package) {
            continue;
        }

        for (server_id, server) in &package.manifest.manifest.mcp_servers {
            if !server.enabled {
                continue;
            }
            desired_servers.insert(
                managed_mcp_server_name(&package.alias, server_id),
                emitted_mcp_server(server),
            );
        }
    }

    if should_auto_register_nodus_mcp(packages) {
        let nodus_command = managed_nodus_command();
        let nodus_args = managed_nodus_args();
        desired_servers.insert(
            "nodus".to_string(),
            EmittedMcpServerConfig {
                transport_type: None,
                command: Some(nodus_command),
                url: None,
                args: nodus_args,
                env: BTreeMap::new(),
                headers: BTreeMap::new(),
                cwd: None,
            },
        );
    }

    if desired_servers.is_empty() && previously_managed.is_empty() {
        return Ok(None);
    }

    let mut config = if merge_existing_mcp && path.exists() {
        read_project_mcp_config(&path)?
    } else {
        ProjectMcpConfig::default()
    };

    config.mcp_servers.retain(|server_name, _| {
        !previously_managed.contains(server_name) && !desired_servers.contains_key(server_name)
    });
    config.mcp_servers.extend(desired_servers);

    if config.mcp_servers.is_empty() && config.extra.is_empty() {
        return Ok(None);
    }

    let mut contents = serde_json::to_vec_pretty(&config)
        .context("failed to serialize managed MCP configuration")?;
    contents.push(b'\n');
    Ok(Some(ManagedFile { path, contents }))
}

fn read_project_mcp_config(path: &Path) -> Result<ProjectMcpConfig> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse MCP config {}", path.display()))
}

fn codex_mcp_config_file(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    existing_lockfile: Option<&Lockfile>,
    merge_existing_mcp: bool,
    emit_launch_sync: bool,
) -> Result<Option<ManagedFile>> {
    let path = project_root.join(".codex/config.toml");
    let previously_managed =
        previously_managed_mcp_servers(existing_lockfile, ".codex/config.toml");
    let mut desired_servers = BTreeMap::new();
    for (package, _) in packages {
        if !package_selects_mcp(package) {
            continue;
        }

        for (server_id, server) in &package.manifest.manifest.mcp_servers {
            if !server.enabled {
                continue;
            }
            desired_servers.insert(
                managed_mcp_server_name(&package.alias, server_id),
                emitted_codex_mcp_server(server),
            );
        }
    }

    if should_auto_register_nodus_mcp(packages) {
        let nodus_command = managed_nodus_command();
        let mut table = toml::map::Map::new();
        table.insert("command".into(), TomlValue::String(nodus_command));
        table.insert(
            "args".into(),
            TomlValue::Array(
                managed_nodus_args()
                    .into_iter()
                    .map(TomlValue::String)
                    .collect(),
            ),
        );
        desired_servers.insert("nodus".to_string(), TomlValue::Table(table));
    }

    if desired_servers.is_empty() && previously_managed.is_empty() && !emit_launch_sync {
        return Ok(None);
    }

    let mut config = if merge_existing_mcp && path.exists() {
        read_project_codex_config(&path)?
    } else {
        ProjectCodexConfig::default()
    };

    config.mcp_servers.retain(|server_name, _| {
        !previously_managed.contains(server_name) && !desired_servers.contains_key(server_name)
    });
    config.mcp_servers.extend(desired_servers);
    if emit_launch_sync {
        config
            .features
            .insert("codex_hooks".into(), TomlValue::Boolean(true));
    }

    if config.mcp_servers.is_empty() && config.features.is_empty() && config.extra.is_empty() {
        return Ok(None);
    }

    let mut contents = toml::to_string_pretty(&config)
        .context("failed to serialize managed Codex MCP configuration")?
        .into_bytes();
    contents.push(b'\n');
    Ok(Some(ManagedFile { path, contents }))
}

fn read_project_codex_config(path: &Path) -> Result<ProjectCodexConfig> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&contents)
        .with_context(|| format!("failed to parse Codex config {}", path.display()))
}

fn emitted_codex_mcp_server(server: &McpServerConfig) -> TomlValue {
    let mut table = toml::map::Map::new();

    if let Some(command) = &server.command {
        table.insert("command".into(), TomlValue::String(command.clone()));
        if !server.args.is_empty() {
            table.insert(
                "args".into(),
                TomlValue::Array(server.args.iter().cloned().map(TomlValue::String).collect()),
            );
        }
        if !server.env.is_empty() {
            table.insert(
                "env".into(),
                TomlValue::Table(
                    server
                        .env
                        .iter()
                        .map(|(key, value)| (key.clone(), TomlValue::String(value.clone())))
                        .collect(),
                ),
            );
        }
        if let Some(cwd) = &server.cwd {
            table.insert("cwd".into(), TomlValue::String(display_path(cwd)));
        }
    } else if let Some(url) = &server.url {
        table.insert("url".into(), TomlValue::String(url.clone()));
        let (bearer_token_env_var, http_headers, env_http_headers) =
            emitted_codex_http_headers(&server.headers);
        if let Some(value) = bearer_token_env_var {
            table.insert("bearer_token_env_var".into(), TomlValue::String(value));
        }
        if !http_headers.is_empty() {
            table.insert(
                "http_headers".into(),
                TomlValue::Table(
                    http_headers
                        .into_iter()
                        .map(|(key, value)| (key, TomlValue::String(value)))
                        .collect(),
                ),
            );
        }
        if !env_http_headers.is_empty() {
            table.insert(
                "env_http_headers".into(),
                TomlValue::Table(
                    env_http_headers
                        .into_iter()
                        .map(|(key, value)| (key, TomlValue::String(value)))
                        .collect(),
                ),
            );
        }
    }

    if !server.enabled {
        table.insert("enabled".into(), TomlValue::Boolean(false));
    }

    TomlValue::Table(table)
}

fn emitted_codex_http_headers(
    headers: &BTreeMap<String, String>,
) -> (
    Option<String>,
    BTreeMap<String, String>,
    BTreeMap<String, String>,
) {
    let mut bearer_token_env_var = None;
    let mut http_headers = BTreeMap::new();
    let mut env_http_headers = BTreeMap::new();

    for (key, value) in headers {
        if key.eq_ignore_ascii_case("authorization")
            && let Some(env_var) = extract_bearer_env_reference(value)
        {
            bearer_token_env_var = Some(env_var.to_string());
            continue;
        }
        if let Some(env_var) = extract_exact_env_reference(value) {
            env_http_headers.insert(key.clone(), env_var.to_string());
        } else {
            http_headers.insert(key.clone(), value.clone());
        }
    }

    (bearer_token_env_var, http_headers, env_http_headers)
}

fn opencode_mcp_config_file(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    existing_lockfile: Option<&Lockfile>,
    merge_existing_mcp: bool,
    warnings: &mut Vec<String>,
) -> Result<Option<ManagedFile>> {
    let path = project_root.join("opencode.json");
    let previously_managed = previously_managed_mcp_servers(existing_lockfile, "opencode.json");
    let mut desired_servers = BTreeMap::new();
    for (package, _) in packages {
        if !package_selects_mcp(package) {
            continue;
        }

        for (server_id, server) in &package.manifest.manifest.mcp_servers {
            if !server.enabled {
                continue;
            }
            let managed_name = managed_mcp_server_name(&package.alias, server_id);
            let Some(server_value) =
                emitted_opencode_mcp_server(package, server_id, server, warnings)
            else {
                continue;
            };
            desired_servers.insert(managed_name, server_value);
        }
    }

    if should_auto_register_nodus_mcp(packages) {
        let nodus_command = managed_nodus_command();
        let mut object = JsonMap::new();
        object.insert("type".into(), JsonValue::String("local".into()));
        object.insert(
            "command".into(),
            JsonValue::Array(
                std::iter::once(nodus_command)
                    .chain(managed_nodus_args())
                    .map(JsonValue::String)
                    .collect(),
            ),
        );
        desired_servers.insert("nodus".to_string(), JsonValue::Object(object));
    }

    if desired_servers.is_empty() && previously_managed.is_empty() {
        return Ok(None);
    }

    let mut config = if merge_existing_mcp && path.exists() {
        read_project_opencode_config(&path)?
    } else {
        ProjectOpenCodeConfig::default()
    };

    config.mcp_servers.retain(|server_name, _| {
        !previously_managed.contains(server_name) && !desired_servers.contains_key(server_name)
    });
    config.mcp_servers.extend(desired_servers);

    if config.mcp_servers.is_empty() && config.extra.is_empty() {
        return Ok(None);
    }

    let mut contents = serde_json::to_vec_pretty(&config)
        .context("failed to serialize managed OpenCode MCP configuration")?;
    contents.push(b'\n');
    Ok(Some(ManagedFile { path, contents }))
}

fn read_project_opencode_config(path: &Path) -> Result<ProjectOpenCodeConfig> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse OpenCode config {}", path.display()))
}

fn should_auto_register_nodus_mcp(packages: &[(ResolvedPackage, PathBuf)]) -> bool {
    packages
        .iter()
        .any(|(package, _)| package_selects_mcp(package))
}

fn package_selects_mcp(package: &ResolvedPackage) -> bool {
    package.emits_runtime_outputs() && package.selects_component(DependencyComponent::Mcp)
}

fn previously_managed_mcp_servers(
    existing_lockfile: Option<&Lockfile>,
    config_path: &str,
) -> HashSet<String> {
    let mut names = existing_lockfile
        .map(Lockfile::managed_mcp_server_names)
        .unwrap_or_default();
    if existing_lockfile.is_some_and(|lockfile| {
        lockfile
            .managed_files
            .iter()
            .any(|managed_file| managed_file == config_path)
    }) {
        names.insert("nodus".to_string());
    }
    names
}

fn emitted_opencode_mcp_server(
    package: &ResolvedPackage,
    server_id: &str,
    server: &McpServerConfig,
    warnings: &mut Vec<String>,
) -> Option<JsonValue> {
    let mut object = JsonMap::new();

    if let Some(command) = &server.command {
        if server.cwd.is_some() {
            warnings.push(format!(
                "skipping OpenCode MCP server `{}` from package `{}` because OpenCode project config does not support `cwd`",
                server_id, package.alias
            ));
            return None;
        }
        object.insert("type".into(), JsonValue::String("local".into()));
        object.insert(
            "command".into(),
            JsonValue::Array(
                std::iter::once(command.clone())
                    .chain(server.args.iter().cloned())
                    .map(JsonValue::String)
                    .collect(),
            ),
        );
        if !server.env.is_empty() {
            object.insert(
                "environment".into(),
                JsonValue::Object(
                    server
                        .env
                        .iter()
                        .map(|(key, value)| (key.clone(), JsonValue::String(value.clone())))
                        .collect(),
                ),
            );
        }
    } else if let Some(url) = &server.url {
        object.insert("type".into(), JsonValue::String("remote".into()));
        object.insert("url".into(), JsonValue::String(url.clone()));
        if !server.headers.is_empty() {
            object.insert(
                "headers".into(),
                JsonValue::Object(
                    server
                        .headers
                        .iter()
                        .map(|(key, value)| {
                            (
                                key.clone(),
                                JsonValue::String(emitted_opencode_header_value(value)),
                            )
                        })
                        .collect(),
                ),
            );
        }
    }

    if !server.enabled {
        object.insert("enabled".into(), JsonValue::Bool(false));
    }

    Some(JsonValue::Object(object))
}

fn emitted_opencode_header_value(value: &str) -> String {
    if let Some(env_var) = extract_exact_env_reference(value) {
        return format!("{{env:{env_var}}}");
    }
    if let Some(env_var) = extract_bearer_env_reference(value) {
        return format!("Bearer {{env:{env_var}}}");
    }
    value.to_string()
}

fn extract_exact_env_reference(value: &str) -> Option<&str> {
    let env_var = value.strip_prefix("${")?.strip_suffix('}')?;
    if env_var.is_empty() {
        None
    } else {
        Some(env_var)
    }
}

fn extract_bearer_env_reference(value: &str) -> Option<&str> {
    let env_var = value.strip_prefix("Bearer ${")?.strip_suffix('}')?;
    if env_var.is_empty() {
        None
    } else {
        Some(env_var)
    }
}

fn emitted_mcp_server(server: &McpServerConfig) -> EmittedMcpServerConfig {
    EmittedMcpServerConfig {
        transport_type: server.transport_type.clone(),
        command: server.command.clone(),
        url: server.url.clone(),
        args: server.args.clone(),
        env: server.env.clone(),
        headers: server.headers.clone(),
        cwd: server.cwd.as_ref().map(|cwd| display_path(cwd)),
    }
}

fn gitignore_entry(project_root: &Path, path: &Path) -> Result<Option<(PathBuf, String)>> {
    let relative = strip_path_prefix(path, project_root)
        .with_context(|| format!("failed to make {} relative", path.display()))?;
    let components = relative
        .iter()
        .map(|component| component.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    let [runtime, rest @ ..] = components.as_slice() else {
        return Ok(None);
    };
    if !matches!(
        runtime.as_str(),
        ".agents" | ".claude" | ".codex" | ".cursor" | ".opencode"
    ) {
        return Ok(None);
    }
    if rest.is_empty() {
        return Ok(None);
    }
    if rest == [".gitignore"] {
        return Ok(None);
    }

    let pattern = if rest.len() >= 2 {
        managed_artifact_gitignore_pattern(runtime, &rest[0], &rest[1])
    } else {
        rest.join("/")
    };

    Ok(Some((project_root.join(runtime), pattern)))
}

fn runtime_root_gitignore(project_root: &Path, path: &Path) -> Result<Option<PathBuf>> {
    let relative = strip_path_prefix(path, project_root)
        .with_context(|| format!("failed to make {} relative", path.display()))?;
    let components = relative
        .iter()
        .map(|component| component.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    let [runtime, gitignore] = components.as_slice() else {
        return Ok(None);
    };
    if !matches!(
        runtime.as_str(),
        ".agents" | ".claude" | ".codex" | ".cursor" | ".opencode"
    ) || gitignore != ".gitignore"
    {
        return Ok(None);
    }

    Ok(Some(project_root.join(runtime)))
}

fn parse_gitignore_lines(path: &Path, contents: &[u8]) -> Result<Vec<String>> {
    let text = std::str::from_utf8(contents)
        .with_context(|| format!("managed gitignore {} must be valid UTF-8", path.display()))?;
    Ok(text
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn managed_artifact_gitignore_pattern(
    runtime: &str,
    artifact_dir: &str,
    artifact_name: &str,
) -> String {
    if artifact_dir == "skills"
        && runtime == ".codex"
        && artifact_name.starts_with(crate::adapters::codex::SYNTHETIC_COMMAND_SKILL_PREFIX)
    {
        return format!("skills/{artifact_name}");
    }

    if artifact_dir == "skills"
        && matches!(
            runtime,
            ".agents" | ".claude" | ".codex" | ".cursor" | ".opencode"
        )
        && let Some((_, suffix)) = artifact_name.rsplit_once('_')
        && !suffix.is_empty()
    {
        return format!("skills/*_{suffix}/");
    }

    if matches!(
        runtime,
        ".agents" | ".claude" | ".codex" | ".cursor" | ".opencode"
    ) && matches!(artifact_dir, "agents" | "commands" | "rules")
        && let Some((stem, extension)) = artifact_name.rsplit_once('.')
        && let Some((_, suffix)) = stem.rsplit_once('_')
        && !suffix.is_empty()
    {
        return format!("{artifact_dir}/*_{suffix}.{extension}");
    }

    format!("{artifact_dir}/{artifact_name}")
}

fn render_gitignore(explicit_lines: &[String], patterns: &BTreeSet<String>) -> String {
    let mut output = String::from("# Managed by nodus\n.gitignore\n");
    let mut seen = BTreeSet::from([
        String::from("# Managed by nodus"),
        String::from(".gitignore"),
    ]);

    for line in explicit_lines {
        if seen.insert(line.clone()) {
            output.push_str(line);
            output.push('\n');
        }
    }
    for pattern in patterns {
        if seen.insert(pattern.clone()) {
            output.push_str(pattern);
            output.push('\n');
        }
    }
    output
}

fn hook_files(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    hooks: &[ManagedHookSpec],
    selected_adapters: Adapters,
    merge_existing_mcp: bool,
    warnings: &mut Vec<String>,
) -> Result<Vec<ManagedFile>> {
    let mut files = Vec::new();

    let claude_hooks = hooks_for_adapter(hooks, selected_adapters, Adapter::Claude);
    let claude_plugin_packages = if selected_adapters.contains(Adapter::Claude) {
        packages
            .iter()
            .filter(|(package, _)| {
                !package
                    .manifest
                    .claude_plugin_hook_compat_sources()
                    .is_empty()
            })
            .map(|(package, snapshot_root)| (package, snapshot_root.as_path()))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if !claude_hooks.is_empty() || !claude_plugin_packages.is_empty() {
        let (claude_files, claude_warnings) = super::claude::hook_files(
            project_root,
            &claude_hooks,
            &claude_plugin_packages,
            merge_existing_mcp,
        )?;
        files.extend(claude_files);
        warnings.extend(claude_warnings);
    }
    let opencode_hooks = hooks_for_adapter(hooks, selected_adapters, Adapter::OpenCode);
    let opencode_plugin_packages = if selected_adapters.contains(Adapter::OpenCode) {
        packages
            .iter()
            .filter(|(package, _)| !package.manifest.manifest.opencode_plugin_hooks.is_empty())
            .map(|(package, snapshot_root)| (package, snapshot_root.as_path()))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if !opencode_hooks.is_empty() {
        files.extend(super::opencode::hook_files(project_root, &opencode_hooks));
    }
    if !opencode_plugin_packages.is_empty() {
        files.extend(super::opencode::plugin_hook_files(
            project_root,
            &opencode_plugin_packages,
        )?);
    }
    if hooks
        .iter()
        .any(|hook| hook_targets_adapter(&hook.hook, selected_adapters, Adapter::Agents))
    {
        warnings.push(
            "hooks are not emitted for `agents`; no documented project hook surface is available"
                .into(),
        );
    }
    let codex_hooks = hooks_for_adapter(hooks, selected_adapters, Adapter::Codex);
    if !codex_hooks.is_empty() {
        files.extend(super::codex::hook_files(project_root, &codex_hooks)?);
    }
    let copilot_hooks = hooks_for_adapter(hooks, selected_adapters, Adapter::Copilot);
    if !copilot_hooks.is_empty() {
        files.extend(super::copilot::hook_files(project_root, &copilot_hooks)?);
    }
    if hooks
        .iter()
        .any(|hook| hook_targets_adapter(&hook.hook, selected_adapters, Adapter::Cursor))
    {
        warnings.push(
            "hooks are not emitted for `cursor`; project hooks exist, but no documented auto-start hook is available for repo-local config".into(),
        );
    }

    Ok(files)
}

fn hooks_for_adapter(
    hooks: &[ManagedHookSpec],
    selected_adapters: Adapters,
    adapter: Adapter,
) -> Vec<ManagedHookSpec> {
    hooks
        .iter()
        .filter(|hook| hook_targets_adapter(&hook.hook, selected_adapters, adapter))
        .filter(|hook| hook_supported_by_adapter(&hook.hook, adapter))
        .cloned()
        .collect()
}

fn hook_targets_adapter(hook: &HookSpec, selected_adapters: Adapters, adapter: Adapter) -> bool {
    if !selected_adapters.contains(adapter) {
        return false;
    }
    if hook.adapters.is_empty() {
        true
    } else {
        hook.adapters.contains(&adapter)
    }
}

fn display_relative(project_root: &Path, path: &Path) -> String {
    display_path(strip_path_prefix(path, project_root).unwrap_or(path))
}

fn validate_direct_managed_root(
    project_root: &Path,
    managed_files: &BTreeSet<String>,
    candidate: &Path,
) -> Result<()> {
    for existing in managed_files.iter().map(PathBuf::from) {
        if existing.starts_with(candidate) || candidate.starts_with(&existing) {
            bail!(
                "managed output roots overlap at {} and {}",
                display_relative(project_root, &project_root.join(&existing)),
                display_relative(project_root, &project_root.join(candidate))
            );
        }
    }

    Ok(())
}

fn warn_if_unsupported(
    warnings: &mut Vec<String>,
    package: &ResolvedPackage,
    kind: ArtifactKind,
    count: usize,
) {
    if count == 0 || !kind.supported_adapters().is_empty() {
        return;
    }

    warnings.push(format!(
        "package `{}` discovered {} {} but no adapters support them",
        package.alias,
        count,
        kind.plural_name()
    ));
}

fn merge_files(target: &mut BTreeMap<PathBuf, Vec<u8>>, files: Vec<ManagedFile>) -> Result<()> {
    for file in files {
        merge_file(target, file)?;
    }
    Ok(())
}

fn merge_file(target: &mut BTreeMap<PathBuf, Vec<u8>>, file: ManagedFile) -> Result<()> {
    match target.get(&file.path) {
        Some(existing) if existing != &file.contents => {
            bail!("multiple packages want to manage {}", file.path.display());
        }
        Some(_) => {}
        None => {
            target.insert(file.path, file.contents);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_kind_support_matrix_matches_supported_adapters() {
        let skill = ArtifactKind::Skill.supported_adapters();
        assert!(skill.contains(Adapter::Agents));
        assert!(skill.intersects(Adapters::CLAUDE));
        assert!(skill.contains(Adapter::Claude));
        assert!(skill.contains(Adapter::Codex));
        assert!(skill.contains(Adapter::Copilot));
        assert!(skill.contains(Adapter::Cursor));
        assert!(skill.contains(Adapter::OpenCode));
        assert_eq!(skill.iter().count(), 6);

        let agent = ArtifactKind::Agent.supported_adapters();
        assert!(!agent.contains(Adapter::Agents));
        assert!(agent.contains(Adapter::Claude));
        assert!(agent.contains(Adapter::Codex));
        assert!(agent.contains(Adapter::Copilot));
        assert!(!agent.contains(Adapter::Cursor));
        assert!(agent.contains(Adapter::OpenCode));

        let rule = ArtifactKind::Rule.supported_adapters();
        assert!(!rule.contains(Adapter::Agents));
        assert!(rule.contains(Adapter::Claude));
        assert!(!rule.contains(Adapter::Codex));
        assert!(rule.contains(Adapter::Cursor));
        assert!(rule.contains(Adapter::OpenCode));

        let command = ArtifactKind::Command.supported_adapters();
        assert!(command.contains(Adapter::Agents));
        assert!(command.contains(Adapter::Claude));
        assert!(!command.contains(Adapter::Codex));
        assert!(command.contains(Adapter::Cursor));
        assert!(command.contains(Adapter::OpenCode));

        assert!(Adapters::NONE.is_empty());
    }

    #[test]
    fn gitignore_files_merge_explicit_runtime_root_gitignore_with_generated_patterns() {
        let project_root = Path::new("/tmp/project");
        let mut files = BTreeMap::new();
        files.insert(
            project_root.join(".claude/.gitignore"),
            b".gitignore\n# custom\nskills/*_abc123/\n".to_vec(),
        );
        files.insert(
            project_root.join(".claude/skills/review_abc123/SKILL.md"),
            b"# Review\n".to_vec(),
        );
        files.insert(
            project_root.join(".claude/commands/build_abc123.md"),
            b"cargo test\n".to_vec(),
        );

        let plan = gitignore_files(project_root, &files).unwrap();

        assert_eq!(
            plan.consumed_inputs,
            vec![project_root.join(".claude/.gitignore")]
        );
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].path, project_root.join(".claude/.gitignore"));
        assert_eq!(
            String::from_utf8(plan.files[0].contents.clone()).unwrap(),
            "# Managed by nodus\n.gitignore\n# custom\nskills/*_abc123/\ncommands/*_abc123.md\n"
        );
    }

    #[test]
    fn gitignore_files_preserve_explicit_runtime_root_gitignore_without_generated_patterns() {
        let project_root = Path::new("/tmp/project");
        let mut files = BTreeMap::new();
        files.insert(
            project_root.join(".claude/.gitignore"),
            b".gitignore\n# custom\n".to_vec(),
        );

        let plan = gitignore_files(project_root, &files).unwrap();

        assert!(plan.consumed_inputs.is_empty());
        assert!(plan.files.is_empty());
    }

    #[test]
    fn gitignore_files_merge_existing_runtime_root_gitignore_from_disk() {
        let temp = tempfile::TempDir::new().unwrap();
        let project_root = temp.path();
        fs::create_dir_all(project_root.join(".codex")).unwrap();
        fs::write(
            project_root.join(".codex/.gitignore"),
            b".gitignore\n# custom\nskills/*_legacy/\n",
        )
        .unwrap();

        let mut files = BTreeMap::new();
        files.insert(
            project_root.join(".codex/skills/review_abc123/SKILL.md"),
            b"# Review\n".to_vec(),
        );

        let plan = gitignore_files(project_root, &files).unwrap();

        assert_eq!(
            plan.consumed_inputs,
            vec![project_root.join(".codex/.gitignore")]
        );
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].path, project_root.join(".codex/.gitignore"));
        assert_eq!(
            String::from_utf8(plan.files[0].contents.clone()).unwrap(),
            "# Managed by nodus\n.gitignore\n# custom\nskills/*_legacy/\nskills/*_abc123/\n"
        );
    }

    #[test]
    fn codex_http_headers_promote_bearer_env_references() {
        let (bearer_token_env_var, http_headers, env_http_headers) =
            emitted_codex_http_headers(&BTreeMap::from([
                (
                    String::from("Authorization"),
                    String::from("Bearer ${FIGMA_TOKEN}"),
                ),
                (String::from("X-Figma-Region"), String::from("us-east-1")),
                (String::from("X-Workspace"), String::from("${WORKSPACE_ID}")),
            ]));

        assert_eq!(bearer_token_env_var.as_deref(), Some("FIGMA_TOKEN"));
        assert_eq!(
            http_headers,
            BTreeMap::from([(String::from("X-Figma-Region"), String::from("us-east-1"))])
        );
        assert_eq!(
            env_http_headers,
            BTreeMap::from([(String::from("X-Workspace"), String::from("WORKSPACE_ID"))])
        );
    }

    #[test]
    fn opencode_header_values_convert_env_references() {
        assert_eq!(emitted_opencode_header_value("${API_KEY}"), "{env:API_KEY}");
        assert_eq!(
            emitted_opencode_header_value("Bearer ${API_KEY}"),
            "Bearer {env:API_KEY}"
        );
        assert_eq!(emitted_opencode_header_value("us-east-1"), "us-east-1");
    }

    #[test]
    fn managed_nodus_command_uses_plain_binary_name() {
        let command = managed_nodus_command();
        assert_eq!(command, "nodus");
    }
}
