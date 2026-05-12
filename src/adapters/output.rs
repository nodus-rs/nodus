use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use toml::Value as TomlValue;
use toml_edit::{DocumentMut, Item as EditableTomlItem, Table as EditableTomlTable};

use super::{
    Adapter, Adapters, ArtifactKind, ManagedActivationHook, ManagedArtifactNames, ManagedFile,
    ManagedHookSpec, PreferredSurface, artifact_supported, hook_supported_by_adapter,
    managed_runtime_skill_id, preferred_surface,
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

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct OutputPlanOptions {
    pub merge_existing_mcp: bool,
    pub codex_native_plugins_auto_enabled: bool,
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

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn build_output_plan(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    selected_adapters: Adapters,
    existing_lockfile: Option<&Lockfile>,
    merge_existing_mcp: bool,
) -> Result<OutputPlan> {
    build_output_plan_with_options(
        project_root,
        packages,
        selected_adapters,
        existing_lockfile,
        OutputPlanOptions {
            merge_existing_mcp,
            codex_native_plugins_auto_enabled: false,
        },
    )
}

pub(crate) fn build_output_plan_with_options(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    selected_adapters: Adapters,
    existing_lockfile: Option<&Lockfile>,
    options: OutputPlanOptions,
) -> Result<OutputPlan> {
    let mut plan = OutputAccumulator::default();
    let managed_names =
        ManagedArtifactNames::from_resolved_packages(packages.iter().map(|(package, _)| package));
    let hooks = collected_hooks(packages);
    let has_activation = packages
        .iter()
        .any(|(package, _)| package.manifest.manifest.activation_enabled());
    let codex_prefers_native_plugins =
        preferred_surface(Adapter::Codex) == PreferredSurface::PackagePluginWorkspaceMarketplace;
    let emit_codex_hooks = selected_adapters.contains(Adapter::Codex)
        && codex_prefers_native_plugins
        && (has_activation
            || hooks
                .iter()
                .any(|hook| hook_targets_adapter(&hook.hook, selected_adapters, Adapter::Codex)));
    warn_if_activation_unsupported(&mut plan.warnings, selected_adapters, has_activation);

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
                && artifact_supported(Adapter::Agents, ArtifactKind::Skill)
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
                && adapter_uses_direct_runtime_outputs(Adapter::Claude)
                && artifact_supported(Adapter::Claude, ArtifactKind::Skill)
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
                && adapter_uses_direct_runtime_outputs(Adapter::Codex)
                && artifact_supported(Adapter::Codex, ArtifactKind::Skill)
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
                && artifact_supported(Adapter::Copilot, ArtifactKind::Skill)
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
                && artifact_supported(Adapter::Cursor, ArtifactKind::Skill)
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
                && artifact_supported(Adapter::OpenCode, ArtifactKind::Skill)
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
                && adapter_uses_direct_runtime_outputs(Adapter::Claude)
                && artifact_supported(Adapter::Claude, ArtifactKind::Agent)
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
                && adapter_uses_direct_runtime_outputs(Adapter::Codex)
                && artifact_supported(Adapter::Codex, ArtifactKind::Agent)
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
                && artifact_supported(Adapter::Copilot, ArtifactKind::Agent)
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
                && artifact_supported(Adapter::OpenCode, ArtifactKind::Agent)
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
                && adapter_uses_direct_runtime_outputs(Adapter::Claude)
                && artifact_supported(Adapter::Claude, ArtifactKind::Rule)
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
                && artifact_supported(Adapter::OpenCode, ArtifactKind::Rule)
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
                && artifact_supported(Adapter::Cursor, ArtifactKind::Rule)
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
                && artifact_supported(Adapter::Agents, ArtifactKind::Command)
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
                && adapter_uses_direct_runtime_outputs(Adapter::Claude)
                && artifact_supported(Adapter::Claude, ArtifactKind::Command)
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

            if selected_adapters.contains(Adapter::Codex)
                && adapter_uses_direct_runtime_outputs(Adapter::Codex)
            {
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
                && artifact_supported(Adapter::Cursor, ArtifactKind::Command)
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
                && artifact_supported(Adapter::OpenCode, ArtifactKind::Command)
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

        emit_native_package_plugins(
            &mut plan,
            project_root,
            package,
            snapshot_root,
            selected_adapters,
        )?;

        merge_files(
            &mut plan.files,
            managed_path_files(project_root, package, snapshot_root)?,
        )?;
        register_managed_paths(project_root, &mut plan.managed_files, package)?;
    }

    for file in native_package_marketplace_files(project_root, packages, selected_adapters)? {
        plan.managed_files
            .insert(display_relative(project_root, &file.path));
        merge_file(&mut plan.files, file)?;
    }

    if let Some(file) = mcp_config_file(
        project_root,
        packages,
        selected_adapters,
        existing_lockfile,
        options.merge_existing_mcp,
    )? {
        plan.managed_files
            .insert(display_relative(project_root, &file.path));
        merge_file(&mut plan.files, file)?;
    }
    if selected_adapters.contains(Adapter::Codex)
        && let Some(file) = codex_mcp_config_file(
            project_root,
            packages,
            selected_adapters,
            options.codex_native_plugins_auto_enabled,
            existing_lockfile,
            options.merge_existing_mcp,
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
            options.merge_existing_mcp,
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
    let has_claude_native_plugin_enablement = selected_adapters.contains(Adapter::Claude)
        && preferred_surface(Adapter::Claude)
            == PreferredSurface::PackagePluginWorkspaceMarketplace
        && native_package_plugin_keys(project_root, packages, Adapter::Claude)?.is_some();

    if !hooks.is_empty()
        || has_activation
        || has_claude_plugin_hooks
        || has_opencode_plugin_hooks
        || has_claude_native_plugin_enablement
    {
        for file in hook_files(
            project_root,
            packages,
            &hooks,
            &managed_names,
            selected_adapters,
            options.merge_existing_mcp,
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

fn adapter_uses_direct_runtime_outputs(adapter: Adapter) -> bool {
    preferred_surface(adapter) == PreferredSurface::DirectManagedOutput
}

fn native_package_marketplace_files(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    selected_adapters: Adapters,
) -> Result<Vec<ManagedFile>> {
    if packages.iter().any(|(package, _)| {
        matches!(package.source, PackageSource::Root)
            && package.manifest.manifest.workspace.is_some()
    }) {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    if selected_adapters.contains(Adapter::Claude)
        && preferred_surface(Adapter::Claude) == PreferredSurface::PackagePluginWorkspaceMarketplace
    {
        let plugins = packages
            .iter()
            .filter_map(|(package, _)| {
                native_marketplace_plugin_entry(project_root, package, Adapter::Claude)
            })
            .collect::<Vec<_>>();
        if !plugins.is_empty() {
            let (name, owner_name) = native_marketplace_names(project_root, packages);
            files.push(ManagedFile {
                path: super::native_marketplace_path(project_root, Adapter::Claude)
                    .expect("claude marketplace path"),
                contents: json_bytes(JsonMap::from_iter([
                    ("name".to_string(), JsonValue::String(name)),
                    (
                        "owner".to_string(),
                        JsonValue::Object(JsonMap::from_iter([(
                            "name".to_string(),
                            JsonValue::String(owner_name),
                        )])),
                    ),
                    ("plugins".to_string(), JsonValue::Array(plugins)),
                ]))?,
            });
        }
    }

    if selected_adapters.contains(Adapter::Codex)
        && preferred_surface(Adapter::Codex) == PreferredSurface::PackagePluginWorkspaceMarketplace
    {
        let plugins = packages
            .iter()
            .filter_map(|(package, _)| {
                native_marketplace_plugin_entry(project_root, package, Adapter::Codex)
            })
            .collect::<Vec<_>>();
        if !plugins.is_empty() {
            let (name, _) = native_marketplace_names(project_root, packages);
            files.push(ManagedFile {
                path: super::native_marketplace_path(project_root, Adapter::Codex)
                    .expect("codex marketplace path"),
                contents: json_bytes(JsonMap::from_iter([
                    ("name".to_string(), JsonValue::String(name)),
                    ("plugins".to_string(), JsonValue::Array(plugins)),
                ]))?,
            });
        }
    }

    Ok(files)
}

fn native_marketplace_plugin_entry(
    project_root: &Path,
    package: &ResolvedPackage,
    adapter: Adapter,
) -> Option<JsonValue> {
    if matches!(package.source, PackageSource::Root)
        || !package.emits_runtime_outputs()
        || !native_package_plugin_has_content(adapter, package)
    {
        return None;
    }

    let plugin_root = native_package_plugin_root(project_root, adapter, package);
    let source_path = super::native_marketplace_plugin_source_path(project_root, &plugin_root);
    let mut entry = JsonMap::from_iter([(
        "name".to_string(),
        JsonValue::String(native_package_plugin_name(package)),
    )]);
    if let Some(version) = package
        .manifest
        .effective_version()
        .map(|version| version.to_string())
    {
        entry.insert("version".to_string(), JsonValue::String(version));
    }

    match adapter {
        Adapter::Claude => {
            entry.insert("source".to_string(), JsonValue::String(source_path));
        }
        Adapter::Codex => {
            entry.insert(
                "source".to_string(),
                JsonValue::Object(JsonMap::from_iter([
                    ("source".to_string(), JsonValue::String("local".to_string())),
                    ("path".to_string(), JsonValue::String(source_path)),
                ])),
            );
            entry.insert(
                "policy".to_string(),
                JsonValue::Object(JsonMap::from_iter([
                    (
                        "installation".to_string(),
                        JsonValue::String("INSTALLED_BY_DEFAULT".to_string()),
                    ),
                    (
                        "authentication".to_string(),
                        JsonValue::String("ON_INSTALL".to_string()),
                    ),
                ])),
            );
            entry.insert(
                "category".to_string(),
                JsonValue::String("Productivity".to_string()),
            );
        }
        Adapter::Agents | Adapter::Copilot | Adapter::Cursor | Adapter::OpenCode => {
            unreachable!("only native plugin adapters have marketplace entries")
        }
    }

    Some(JsonValue::Object(entry))
}

fn native_marketplace_names(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
) -> (String, String) {
    let owner_name = packages
        .iter()
        .find(|(package, _)| matches!(package.source, PackageSource::Root))
        .map(|(package, _)| package.manifest.effective_name())
        .unwrap_or_else(|| {
            project_root
                .file_name()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| "agentpack".to_string())
        });
    (normalize_marketplace_name(&owner_name), owner_name)
}

fn normalize_marketplace_name(value: &str) -> String {
    let mut normalized = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            normalized.push(character.to_ascii_lowercase());
        } else if !normalized.ends_with('-') {
            normalized.push('-');
        }
    }

    let normalized = normalized.trim_matches('-').to_string();
    if normalized.is_empty() {
        String::from("agentpack")
    } else {
        normalized
    }
}

fn native_package_plugin_name(package: &ResolvedPackage) -> String {
    let base = if matches!(package.source, PackageSource::Root) {
        package.manifest.effective_name()
    } else {
        package.alias.clone()
    };
    normalize_marketplace_name(&base)
}

fn native_package_plugin_keys(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    adapter: Adapter,
) -> Result<Option<(String, Vec<String>)>> {
    if !matches!(adapter, Adapter::Claude | Adapter::Codex) {
        return Ok(None);
    }
    if packages.iter().any(|(package, _)| {
        matches!(package.source, PackageSource::Root)
            && package.manifest.manifest.workspace.is_some()
    }) {
        let has_enabled_members = packages
            .iter()
            .find(|(package, _)| matches!(package.source, PackageSource::Root))
            .map(|(package, _)| package.manifest.workspace_member_statuses())
            .transpose()?
            .unwrap_or_default()
            .into_iter()
            .any(|member| member.enabled);
        return Ok(
            (adapter == Adapter::Claude && has_enabled_members).then(|| {
                (
                    native_marketplace_names(project_root, packages).0,
                    Vec::new(),
                )
            }),
        );
    }

    let plugins = packages
        .iter()
        .filter(|(package, _)| {
            !matches!(package.source, PackageSource::Root)
                && package.emits_runtime_outputs()
                && native_package_plugin_has_content(adapter, package)
        })
        .map(|(package, _)| native_package_plugin_name(package))
        .collect::<Vec<_>>();
    if plugins.is_empty() {
        return Ok(None);
    }

    let (marketplace, _) = native_marketplace_names(project_root, packages);
    let keys = plugins
        .into_iter()
        .map(|plugin| format!("{plugin}@{marketplace}"))
        .collect::<Vec<_>>();
    Ok(Some((marketplace, keys)))
}

pub(crate) fn codex_user_plugin_config_file(
    path: &Path,
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
) -> Result<Option<ManagedFile>> {
    let enablement = match native_package_plugin_keys(project_root, packages, Adapter::Codex)? {
        Some(enablement) => Some(enablement),
        None => workspace_codex_marketplace(project_root, packages)?,
    };
    let Some((marketplace_name, plugin_keys)) = enablement else {
        return Ok(None);
    };
    let marketplace_source = absolute_path(&super::native_marketplace_root(project_root))?;
    let existing = if path.exists() {
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?
    } else {
        String::new()
    };
    let contents = codex_user_plugin_config_contents(
        &existing,
        &marketplace_name,
        &display_path(&marketplace_source),
        &plugin_keys,
        path,
    )?;
    if contents == existing.as_bytes() {
        return Ok(None);
    }

    Ok(Some(ManagedFile {
        path: path.to_path_buf(),
        contents,
    }))
}

fn workspace_codex_marketplace(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
) -> Result<Option<(String, Vec<String>)>> {
    let Some(root) = packages
        .iter()
        .find(|(package, _)| matches!(package.source, PackageSource::Root))
        .map(|(package, _)| package)
    else {
        return Ok(None);
    };
    if root.manifest.manifest.workspace.is_none() {
        return Ok(None);
    }

    let has_codex_members = root
        .manifest
        .workspace_member_statuses()?
        .iter()
        .any(|member| member.enabled && member.codex.is_some());
    Ok(has_codex_members.then(|| {
        (
            native_marketplace_names(project_root, packages).0,
            Vec::new(),
        )
    }))
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(env::current_dir()
            .context("failed to resolve current directory")?
            .join(path))
    }
}

fn codex_user_plugin_config_contents(
    existing: &str,
    marketplace_name: &str,
    marketplace_source: &str,
    plugin_keys: &[String],
    path: &Path,
) -> Result<Vec<u8>> {
    let mut document = if existing.trim().is_empty() {
        DocumentMut::new()
    } else {
        existing
            .parse::<DocumentMut>()
            .with_context(|| format!("failed to parse Codex user config {}", path.display()))?
    };
    let marketplaces = document
        .entry("marketplaces")
        .or_insert_with(|| EditableTomlItem::Table(EditableTomlTable::new()));
    let Some(marketplaces) = marketplaces.as_table_mut() else {
        bail!(
            "failed to merge Codex user config {}; `marketplaces` must be a TOML table",
            path.display()
        );
    };
    let marketplace = marketplaces
        .entry(marketplace_name)
        .or_insert_with(|| EditableTomlItem::Table(EditableTomlTable::new()));
    let Some(marketplace) = marketplace.as_table_mut() else {
        bail!(
            "failed to merge Codex user config {}; `marketplaces.{}` must be a TOML table",
            path.display(),
            marketplace_name
        );
    };
    marketplace["source_type"] = toml_edit::value("local");
    marketplace["source"] = toml_edit::value(marketplace_source);
    marketplace.remove("ref");
    marketplace.remove("sparse_paths");

    if !plugin_keys.is_empty() {
        let plugins = document
            .entry("plugins")
            .or_insert_with(|| EditableTomlItem::Table(EditableTomlTable::new()));
        let Some(plugins) = plugins.as_table_mut() else {
            bail!(
                "failed to merge Codex user config {}; `plugins` must be a TOML table",
                path.display()
            );
        };

        for plugin_key in plugin_keys {
            let plugin = plugins
                .entry(plugin_key)
                .or_insert_with(|| EditableTomlItem::Table(EditableTomlTable::new()));
            let Some(plugin) = plugin.as_table_mut() else {
                bail!(
                    "failed to merge Codex user config {}; `plugins.{}` must be a TOML table",
                    path.display(),
                    plugin_key
                );
            };
            plugin["enabled"] = toml_edit::value(true);
        }
    }

    let mut contents = document.to_string().into_bytes();
    if !contents.ends_with(b"\n") {
        contents.push(b'\n');
    }
    Ok(contents)
}

fn emit_native_package_plugins(
    plan: &mut OutputAccumulator,
    project_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
    selected_adapters: Adapters,
) -> Result<()> {
    if !package.emits_runtime_outputs() {
        return Ok(());
    }

    if selected_adapters.contains(Adapter::Claude)
        && preferred_surface(Adapter::Claude) == PreferredSurface::PackagePluginWorkspaceMarketplace
        && native_package_plugin_has_content(Adapter::Claude, package)
    {
        let plugin_root = native_package_plugin_root(project_root, Adapter::Claude, package);
        merge_files(
            &mut plan.files,
            claude_native_package_plugin_files(&plugin_root, package, snapshot_root)?,
        )?;
        register_native_package_plugin_root(
            project_root,
            &mut plan.managed_files,
            Adapter::Claude,
            package,
            &plugin_root,
        );
    }

    if selected_adapters.contains(Adapter::Codex)
        && preferred_surface(Adapter::Codex) == PreferredSurface::PackagePluginWorkspaceMarketplace
        && native_package_plugin_has_content(Adapter::Codex, package)
    {
        let plugin_root = native_package_plugin_root(project_root, Adapter::Codex, package);
        merge_files(
            &mut plan.files,
            codex_native_package_plugin_files(&plugin_root, package, snapshot_root)?,
        )?;
        register_native_package_plugin_root(
            project_root,
            &mut plan.managed_files,
            Adapter::Codex,
            package,
            &plugin_root,
        );
    }

    Ok(())
}

fn native_package_plugin_has_content(adapter: Adapter, package: &ResolvedPackage) -> bool {
    let manifest = &package.manifest;
    (package.selects_component(DependencyComponent::Skills)
        && artifact_supported(adapter, ArtifactKind::Skill)
        && !manifest.discovered.skills.is_empty())
        || (package.selects_component(DependencyComponent::Agents)
            && artifact_supported(adapter, ArtifactKind::Agent)
            && !manifest.discovered.selected_agents(adapter).is_empty())
        || (package.selects_component(DependencyComponent::Commands)
            && (artifact_supported(adapter, ArtifactKind::Command) || adapter == Adapter::Codex)
            && !manifest.discovered.commands.is_empty())
        || (package.selects_component(DependencyComponent::Rules)
            && artifact_supported(adapter, ArtifactKind::Rule)
            && !manifest.discovered.rules.is_empty())
        || package_has_mcp_servers(package)
}

fn native_package_plugin_root(
    project_root: &Path,
    adapter: Adapter,
    package: &ResolvedPackage,
) -> PathBuf {
    if matches!(package.source, PackageSource::Root) {
        return project_root.to_path_buf();
    }

    project_root
        .join(".nodus")
        .join("packages")
        .join(&package.alias)
        .join(match adapter {
            Adapter::Claude => "claude-plugin",
            Adapter::Codex => "codex-plugin",
            Adapter::Agents | Adapter::Copilot | Adapter::Cursor | Adapter::OpenCode => {
                unreachable!("only native plugin adapters have package plugin roots")
            }
        })
}

fn register_native_package_plugin_root(
    project_root: &Path,
    managed_files: &mut BTreeSet<String>,
    adapter: Adapter,
    package: &ResolvedPackage,
    plugin_root: &Path,
) {
    if matches!(package.source, PackageSource::Root) {
        let metadata_path = match adapter {
            Adapter::Claude => project_root.join(".claude-plugin/plugin.json"),
            Adapter::Codex => project_root.join(".codex-plugin/plugin.json"),
            Adapter::Agents | Adapter::Copilot | Adapter::Cursor | Adapter::OpenCode => {
                unreachable!("only native plugin adapters have plugin metadata")
            }
        };
        managed_files.insert(display_relative(project_root, &metadata_path));
        let runtime_root = super::runtime_root(project_root, adapter);
        if package.selects_component(DependencyComponent::Skills)
            && artifact_supported(adapter, ArtifactKind::Skill)
            && !package.manifest.discovered.skills.is_empty()
        {
            managed_files.insert(display_relative(project_root, &runtime_root.join("skills")));
        }
        if package.selects_component(DependencyComponent::Agents)
            && artifact_supported(adapter, ArtifactKind::Agent)
            && !package
                .manifest
                .discovered
                .selected_agents(adapter)
                .is_empty()
        {
            managed_files.insert(display_relative(project_root, &runtime_root.join("agents")));
        }
        if package.selects_component(DependencyComponent::Commands)
            && !package.manifest.discovered.commands.is_empty()
        {
            let directory = match adapter {
                Adapter::Claude => "commands",
                Adapter::Codex => "skills",
                Adapter::Agents | Adapter::Copilot | Adapter::Cursor | Adapter::OpenCode => {
                    unreachable!("only native plugin adapters have plugin metadata")
                }
            };
            managed_files.insert(display_relative(
                project_root,
                &runtime_root.join(directory),
            ));
        }
        if package.selects_component(DependencyComponent::Rules)
            && artifact_supported(adapter, ArtifactKind::Rule)
            && !package.manifest.discovered.rules.is_empty()
        {
            managed_files.insert(display_relative(project_root, &runtime_root.join("rules")));
        }
    } else {
        managed_files.insert(display_relative(project_root, plugin_root));
    }
}

fn claude_native_package_plugin_files(
    plugin_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
) -> Result<Vec<ManagedFile>> {
    let names = ManagedArtifactNames::from_resolved_packages([package]);
    let mut files = BTreeMap::new();

    if package.selects_component(DependencyComponent::Skills) {
        for skill in &package.manifest.discovered.skills {
            merge_files(
                &mut files,
                super::claude::skill_files(&names, plugin_root, package, snapshot_root, skill)?,
            )?;
        }
    }

    if package.selects_component(DependencyComponent::Agents) {
        for agent in package.manifest.discovered.selected_agents(Adapter::Claude) {
            merge_file(
                &mut files,
                super::claude::agent_file(&names, plugin_root, package, snapshot_root, agent)?,
            )?;
        }
    }

    if package.selects_component(DependencyComponent::Rules) {
        for rule in &package.manifest.discovered.rules {
            merge_file(
                &mut files,
                super::claude::rule_file(&names, plugin_root, package, snapshot_root, rule)?,
            )?;
        }
    }

    if package.selects_component(DependencyComponent::Commands) {
        for command in &package.manifest.discovered.commands {
            merge_file(
                &mut files,
                super::claude::command_file(&names, plugin_root, package, snapshot_root, command)?,
            )?;
        }
    }

    if package_has_mcp_servers(package) {
        merge_file(
            &mut files,
            ManagedFile {
                path: plugin_root.join(".mcp.json"),
                contents: native_package_mcp_json(package)?,
            },
        )?;
    }

    merge_file(
        &mut files,
        ManagedFile {
            path: plugin_root.join(".claude-plugin/plugin.json"),
            contents: claude_native_package_plugin_json(&names, package)?,
        },
    )?;
    Ok(managed_files_from_map(files))
}

fn codex_native_package_plugin_files(
    plugin_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
) -> Result<Vec<ManagedFile>> {
    let names = ManagedArtifactNames::from_resolved_packages([package]);
    let mut files = BTreeMap::new();

    if package.selects_component(DependencyComponent::Skills) {
        for skill in &package.manifest.discovered.skills {
            merge_files(
                &mut files,
                super::codex::skill_files(&names, plugin_root, package, snapshot_root, skill)?,
            )?;
        }
    }

    if package.selects_component(DependencyComponent::Agents) {
        for agent in package.manifest.discovered.selected_agents(Adapter::Codex) {
            merge_file(
                &mut files,
                super::codex::agent_file(&names, plugin_root, package, snapshot_root, agent)?,
            )?;
        }
    }

    if package.selects_component(DependencyComponent::Commands) {
        for command in &package.manifest.discovered.commands {
            merge_file(
                &mut files,
                super::codex::command_skill_file(
                    &names,
                    plugin_root,
                    package,
                    snapshot_root,
                    command,
                )?,
            )?;
        }
    }

    if package_has_mcp_servers(package) {
        merge_file(
            &mut files,
            ManagedFile {
                path: plugin_root.join(".mcp.json"),
                contents: native_package_mcp_json(package)?,
            },
        )?;
    }

    merge_file(
        &mut files,
        ManagedFile {
            path: plugin_root.join(".codex-plugin/plugin.json"),
            contents: codex_native_package_plugin_json(package)?,
        },
    )?;
    Ok(managed_files_from_map(files))
}

fn claude_native_package_plugin_json(
    names: &ManagedArtifactNames,
    package: &ResolvedPackage,
) -> Result<Vec<u8>> {
    let mut root = native_plugin_metadata_base(package);

    if package.selects_component(DependencyComponent::Skills)
        && !package.manifest.discovered.skills.is_empty()
    {
        root.insert("skills".into(), JsonValue::String("./skills/".to_string()));
    }

    if package.selects_component(DependencyComponent::Agents) {
        let agents = package
            .manifest
            .discovered
            .selected_agents(Adapter::Claude)
            .into_iter()
            .map(|agent| {
                JsonValue::String(format!(
                    "./agents/{}",
                    super::managed_file_name(names, package, ArtifactKind::Agent, &agent.id, "md")
                ))
            })
            .collect::<Vec<_>>();
        if !agents.is_empty() {
            root.insert("agents".into(), JsonValue::Array(agents));
        }
    }

    if package.selects_component(DependencyComponent::Commands) {
        let commands = package
            .manifest
            .discovered
            .commands
            .iter()
            .map(|command| {
                (
                    command.id.clone(),
                    JsonValue::Object(JsonMap::from_iter([(
                        "source".to_string(),
                        JsonValue::String(format!(
                            "./commands/{}",
                            super::managed_file_name(
                                names,
                                package,
                                ArtifactKind::Command,
                                &command.id,
                                "md",
                            )
                        )),
                    )])),
                )
            })
            .collect::<JsonMap<_, _>>();
        if !commands.is_empty() {
            root.insert("commands".into(), JsonValue::Object(commands));
        }
    }

    if package_has_mcp_servers(package) {
        root.insert(
            "mcpServers".into(),
            JsonValue::String("./.mcp.json".to_string()),
        );
    }

    json_bytes(root)
}

fn codex_native_package_plugin_json(package: &ResolvedPackage) -> Result<Vec<u8>> {
    let mut root = native_plugin_metadata_base(package);
    if package.selects_component(DependencyComponent::Skills)
        && (!package.manifest.discovered.skills.is_empty()
            || (package.selects_component(DependencyComponent::Commands)
                && !package.manifest.discovered.commands.is_empty()))
    {
        root.insert("skills".into(), JsonValue::String("./skills/".to_string()));
    }
    if package_has_mcp_servers(package) {
        root.insert(
            "mcpServers".into(),
            JsonValue::String("./.mcp.json".to_string()),
        );
    }
    json_bytes(root)
}

fn native_package_mcp_json(package: &ResolvedPackage) -> Result<Vec<u8>> {
    json_bytes(JsonMap::from_iter([(
        "mcpServers".to_string(),
        serde_json::to_value(&package.manifest.manifest.mcp_servers)
            .context("failed to serialize plugin MCP metadata")?,
    )]))
}

fn native_plugin_metadata_base(package: &ResolvedPackage) -> JsonMap<String, JsonValue> {
    let mut root = JsonMap::from_iter([(
        "name".to_string(),
        JsonValue::String(native_package_plugin_name(package)),
    )]);
    if let Some(version) = package
        .manifest
        .effective_version()
        .map(|version| version.to_string())
    {
        root.insert("version".to_string(), JsonValue::String(version));
    }
    root
}

fn json_bytes(root: JsonMap<String, JsonValue>) -> Result<Vec<u8>> {
    let mut contents =
        serde_json::to_vec_pretty(&JsonValue::Object(root)).context("failed to serialize JSON")?;
    contents.push(b'\n');
    Ok(contents)
}

fn managed_files_from_map(files: BTreeMap<PathBuf, Vec<u8>>) -> Vec<ManagedFile> {
    files
        .into_iter()
        .map(|(path, contents)| ManagedFile { path, contents })
        .collect()
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
    selected_adapters: Adapters,
    existing_lockfile: Option<&Lockfile>,
    merge_existing_mcp: bool,
) -> Result<Option<ManagedFile>> {
    let path = project_root.join(".mcp.json");
    let previously_managed = previously_managed_mcp_servers(existing_lockfile, ".mcp.json");
    let mut desired_servers = BTreeMap::new();
    let mut has_direct_mcp_package = false;
    for (package, _) in packages {
        if !package_has_mcp_servers(package) {
            continue;
        }
        if mcp_servers_are_emitted_by_native_plugin(Adapter::Claude, package, selected_adapters) {
            continue;
        }
        has_direct_mcp_package = true;

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

    if has_direct_mcp_package {
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

fn mcp_servers_are_emitted_by_native_plugin(
    adapter: Adapter,
    package: &ResolvedPackage,
    selected_adapters: Adapters,
) -> bool {
    selected_adapters.contains(adapter)
        && preferred_surface(adapter) == PreferredSurface::PackagePluginWorkspaceMarketplace
        && !matches!(package.source, PackageSource::Root)
        && native_package_plugin_has_content(adapter, package)
        && package_has_mcp_servers(package)
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
    selected_adapters: Adapters,
    codex_native_plugins_auto_enabled: bool,
    existing_lockfile: Option<&Lockfile>,
    merge_existing_mcp: bool,
    emit_launch_sync: bool,
) -> Result<Option<ManagedFile>> {
    let path = project_root.join(".codex/config.toml");
    let previously_managed =
        previously_managed_mcp_servers(existing_lockfile, ".codex/config.toml");
    let mut desired_servers = BTreeMap::new();
    let mut has_direct_mcp_package = false;
    for (package, _) in packages {
        let has_direct_mcp_signal = if codex_native_plugins_auto_enabled {
            package_has_mcp_servers(package)
        } else {
            package_selects_mcp(package)
        };
        if !has_direct_mcp_signal {
            continue;
        }
        if codex_native_plugins_auto_enabled
            && mcp_servers_are_emitted_by_native_plugin(Adapter::Codex, package, selected_adapters)
        {
            continue;
        }
        has_direct_mcp_package = true;

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

    if has_direct_mcp_package {
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
    config.features.remove("codex_hooks");
    if emit_launch_sync {
        config
            .features
            .insert("hooks".into(), TomlValue::Boolean(true));
    } else {
        config.features.remove("hooks");
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

fn package_has_mcp_servers(package: &ResolvedPackage) -> bool {
    package_selects_mcp(package) && !package.manifest.manifest.mcp_servers.is_empty()
}

fn warn_if_activation_unsupported(
    warnings: &mut Vec<String>,
    selected_adapters: Adapters,
    has_activation: bool,
) {
    if !has_activation {
        return;
    }

    for adapter in selected_adapters.iter() {
        if matches!(adapter, Adapter::Claude | Adapter::Codex) {
            continue;
        }
        warnings.push(format!(
            "activation context is not emitted for `{adapter}`; no supported session-start context injection surface is available"
        ));
    }
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

fn activation_hooks_for_adapter(
    packages: &[(ResolvedPackage, PathBuf)],
    names: &ManagedArtifactNames,
    adapter: Adapter,
) -> Result<Vec<ManagedActivationHook>> {
    packages
        .iter()
        .filter(|(package, _)| package.manifest.manifest.activation_enabled())
        .map(|(package, snapshot_root)| {
            Ok(ManagedActivationHook {
                package_alias: package.alias.clone(),
                context: activation_context_text(package, snapshot_root, names, adapter)?,
            })
        })
        .collect()
}

fn activation_context_text(
    package: &ResolvedPackage,
    snapshot_root: &Path,
    names: &ManagedArtifactNames,
    adapter: Adapter,
) -> Result<String> {
    let activation = package
        .manifest
        .manifest
        .activation
        .as_ref()
        .expect("activation context requires activation metadata");
    let mut context = format!("Nodus package `{}` startup context.\n", package.alias);

    for path in package
        .manifest
        .manifest
        .normalized_activation_context_paths()?
    {
        let contents = fs::read_to_string(snapshot_root.join(&path)).with_context(|| {
            format!(
                "failed to read activation context {} for `{}`",
                display_path(&path),
                package.alias
            )
        })?;
        context.push_str("\n--- Nodus activation file: ");
        context.push_str(&display_path(&path));
        context.push_str(" ---\n");
        context.push_str(&contents);
        if !contents.ends_with('\n') {
            context.push('\n');
        }
    }

    if !activation.prefer_skills.is_empty() {
        let skill_names = activation
            .prefer_skills
            .iter()
            .map(|skill_id| managed_runtime_skill_id(names, adapter, package, skill_id))
            .collect::<Vec<_>>();
        context.push_str("\nPrefer loading these Nodus-managed skills first when relevant: ");
        context.push_str(
            &skill_names
                .iter()
                .map(|skill| format!("`{skill}`"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        context.push_str(".\n");
    }

    Ok(context)
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
    managed_names: &ManagedArtifactNames,
    selected_adapters: Adapters,
    merge_existing_mcp: bool,
    warnings: &mut Vec<String>,
) -> Result<Vec<ManagedFile>> {
    let mut files = Vec::new();

    let claude_hooks = hooks_for_adapter(hooks, selected_adapters, Adapter::Claude);
    let claude_activation_hooks = if selected_adapters.contains(Adapter::Claude) {
        activation_hooks_for_adapter(packages, managed_names, Adapter::Claude)?
    } else {
        Vec::new()
    };
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
    let claude_plugin_enablement = if selected_adapters.contains(Adapter::Claude)
        && preferred_surface(Adapter::Claude) == PreferredSurface::PackagePluginWorkspaceMarketplace
    {
        native_package_plugin_keys(project_root, packages, Adapter::Claude)?
    } else {
        None
    };
    let (claude_plugin_marketplace, claude_enabled_plugins): (Option<&str>, &[String]) =
        claude_plugin_enablement
            .as_ref()
            .map_or((None, &[][..]), |(marketplace, plugins)| {
                (Some(marketplace.as_str()), plugins.as_slice())
            });
    if !claude_hooks.is_empty()
        || !claude_activation_hooks.is_empty()
        || !claude_plugin_packages.is_empty()
        || claude_plugin_marketplace.is_some()
    {
        let (claude_files, claude_warnings) = super::claude::hook_files(
            project_root,
            &claude_hooks,
            &claude_activation_hooks,
            &claude_plugin_packages,
            claude_plugin_marketplace,
            claude_enabled_plugins,
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
    let codex_activation_hooks = if selected_adapters.contains(Adapter::Codex) {
        activation_hooks_for_adapter(packages, managed_names, Adapter::Codex)?
    } else {
        Vec::new()
    };
    if !codex_hooks.is_empty() || !codex_activation_hooks.is_empty() {
        files.extend(super::codex::hook_files(
            project_root,
            &codex_hooks,
            &codex_activation_hooks,
        )?);
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
