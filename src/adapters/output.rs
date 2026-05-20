use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use toml::Value as TomlValue;

use super::profile::runtime_root_name;
use super::{
    Adapter, Adapters, ArtifactKind, MANAGED_MARKETPLACE_NAME, ManagedActivationHook,
    ManagedArtifactNames, ManagedFile, ManagedHookSpec, ManagedPackageIdentities, PreferredSurface,
    artifact_supported, hook_supported_by_adapter, managed_runtime_skill_id, preferred_surface,
};
use crate::lockfile::{Lockfile, OwnedPrefix, managed_mcp_server_name};
use crate::manifest::{
    DependencyComponent, HookSpec, LoadedManifest, McpServerConfig, load_dependency_from_dir,
};
use crate::paths::{display_path, strip_path_prefix};
use crate::resolver::{PackageSource, ResolvedPackage};

#[derive(Debug, Default)]
pub(crate) struct OutputPlan {
    pub files: Vec<ManagedFile>,
    pub external_files: Vec<ManagedFile>,
    /// Flat list of every managed entry. Kept on the struct for symmetry with
    /// the per-package view and as a backstop for ad-hoc introspection in
    /// tests; v10 writes derive lockfile ownership from
    /// [`OutputPlan::managed_files_by_package`] and the v9 read path consults
    /// `Lockfile::legacy_managed_files` directly.
    #[allow(dead_code)]
    pub managed_files: Vec<String>,
    /// Per-package attribution of every entry in `managed_files`. Slice 3
    /// feeds this into the per-package `owned_subtrees`/`owned_prefixes`/
    /// `owned_files` lockfile fields. Empty entries for packages with no
    /// owned outputs are omitted.
    pub managed_files_by_package: Vec<PackageOwnedPaths>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PackageOwnedPaths {
    pub alias: String,
    /// Subtree roots Nodus owns wholesale (e.g.
    /// `.nodus/packages/<alias>/claude-plugin`).
    pub subtrees: Vec<String>,
    /// Filename-prefix rules — directory + filename prefix. One rule per
    /// (dir, prefix) pair; multiple hook files sharing a (dir, prefix) collapse
    /// into a single entry.
    pub prefixes: Vec<OwnedPrefix>,
    /// Exact paths Nodus owns (singletons that don't fit subtrees or prefix
    /// rules).
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct OutputPlanOptions {
    pub merge_existing_mcp: bool,
    pub codex_native_plugins_auto_enabled: bool,
    pub codex_user_config: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
struct NativePackagePluginHooks<'a> {
    claude: &'a [ManagedHookSpec],
    codex: &'a [ManagedHookSpec],
}

#[derive(Debug, Clone, Copy)]
struct CodexFeatureRequirements {
    hooks: bool,
    plugin_hooks: bool,
}

#[derive(Debug)]
struct CodexMarketplaceRegistration {
    marketplace: String,
    enabled_plugins: Vec<String>,
}

#[derive(Debug, Default)]
struct OutputAccumulator {
    files: BTreeMap<PathBuf, Vec<u8>>,
    external_files: BTreeMap<PathBuf, Vec<u8>>,
    /// Flat list of managed entries. Mirrors the pre-Slice-3 shape (uses raw
    /// artifact IDs for entries like `.claude/skills/<id>`) so v9 lockfile
    /// reads keep producing the same expanded paths through
    /// `Lockfile::managed_paths`. v10 ownership is tracked separately in
    /// `owned_by_package` using actual on-disk paths.
    managed_files: BTreeSet<String>,
    /// Per-package attribution using actual on-disk paths. Populated alongside
    /// the flat set as files are emitted; emitted as
    /// [`OutputPlan::managed_files_by_package`] after the build completes.
    owned_by_package: BTreeMap<String, PackageOwnedAccumulator>,
    warnings: Vec<String>,
}

#[derive(Debug, Default)]
struct PackageOwnedAccumulator {
    subtrees: BTreeSet<String>,
    /// Deduped on `(dir, prefix)`. Multiple hook files matching the same
    /// `(dir, prefix)` collapse into a single rule by virtue of insertion into
    /// this set.
    prefixes: BTreeSet<(String, String)>,
    files: BTreeSet<String>,
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    marketplaces: BTreeMap<String, TomlValue>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    plugins: BTreeMap<String, TomlValue>,
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
            codex_user_config: None,
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
    let package_identities = ManagedPackageIdentities::from_resolved_packages(
        packages.iter().map(|(package, _)| package),
    );
    let hooks = collected_hooks(packages);
    let has_activation = packages
        .iter()
        .any(|(package, _)| package.manifest.manifest.activation_enabled());
    let codex_prefers_native_plugins =
        preferred_surface(Adapter::Codex) == PreferredSurface::PackagePluginWorkspaceMarketplace;
    let codex_plugin_hook_packages: BTreeSet<String> =
        if selected_adapters.contains(Adapter::Codex) && codex_prefers_native_plugins {
            packages
                .iter()
                .filter(|(package, _)| package_emits_codex_plugin_hooks(package))
                .map(|(package, _)| package.alias.clone())
                .collect()
        } else {
            BTreeSet::new()
        };
    let emit_codex_plugin_hooks = !codex_plugin_hook_packages.is_empty();
    let emit_codex_workspace_hooks = selected_adapters.contains(Adapter::Codex)
        && (hooks.iter().any(|hook| {
            hook_targets_adapter(&hook.hook, selected_adapters, Adapter::Codex)
                && hook_supported_by_adapter(&hook.hook, Adapter::Codex)
                && (hook.emitted_from_root
                    || !codex_plugin_hook_packages.contains(&hook.package_alias))
        }) || packages.iter().any(|(package, _)| {
            package.manifest.manifest.activation_enabled()
                && !codex_plugin_hook_packages.contains(&package.alias)
        }));
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
                track_owned_skill_root(
                    &mut plan,
                    project_root,
                    &managed_names,
                    Adapter::Agents,
                    package,
                    &skill.id,
                );
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
                track_owned_skill_root(
                    &mut plan,
                    project_root,
                    &managed_names,
                    Adapter::Claude,
                    package,
                    &skill.id,
                );
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
                track_owned_skill_root(
                    &mut plan,
                    project_root,
                    &managed_names,
                    Adapter::Codex,
                    package,
                    &skill.id,
                );
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
                track_owned_skill_root(
                    &mut plan,
                    project_root,
                    &managed_names,
                    Adapter::Copilot,
                    package,
                    &skill.id,
                );
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
                track_owned_skill_root(
                    &mut plan,
                    project_root,
                    &managed_names,
                    Adapter::Cursor,
                    package,
                    &skill.id,
                );
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
                track_owned_skill_root(
                    &mut plan,
                    project_root,
                    &managed_names,
                    Adapter::OpenCode,
                    package,
                    &skill.id,
                );
            }
        }

        if package.selects_component(DependencyComponent::Agents) {
            if selected_adapters.contains(Adapter::Claude)
                && adapter_uses_direct_runtime_outputs(Adapter::Claude)
                && artifact_supported(Adapter::Claude, ArtifactKind::Agent)
            {
                for agent in package.manifest.discovered.selected_agents(Adapter::Claude) {
                    let agent_file = super::claude::agent_file(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        agent,
                    )?;
                    track_owned_file(&mut plan, project_root, &package.alias, &agent_file.path);
                    merge_file(&mut plan.files, agent_file)?;
                    plan.managed_files
                        .insert(format!(".claude/agents/{}.md", agent.id));
                }
            }

            if selected_adapters.contains(Adapter::Codex)
                && artifact_supported(Adapter::Codex, ArtifactKind::Agent)
            {
                for agent in package.manifest.discovered.selected_agents(Adapter::Codex) {
                    let agent_file = super::codex::agent_file(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        agent,
                    )?;
                    let on_disk_entry = display_relative(project_root, &agent_file.path);
                    track_owned_file(&mut plan, project_root, &package.alias, &agent_file.path);
                    merge_file(&mut plan.files, agent_file)?;
                    plan.managed_files.insert(on_disk_entry);
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
                    let agent_file = super::copilot::agent_file(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        agent,
                    )?;
                    track_owned_file(&mut plan, project_root, &package.alias, &agent_file.path);
                    merge_file(&mut plan.files, agent_file)?;
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
                    let agent_file = super::opencode::agent_file(
                        &managed_names,
                        project_root,
                        package,
                        snapshot_root,
                        agent,
                    )?;
                    track_owned_file(&mut plan, project_root, &package.alias, &agent_file.path);
                    merge_file(&mut plan.files, agent_file)?;
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
                let rule_file = super::claude::rule_file(
                    &managed_names,
                    project_root,
                    package,
                    snapshot_root,
                    rule,
                )?;
                track_owned_file(&mut plan, project_root, &package.alias, &rule_file.path);
                merge_file(&mut plan.files, rule_file)?;
                plan.managed_files
                    .insert(format!(".claude/rules/{}.md", rule.id));
            }

            if selected_adapters.contains(Adapter::OpenCode)
                && artifact_supported(Adapter::OpenCode, ArtifactKind::Rule)
            {
                let rule_file = super::opencode::rule_file(
                    &managed_names,
                    project_root,
                    package,
                    snapshot_root,
                    rule,
                )?;
                track_owned_file(&mut plan, project_root, &package.alias, &rule_file.path);
                merge_file(&mut plan.files, rule_file)?;
                plan.managed_files
                    .insert(format!(".opencode/rules/{}.md", rule.id));
            }

            if selected_adapters.contains(Adapter::Cursor)
                && artifact_supported(Adapter::Cursor, ArtifactKind::Rule)
            {
                let rule_file = super::cursor::rule_file(
                    &managed_names,
                    project_root,
                    package,
                    snapshot_root,
                    rule,
                )?;
                track_owned_file(&mut plan, project_root, &package.alias, &rule_file.path);
                merge_file(&mut plan.files, rule_file)?;
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
                let command_file = super::agents::command_file(
                    &managed_names,
                    project_root,
                    package,
                    snapshot_root,
                    command,
                )?;
                track_owned_file(&mut plan, project_root, &package.alias, &command_file.path);
                merge_file(&mut plan.files, command_file)?;
                plan.managed_files
                    .insert(format!(".agents/commands/{}.md", command.id));
            }

            if selected_adapters.contains(Adapter::Claude)
                && adapter_uses_direct_runtime_outputs(Adapter::Claude)
                && artifact_supported(Adapter::Claude, ArtifactKind::Command)
            {
                let command_file = super::claude::command_file(
                    &managed_names,
                    project_root,
                    package,
                    snapshot_root,
                    command,
                )?;
                track_owned_file(&mut plan, project_root, &package.alias, &command_file.path);
                merge_file(&mut plan.files, command_file)?;
                plan.managed_files
                    .insert(format!(".claude/commands/{}.md", command.id));
            }

            if selected_adapters.contains(Adapter::Codex)
                && adapter_uses_direct_runtime_outputs(Adapter::Codex)
            {
                let skill_id =
                    super::codex::synthetic_command_skill_id(&managed_names, package, &command.id);
                let command_file = super::codex::command_skill_file(
                    &managed_names,
                    project_root,
                    package,
                    snapshot_root,
                    command,
                )?;
                // The command-skill file lives under `<runtime>/skills/<skill_id>/SKILL.md`.
                // Its parent (the skill dir) is what Nodus owns as a subtree.
                if let Some(skill_root) = command_file.path.parent() {
                    track_owned_subtree(&mut plan, project_root, &package.alias, skill_root);
                }
                merge_file(&mut plan.files, command_file)?;
                plan.managed_files
                    .insert(format!(".codex/skills/{skill_id}"));
            }

            if selected_adapters.contains(Adapter::Cursor)
                && artifact_supported(Adapter::Cursor, ArtifactKind::Command)
            {
                let command_file = super::cursor::command_file(
                    &managed_names,
                    project_root,
                    package,
                    snapshot_root,
                    command,
                )?;
                track_owned_file(&mut plan, project_root, &package.alias, &command_file.path);
                merge_file(&mut plan.files, command_file)?;
                plan.managed_files
                    .insert(format!(".cursor/commands/{}.md", command.id));
            }

            if selected_adapters.contains(Adapter::OpenCode)
                && artifact_supported(Adapter::OpenCode, ArtifactKind::Command)
            {
                let command_file = super::opencode::command_file(
                    &managed_names,
                    project_root,
                    package,
                    snapshot_root,
                    command,
                )?;
                track_owned_file(&mut plan, project_root, &package.alias, &command_file.path);
                merge_file(&mut plan.files, command_file)?;
                plan.managed_files
                    .insert(format!(".opencode/commands/{}.md", command.id));
            }
        }

        let package_claude_plugin_hooks = if selected_adapters.contains(Adapter::Claude)
            && package_emits_claude_plugin_hooks(package)
        {
            hooks_for_package_and_adapter(&hooks, &package.alias, Adapter::Claude)
        } else {
            Vec::new()
        };
        let package_codex_plugin_hooks = if selected_adapters.contains(Adapter::Codex)
            && package_emits_codex_plugin_hooks(package)
        {
            hooks_for_package_and_adapter(&hooks, &package.alias, Adapter::Codex)
        } else {
            Vec::new()
        };
        emit_native_package_plugins(
            &mut plan,
            project_root,
            package,
            snapshot_root,
            selected_adapters,
            &managed_names,
            &package_identities,
            NativePackagePluginHooks {
                claude: &package_claude_plugin_hooks,
                codex: &package_codex_plugin_hooks,
            },
        )?;

        merge_files(
            &mut plan.files,
            managed_path_files(project_root, package, snapshot_root)?,
        )?;
        register_managed_paths(project_root, &mut plan, package)?;
    }

    let workspace_alias = workspace_owner_alias(packages);

    for file in native_package_marketplace_files(
        project_root,
        packages,
        selected_adapters,
        &package_identities,
    )? {
        merge_file(&mut plan.external_files, file)?;
    }

    if let Some(file) = mcp_config_file(
        project_root,
        packages,
        selected_adapters,
        existing_lockfile,
        options.merge_existing_mcp,
    )? {
        track_owned_file(&mut plan, project_root, &workspace_alias, &file.path);
        plan.managed_files
            .insert(display_relative(project_root, &file.path));
        merge_file(&mut plan.files, file)?;
    }
    if let Some(file) = codex_mcp_config_file(
        project_root,
        packages,
        selected_adapters,
        options.codex_native_plugins_auto_enabled,
        existing_lockfile,
        options.merge_existing_mcp,
        CodexFeatureRequirements {
            hooks: emit_codex_workspace_hooks,
            plugin_hooks: emit_codex_plugin_hooks,
        },
    )? {
        track_owned_file(&mut plan, project_root, &workspace_alias, &file.path);
        plan.managed_files
            .insert(display_relative(project_root, &file.path));
        merge_file(&mut plan.files, file)?;
    }
    if let Some(file) = codex_user_config_file(
        project_root,
        packages,
        selected_adapters,
        options.merge_existing_mcp,
        options.codex_user_config.as_deref(),
    )? {
        merge_file(&mut plan.external_files, file)?;
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
        track_owned_file(&mut plan, project_root, &workspace_alias, &file.path);
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
    let has_virtual_plugins = has_virtual_plugin_outputs(
        project_root,
        packages,
        selected_adapters,
        &package_identities,
    )?;
    let has_claude_native_plugin_enablement = selected_adapters.contains(Adapter::Claude)
        && preferred_surface(Adapter::Claude)
            == PreferredSurface::PackagePluginWorkspaceMarketplace
        && native_package_plugin_keys(
            project_root,
            packages,
            Adapter::Claude,
            &package_identities,
        )?
        .is_some();

    if !hooks.is_empty()
        || has_activation
        || has_claude_plugin_hooks
        || has_virtual_plugins
        || has_claude_native_plugin_enablement
    {
        // Pre-compute per-package ownership for every hook file we're about
        // to emit. We do this here (against the typed hooks list) rather than
        // by parsing filenames after the fact, because the script stems for
        // root hooks and per-package hooks are not reliably distinguishable
        // from filename alone (a root hook with id `nodus.sync_on_startup`
        // produces `nodus-hook-nodus-sync-on-startup-<digest>.sh`, which a
        // naive parser would attribute to a nonexistent `nodus` package).
        attribute_hook_owned_paths(
            &mut plan,
            project_root,
            packages,
            &hooks,
            &workspace_alias,
            selected_adapters,
            &package_identities,
        )?;

        let emitted_hook_files = hook_files(
            project_root,
            packages,
            &hooks,
            &managed_names,
            selected_adapters,
            options.merge_existing_mcp,
            &mut plan.warnings,
            &package_identities,
        )?;

        // Any hook-emitted file we did not pre-attribute (e.g. shared aggregator
        // outputs like `.opencode/plugins/nodus-hooks.js`, the
        // `.claude/settings.json` config blob, the codex hooks JSON, plugin
        // wrappers we already covered via `track_*` helpers) falls back to the
        // workspace owner as an exact owned file.
        for file in emitted_hook_files {
            if is_global_nodus_path(project_root, &file.path)
                || strip_path_prefix(&file.path, project_root).is_none()
            {
                merge_file(&mut plan.external_files, file)?;
                continue;
            }
            let relative = display_relative(project_root, &file.path);
            if !already_attributed(&plan, project_root, &file.path) {
                track_owned_file(&mut plan, project_root, &workspace_alias, &file.path);
            }
            plan.managed_files.insert(relative);
            merge_file(&mut plan.files, file)?;
        }
    }

    let gitignores = gitignore_files(project_root, &plan.files)?;
    for consumed in gitignores.consumed_inputs {
        plan.files.remove(&consumed);
    }
    for file in gitignores.files {
        track_owned_file(&mut plan, project_root, &workspace_alias, &file.path);
        plan.managed_files
            .insert(display_relative(project_root, &file.path));
        merge_file(&mut plan.files, file)?;
    }

    prune_redundant_owned_paths(&mut plan.owned_by_package);

    let managed_files: Vec<String> = plan.managed_files.iter().cloned().collect();
    let managed_files_by_package = plan
        .owned_by_package
        .into_iter()
        .filter_map(|(alias, bucket)| {
            if bucket.subtrees.is_empty() && bucket.prefixes.is_empty() && bucket.files.is_empty() {
                None
            } else {
                Some(PackageOwnedPaths {
                    alias,
                    subtrees: bucket.subtrees.into_iter().collect(),
                    prefixes: bucket
                        .prefixes
                        .into_iter()
                        .map(|(dir, prefix)| OwnedPrefix { dir, prefix })
                        .collect(),
                    files: bucket.files.into_iter().collect(),
                })
            }
        })
        .collect::<Vec<_>>();

    #[cfg(debug_assertions)]
    debug_assert_owned_paths_cover_planned_files(
        &plan.files.keys().cloned().collect::<Vec<_>>(),
        project_root,
        &managed_files_by_package,
    );

    Ok(OutputPlan {
        files: plan
            .files
            .into_iter()
            .map(|(path, contents)| ManagedFile { path, contents })
            .collect(),
        external_files: plan
            .external_files
            .into_iter()
            .map(|(path, contents)| ManagedFile { path, contents })
            .collect(),
        managed_files,
        managed_files_by_package,
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
    package_identities: &ManagedPackageIdentities,
) -> Result<Vec<ManagedFile>> {
    if let Some(root) = packages
        .iter()
        .find(|(package, _)| {
            matches!(package.source, PackageSource::Root)
                && package.manifest.manifest.workspace.is_some()
        })
        .map(|(package, _)| &package.manifest)
    {
        return workspace_native_marketplace_files(project_root, root, selected_adapters);
    }

    let mut files = Vec::new();
    if selected_adapters.contains(Adapter::Claude)
        && preferred_surface(Adapter::Claude) == PreferredSurface::PackagePluginWorkspaceMarketplace
    {
        let plugins = packages
            .iter()
            .filter_map(|(package, _)| {
                native_marketplace_plugin_entry(
                    project_root,
                    package,
                    Adapter::Claude,
                    package_identities,
                )
            })
            .collect::<Vec<_>>();
        if !plugins.is_empty() {
            let owner_name = native_marketplace_owner_name(project_root, packages);
            files.push(ManagedFile {
                path: super::native_marketplace_path(project_root, Adapter::Claude)
                    .expect("claude marketplace path"),
                contents: json_bytes(JsonMap::from_iter([
                    (
                        "name".to_string(),
                        JsonValue::String(MANAGED_MARKETPLACE_NAME.to_string()),
                    ),
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

    if selected_adapters.contains(Adapter::Codex) {
        let plugins = packages
            .iter()
            .filter_map(|(package, _)| {
                native_marketplace_plugin_entry(
                    project_root,
                    package,
                    Adapter::Codex,
                    package_identities,
                )
            })
            .collect::<Vec<_>>();
        if !plugins.is_empty() {
            files.push(ManagedFile {
                path: super::native_marketplace_path(project_root, Adapter::Codex)
                    .expect("codex marketplace path"),
                contents: json_bytes(JsonMap::from_iter([
                    (
                        "name".to_string(),
                        JsonValue::String(MANAGED_MARKETPLACE_NAME.to_string()),
                    ),
                    ("plugins".to_string(), JsonValue::Array(plugins)),
                ]))?,
            });
        }
    }

    Ok(files)
}

fn workspace_native_marketplace_files(
    project_root: &Path,
    root: &LoadedManifest,
    selected_adapters: Adapters,
) -> Result<Vec<ManagedFile>> {
    let members = root
        .workspace_member_statuses()?
        .into_iter()
        .filter(|member| member.enabled)
        .collect::<Vec<_>>();
    if members.is_empty() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    let marketplace_name = MANAGED_MARKETPLACE_NAME.to_string();

    if selected_adapters.contains(Adapter::Claude)
        && preferred_surface(Adapter::Claude) == PreferredSurface::PackagePluginWorkspaceMarketplace
    {
        let owner_name = workspace_marketplace_owner_name(root);
        let plugins = members
            .iter()
            .map(|member| {
                let member_root = root.resolve_path(&member.path)?;
                let manifest = load_dependency_from_dir(&member_root)?;
                let mut value = JsonMap::from_iter([
                    (
                        "name".to_string(),
                        JsonValue::String(workspace_member_marketplace_plugin_name(
                            root,
                            member,
                            || manifest.effective_name(),
                        )),
                    ),
                    (
                        "source".to_string(),
                        JsonValue::String(super::native_marketplace_plugin_source_path(
                            &root.root,
                            Adapter::Claude,
                            &member_root,
                        )),
                    ),
                ]);
                if let Some(version) = manifest
                    .effective_version()
                    .map(|version| version.to_string())
                {
                    value.insert("version".to_string(), JsonValue::String(version));
                }
                Ok(JsonValue::Object(value))
            })
            .collect::<Result<Vec<_>>>()?;
        files.push(ManagedFile {
            path: super::native_marketplace_path(project_root, Adapter::Claude)
                .expect("claude marketplace path"),
            contents: json_bytes(JsonMap::from_iter([
                (
                    "name".to_string(),
                    JsonValue::String(marketplace_name.clone()),
                ),
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

    if selected_adapters.contains(Adapter::Codex) {
        let plugins = members
            .iter()
            .map(|member| {
                let Some(codex) = member.codex.as_ref() else {
                    return Ok(None);
                };
                let member_root = root.resolve_path(&member.path)?;
                Ok(Some(JsonValue::Object(JsonMap::from_iter([
                    (
                        "name".to_string(),
                        JsonValue::String(workspace_member_marketplace_plugin_name(
                            root,
                            member,
                            || member.id.clone(),
                        )),
                    ),
                    (
                        "source".to_string(),
                        JsonValue::Object(JsonMap::from_iter([
                            ("source".to_string(), JsonValue::String("local".to_string())),
                            (
                                "path".to_string(),
                                JsonValue::String(super::native_marketplace_plugin_source_path(
                                    &root.root,
                                    Adapter::Codex,
                                    &member_root,
                                )),
                            ),
                        ])),
                    ),
                    (
                        "policy".to_string(),
                        JsonValue::Object(JsonMap::from_iter([
                            (
                                "installation".to_string(),
                                JsonValue::String(codex.installation.clone()),
                            ),
                            (
                                "authentication".to_string(),
                                JsonValue::String(codex.authentication.clone()),
                            ),
                        ])),
                    ),
                    (
                        "category".to_string(),
                        JsonValue::String(codex.category.clone()),
                    ),
                ]))))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        if !plugins.is_empty() {
            files.push(ManagedFile {
                path: super::native_marketplace_path(project_root, Adapter::Codex)
                    .expect("codex marketplace path"),
                contents: json_bytes(JsonMap::from_iter([
                    ("name".to_string(), JsonValue::String(marketplace_name)),
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
    package_identities: &ManagedPackageIdentities,
) -> Option<JsonValue> {
    if matches!(package.source, PackageSource::Root)
        || !package.emits_runtime_outputs()
        || !native_package_plugin_has_content(adapter, package)
    {
        return None;
    }

    let plugin_root =
        super::native_package_plugin_root(project_root, adapter, package, package_identities);
    let source_path =
        super::native_marketplace_plugin_source_path(project_root, adapter, &plugin_root);
    let mut entry = JsonMap::from_iter([(
        "name".to_string(),
        JsonValue::String(package_identities.marketplace_plugin_name(package)),
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

fn native_marketplace_owner_name(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
) -> String {
    packages
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
        })
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
    super::normalized_package_plugin_name(package)
}

fn native_package_plugin_keys(
    _project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    adapter: Adapter,
    package_identities: &ManagedPackageIdentities,
) -> Result<Option<(String, Vec<String>)>> {
    if !matches!(adapter, Adapter::Claude | Adapter::Codex) {
        return Ok(None);
    }
    if packages.iter().any(|(package, _)| {
        matches!(package.source, PackageSource::Root)
            && package.manifest.manifest.workspace.is_some()
    }) {
        let Some(root) = packages
            .iter()
            .find(|(package, _)| matches!(package.source, PackageSource::Root))
            .map(|(package, _)| package)
        else {
            return Ok(None);
        };
        let enabled_members = root
            .manifest
            .workspace_member_statuses()?
            .into_iter()
            .filter(|member| member.enabled)
            .collect::<Vec<_>>();
        if enabled_members.is_empty() {
            return Ok(None);
        }

        let marketplace = MANAGED_MARKETPLACE_NAME.to_string();
        return match adapter {
            Adapter::Claude => Ok(Some((marketplace, Vec::new()))),
            Adapter::Codex => {
                let plugin_keys = enabled_members
                    .iter()
                    .filter(|member| {
                        member
                            .codex
                            .as_ref()
                            .is_some_and(|codex| codex.installation != "NOT_AVAILABLE")
                    })
                    .map(|member| {
                        let plugin = workspace_member_codex_plugin_name(&root.manifest, member);
                        format!("{plugin}@{marketplace}")
                    })
                    .collect::<Vec<_>>();
                let has_codex_members = enabled_members.iter().any(|member| member.codex.is_some());
                Ok(has_codex_members.then_some((marketplace, plugin_keys)))
            }
            Adapter::Agents | Adapter::Copilot | Adapter::Cursor | Adapter::OpenCode => Ok(None),
        };
    }

    let plugins = packages
        .iter()
        .filter(|(package, _)| {
            !matches!(package.source, PackageSource::Root)
                && package.emits_runtime_outputs()
                && native_package_plugin_has_content(adapter, package)
        })
        .map(|(package, _)| package_identities.marketplace_plugin_name(package))
        .collect::<Vec<_>>();
    if plugins.is_empty() {
        return Ok(None);
    }

    let marketplace = MANAGED_MARKETPLACE_NAME.to_string();
    let keys = plugins
        .into_iter()
        .map(|plugin| format!("{plugin}@{marketplace}"))
        .collect::<Vec<_>>();
    Ok(Some((marketplace, keys)))
}

fn workspace_member_codex_plugin_name(
    root: &LoadedManifest,
    member: &crate::manifest::WorkspaceMemberStatus,
) -> String {
    workspace_member_marketplace_plugin_name(root, member, || member.id.clone())
}

fn workspace_member_marketplace_plugin_name(
    root: &LoadedManifest,
    member: &crate::manifest::WorkspaceMemberStatus,
    default_name: impl FnOnce() -> String,
) -> String {
    if root
        .manifest
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.namespace.as_ref())
        .is_some()
    {
        normalize_marketplace_name(&member.alias)
    } else {
        member.name.clone().unwrap_or_else(default_name)
    }
}

fn workspace_marketplace_owner_name(root: &LoadedManifest) -> String {
    root.manifest
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| workspace_marketplace_root_basename(&root.root))
}

fn workspace_marketplace_root_basename(root: &Path) -> String {
    root.file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| String::from("agentpack"))
}

#[allow(clippy::too_many_arguments)]
fn emit_native_package_plugins(
    plan: &mut OutputAccumulator,
    project_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
    selected_adapters: Adapters,
    managed_names: &ManagedArtifactNames,
    package_identities: &ManagedPackageIdentities,
    plugin_hooks: NativePackagePluginHooks<'_>,
) -> Result<()> {
    if !package.emits_runtime_outputs() {
        return Ok(());
    }

    if selected_adapters.contains(Adapter::Claude)
        && preferred_surface(Adapter::Claude) == PreferredSurface::PackagePluginWorkspaceMarketplace
        && native_package_plugin_has_content(Adapter::Claude, package)
    {
        let plugin_root = super::native_package_plugin_root(
            project_root,
            Adapter::Claude,
            package,
            package_identities,
        );
        let activation_hook = if package_emits_claude_plugin_hooks(package)
            && package.manifest.manifest.activation_enabled()
        {
            Some(ManagedActivationHook {
                package_alias: package.alias.clone(),
                context: activation_context_text(
                    package,
                    snapshot_root,
                    managed_names,
                    Adapter::Claude,
                )?,
            })
        } else {
            None
        };
        let plugin_files = claude_native_package_plugin_files(
            &plugin_root,
            package,
            snapshot_root,
            plugin_hooks.claude,
            activation_hook.as_ref(),
        )?;
        if matches!(package.source, PackageSource::Root) {
            merge_files(&mut plan.files, plugin_files)?;
            register_native_package_plugin_root(
                project_root,
                plan,
                Adapter::Claude,
                package,
                &plugin_root,
            );
        } else {
            merge_files(&mut plan.external_files, plugin_files)?;
        }
    }

    if selected_adapters.contains(Adapter::Codex)
        && (preferred_surface(Adapter::Codex)
            == PreferredSurface::PackagePluginWorkspaceMarketplace
            || !matches!(package.source, PackageSource::Root))
        && native_package_plugin_has_content(Adapter::Codex, package)
    {
        let plugin_root = super::native_package_plugin_root(
            project_root,
            Adapter::Codex,
            package,
            package_identities,
        );
        let activation_hook = if package_emits_codex_plugin_hooks(package)
            && package.manifest.manifest.activation_enabled()
        {
            Some(ManagedActivationHook {
                package_alias: package.alias.clone(),
                context: activation_context_text(
                    package,
                    snapshot_root,
                    managed_names,
                    Adapter::Codex,
                )?,
            })
        } else {
            None
        };
        let plugin_files = codex_native_package_plugin_files(
            &plugin_root,
            package,
            snapshot_root,
            plugin_hooks.codex,
            activation_hook.as_ref(),
        )?;
        if preferred_surface(Adapter::Codex) == PreferredSurface::PackagePluginWorkspaceMarketplace
        {
            merge_files(&mut plan.files, plugin_files)?;
            register_native_package_plugin_root(
                project_root,
                plan,
                Adapter::Codex,
                package,
                &plugin_root,
            );
        } else {
            merge_files(&mut plan.external_files, plugin_files)?;
        }
    }

    Ok(())
}

fn native_package_plugin_has_content(adapter: Adapter, package: &ResolvedPackage) -> bool {
    let manifest = &package.manifest;
    (package.selects_component(DependencyComponent::Skills)
        && artifact_supported(adapter, ArtifactKind::Skill)
        && !manifest.discovered.skills.is_empty())
        || (adapter != Adapter::Codex
            && package.selects_component(DependencyComponent::Agents)
            && artifact_supported(adapter, ArtifactKind::Agent)
            && !manifest.discovered.selected_agents(adapter).is_empty())
        || (package.selects_component(DependencyComponent::Commands)
            && (artifact_supported(adapter, ArtifactKind::Command) || adapter == Adapter::Codex)
            && !manifest.discovered.commands.is_empty())
        || (package.selects_component(DependencyComponent::Rules)
            && artifact_supported(adapter, ArtifactKind::Rule)
            && !manifest.discovered.rules.is_empty())
        || package_has_mcp_servers(package)
        || (adapter == Adapter::Claude && !manifest.claude_plugin_native_components().is_empty())
        || (adapter == Adapter::Claude && manifest.claude_plugin_native_metadata().is_some())
        || (adapter == Adapter::Claude
            && !matches!(package.source, PackageSource::Root)
            && (package_has_claude_targeted_hooks(package)
                || package.manifest.manifest.activation_enabled()))
        || (adapter == Adapter::Codex
            && !matches!(package.source, PackageSource::Root)
            && (package_has_codex_targeted_hooks(package)
                || package.manifest.manifest.activation_enabled()))
}

/// True when a non-root package's portable hooks (and activation context)
/// should be emitted inside its Claude plugin folder instead of the workspace
/// `.claude/settings.json`.
///
/// Root manifests describe the workspace itself, so their hooks continue to
/// live in workspace settings even though Nodus also publishes the root as a
/// Claude plugin.
fn package_emits_claude_plugin_hooks(package: &ResolvedPackage) -> bool {
    if matches!(package.source, PackageSource::Root) {
        return false;
    }
    if !package.emits_runtime_outputs() {
        return false;
    }
    if preferred_surface(Adapter::Claude) != PreferredSurface::PackagePluginWorkspaceMarketplace {
        return false;
    }
    native_package_plugin_has_content(Adapter::Claude, package)
        && (package_has_claude_targeted_hooks(package)
            || package.manifest.manifest.activation_enabled())
}

fn package_emits_codex_plugin_hooks(package: &ResolvedPackage) -> bool {
    if matches!(package.source, PackageSource::Root) {
        return false;
    }
    if !package.emits_runtime_outputs() {
        return false;
    }
    if preferred_surface(Adapter::Codex) != PreferredSurface::PackagePluginWorkspaceMarketplace {
        return false;
    }
    native_package_plugin_has_content(Adapter::Codex, package)
        && (package_has_codex_targeted_hooks(package)
            || package.manifest.manifest.activation_enabled())
}

fn package_has_claude_targeted_hooks(package: &ResolvedPackage) -> bool {
    package
        .manifest
        .manifest
        .hooks
        .iter()
        .any(|hook| hook_targets_claude(hook) && hook_supported_by_adapter(hook, Adapter::Claude))
}

fn package_has_codex_targeted_hooks(package: &ResolvedPackage) -> bool {
    package
        .manifest
        .manifest
        .hooks
        .iter()
        .any(|hook| hook_targets_codex(hook) && hook_supported_by_adapter(hook, Adapter::Codex))
}

fn hook_targets_claude(hook: &HookSpec) -> bool {
    hook.adapters.is_empty() || hook.adapters.contains(&Adapter::Claude)
}

fn hook_targets_codex(hook: &HookSpec) -> bool {
    hook.adapters.is_empty() || hook.adapters.contains(&Adapter::Codex)
}

fn hooks_for_package_and_adapter(
    hooks: &[ManagedHookSpec],
    package_alias: &str,
    adapter: Adapter,
) -> Vec<ManagedHookSpec> {
    hooks
        .iter()
        .filter(|hook| hook.package_alias == package_alias && !hook.emitted_from_root)
        .filter(|hook| hook.hook.adapters.is_empty() || hook.hook.adapters.contains(&adapter))
        .filter(|hook| hook_supported_by_adapter(&hook.hook, adapter))
        .cloned()
        .collect()
}

fn register_native_package_plugin_root(
    project_root: &Path,
    plan: &mut OutputAccumulator,
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
        plan.managed_files
            .insert(display_relative(project_root, &metadata_path));
        track_owned_file(plan, project_root, &package.alias, &metadata_path);

        let runtime_root = super::runtime_root(project_root, adapter);
        if package.selects_component(DependencyComponent::Skills)
            && artifact_supported(adapter, ArtifactKind::Skill)
            && !package.manifest.discovered.skills.is_empty()
        {
            let dir = runtime_root.join("skills");
            plan.managed_files
                .insert(display_relative(project_root, &dir));
            track_owned_subtree(plan, project_root, &package.alias, &dir);
        }
        if package.selects_component(DependencyComponent::Agents)
            && artifact_supported(adapter, ArtifactKind::Agent)
            && !package
                .manifest
                .discovered
                .selected_agents(adapter)
                .is_empty()
        {
            let dir = runtime_root.join("agents");
            plan.managed_files
                .insert(display_relative(project_root, &dir));
            track_owned_subtree(plan, project_root, &package.alias, &dir);
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
            let dir = runtime_root.join(directory);
            plan.managed_files
                .insert(display_relative(project_root, &dir));
            track_owned_subtree(plan, project_root, &package.alias, &dir);
        }
        if package.selects_component(DependencyComponent::Rules)
            && artifact_supported(adapter, ArtifactKind::Rule)
            && !package.manifest.discovered.rules.is_empty()
        {
            let dir = runtime_root.join("rules");
            plan.managed_files
                .insert(display_relative(project_root, &dir));
            track_owned_subtree(plan, project_root, &package.alias, &dir);
        }
        if package_has_mcp_servers(package) {
            let mcp = project_root.join(".mcp.json");
            plan.managed_files
                .insert(display_relative(project_root, &mcp));
            track_owned_file(plan, project_root, &package.alias, &mcp);
        }
    } else {
        plan.managed_files
            .insert(display_relative(project_root, plugin_root));
        track_owned_subtree(plan, project_root, &package.alias, plugin_root);
    }
}

fn claude_native_package_plugin_files(
    plugin_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
    hooks: &[ManagedHookSpec],
    activation_hook: Option<&ManagedActivationHook>,
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

    merge_files(
        &mut files,
        claude_native_passthrough_files(plugin_root, package, snapshot_root)?,
    )?;

    let hook_emission =
        super::claude::plugin_native_hook_files(plugin_root, package, hooks, activation_hook)?;
    for file in hook_emission.files {
        merge_file(&mut files, file)?;
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

fn claude_native_passthrough_files(
    plugin_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
) -> Result<Vec<ManagedFile>> {
    let native_components = package.manifest.claude_plugin_native_components();
    if native_components.is_empty() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for package_file in package.manifest.package_files()? {
        let relative = strip_path_prefix(&package_file, &package.manifest.root)
            .with_context(|| format!("failed to make {} relative", package_file.display()))?;
        if !native_components
            .iter()
            .any(|component| relative == component || relative.starts_with(component))
        {
            continue;
        }
        files.push(ManagedFile {
            path: plugin_root.join(relative),
            contents: fs::read(snapshot_root.join(relative)).with_context(|| {
                format!(
                    "failed to read snapshot file {}",
                    snapshot_root.join(relative).display()
                )
            })?,
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn codex_native_package_plugin_files(
    plugin_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
    hooks: &[ManagedHookSpec],
    activation_hook: Option<&ManagedActivationHook>,
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

    // Codex's plugin format does not declare agents; they are emitted directly
    // under `.codex/agents/` by `build_output_plan` instead.

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

    let hook_emission =
        super::codex::plugin_native_hook_files(plugin_root, package, hooks, activation_hook)?;
    for file in hook_emission.files {
        merge_file(&mut files, file)?;
    }

    merge_file(
        &mut files,
        ManagedFile {
            path: plugin_root.join(".codex-plugin/plugin.json"),
            contents: codex_native_package_plugin_json(package, hook_emission.has_hooks_json)?,
        },
    )?;
    Ok(managed_files_from_map(files))
}

fn claude_native_package_plugin_json(
    names: &ManagedArtifactNames,
    package: &ResolvedPackage,
) -> Result<Vec<u8>> {
    let mut root = native_plugin_metadata_base(package);
    let prefix = native_package_plugin_artifact_prefix(Adapter::Claude, package);

    if package.selects_component(DependencyComponent::Skills)
        && !package.manifest.discovered.skills.is_empty()
    {
        root.insert(
            "skills".into(),
            JsonValue::String(format!("{prefix}skills/")),
        );
    }

    if package.selects_component(DependencyComponent::Agents) {
        let agents = package
            .manifest
            .discovered
            .selected_agents(Adapter::Claude)
            .into_iter()
            .map(|agent| {
                JsonValue::String(format!(
                    "{prefix}agents/{}",
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
                            "{prefix}commands/{}",
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

    if let Some(metadata) = package.manifest.claude_plugin_native_metadata() {
        for (key, value) in metadata {
            root.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }

    json_bytes(root)
}

fn codex_native_package_plugin_json(
    package: &ResolvedPackage,
    has_plugin_hooks_json: bool,
) -> Result<Vec<u8>> {
    let mut root = native_plugin_metadata_base(package);
    let prefix = native_package_plugin_artifact_prefix(Adapter::Codex, package);
    if has_plugin_hooks_json {
        root.insert(
            "hooks".into(),
            JsonValue::String(format!("{prefix}{}", super::codex::PLUGIN_HOOKS_JSON_PATH)),
        );
    }
    if package.selects_component(DependencyComponent::Skills)
        && (!package.manifest.discovered.skills.is_empty()
            || (package.selects_component(DependencyComponent::Commands)
                && !package.manifest.discovered.commands.is_empty()))
    {
        root.insert(
            "skills".into(),
            JsonValue::String(format!("{prefix}skills/")),
        );
    }
    if package_has_mcp_servers(package) {
        root.insert(
            "mcpServers".into(),
            JsonValue::String("./.mcp.json".to_string()),
        );
    }
    json_bytes(root)
}

fn native_package_plugin_artifact_prefix(adapter: Adapter, package: &ResolvedPackage) -> String {
    if matches!(package.source, PackageSource::Root) {
        format!("./{}/", runtime_root_name(adapter))
    } else {
        "./".to_string()
    }
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
    plan: &mut OutputAccumulator,
    package: &ResolvedPackage,
) -> Result<()> {
    use crate::resolver::ResolvedManagedPathOrigin;

    for mapping in package.managed_paths() {
        validate_direct_managed_root(project_root, &plan.managed_files, &mapping.ownership_root)?;
        let ownership_root = project_root.join(&mapping.ownership_root);
        plan.managed_files
            .insert(display_relative(project_root, &ownership_root));

        // Per-package ownership for managed paths depends on the mapping
        // origin:
        //
        // - **`PackageManagedExport { placement = "package" }`** — the entire
        //   ownership root lives under `.nodus/packages/<alias>/` and Nodus
        //   owns the whole directory tree. Track as a subtree so subtree
        //   cleanup can prune any file Nodus didn't write.
        //
        // - **`PackageManagedExport { placement = "project" }`** — the
        //   ownership root is a user-visible directory (e.g. `learnings`).
        //   Pre-Slice-3 behavior treated the directory as owned at the
        //   leaf-grain by listing the root only (the v9 compression dropped
        //   individual file entries), and the on-disk cleanup pass used the
        //   directory-mismatch logic in `planned_paths_to_replace` to wipe
        //   any non-planned content. We mirror that by tracking the root as
        //   an exact owned file (so `path_is_owned` returns true) without
        //   the individual file entries.
        //
        // - **`LegacyDependencyMapping`** — the consumer manifest's legacy
        //   `[managed]` block. v9 left individual file entries intact in
        //   `legacy_managed_files`, which had the side effect that user-
        //   placed files inside the directory survived re-sync. We mirror
        //   that here by tracking each individual `file.target_relative` as
        //   an exact owned file.
        match mapping.origin {
            ResolvedManagedPathOrigin::PackageManagedExport {
                placement: crate::manifest::ManagedPlacement::Package,
            } => {
                track_owned_subtree(plan, project_root, &package.alias, &ownership_root);
            }
            ResolvedManagedPathOrigin::PackageManagedExport {
                placement: crate::manifest::ManagedPlacement::Project,
            } => {
                track_owned_file(plan, project_root, &package.alias, &ownership_root);
            }
            ResolvedManagedPathOrigin::LegacyDependencyMapping => {
                for file in &mapping.files {
                    let target = project_root.join(&file.target_relative);
                    track_owned_file(plan, project_root, &package.alias, &target);
                }
            }
        }
        for file in &mapping.files {
            let target = project_root.join(&file.target_relative);
            plan.managed_files
                .insert(display_relative(project_root, &target));
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
        && package.emits_runtime_outputs()
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
    feature_requirements: CodexFeatureRequirements,
) -> Result<Option<ManagedFile>> {
    let path = project_root.join(".codex/config.toml");
    let previously_managed =
        previously_managed_mcp_servers(existing_lockfile, ".codex/config.toml");
    let mut desired_servers = BTreeMap::new();
    let mut has_direct_mcp_package = false;
    if selected_adapters.contains(Adapter::Codex) {
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
                && mcp_servers_are_emitted_by_native_plugin(
                    Adapter::Codex,
                    package,
                    selected_adapters,
                )
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

    let managed_marketplace = Some(MANAGED_MARKETPLACE_NAME.to_string());
    let legacy_marketplace = legacy_project_marketplace_name(project_root, packages);
    let needs_config_for_outputs = !desired_servers.is_empty()
        || !previously_managed.is_empty()
        || feature_requirements.hooks
        || feature_requirements.plugin_hooks;
    let needs_marketplace_cleanup = if needs_config_for_outputs {
        false
    } else {
        codex_project_config_has_marketplace_entries(&path, managed_marketplace.as_deref())?
            || (managed_marketplace.as_deref() != Some(legacy_marketplace.as_str())
                && codex_project_config_has_marketplace_entries(&path, Some(&legacy_marketplace))?)
    };

    if !needs_config_for_outputs && !needs_marketplace_cleanup {
        return Ok(None);
    }

    let mut config = if merge_existing_mcp && path.exists() {
        read_project_codex_config(&path)?
    } else {
        ProjectCodexConfig::default()
    };

    if let Some(marketplace) = managed_marketplace.as_deref() {
        remove_codex_marketplace_config(&mut config, marketplace);
    }
    if managed_marketplace.as_deref() != Some(legacy_marketplace.as_str()) {
        remove_codex_marketplace_config(&mut config, &legacy_marketplace);
    }

    config.mcp_servers.retain(|server_name, _| {
        !previously_managed.contains(server_name) && !desired_servers.contains_key(server_name)
    });
    config.mcp_servers.extend(desired_servers);
    config.features.remove("codex_hooks");
    if feature_requirements.hooks {
        config
            .features
            .insert("hooks".into(), TomlValue::Boolean(true));
    } else {
        config.features.remove("hooks");
    }
    if feature_requirements.plugin_hooks {
        config
            .features
            .insert("plugin_hooks".into(), TomlValue::Boolean(true));
    } else {
        config.features.remove("plugin_hooks");
    }

    if config.mcp_servers.is_empty()
        && config.features.is_empty()
        && config.marketplaces.is_empty()
        && config.plugins.is_empty()
        && config.extra.is_empty()
        && !needs_marketplace_cleanup
    {
        return Ok(None);
    }

    let mut contents = toml::to_string_pretty(&config)
        .context("failed to serialize managed Codex MCP configuration")?
        .into_bytes();
    contents.push(b'\n');
    Ok(Some(ManagedFile { path, contents }))
}

fn codex_user_config_file(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    selected_adapters: Adapters,
    merge_existing_mcp: bool,
    codex_user_config: Option<&Path>,
) -> Result<Option<ManagedFile>> {
    let path = codex_user_config
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_codex_user_config_path(project_root));
    let plugin_registration =
        codex_plugin_marketplace_registration(project_root, packages, selected_adapters)?;
    let legacy_marketplace = legacy_project_marketplace_name(project_root, packages);
    let needs_registration = plugin_registration.is_some();
    let needs_cleanup = if needs_registration {
        false
    } else {
        codex_project_config_has_marketplace_entries(&path, Some(MANAGED_MARKETPLACE_NAME))?
            || (legacy_marketplace != MANAGED_MARKETPLACE_NAME
                && codex_project_config_has_marketplace_entries(&path, Some(&legacy_marketplace))?)
    };

    if !needs_registration && !needs_cleanup {
        return Ok(None);
    }

    let mut config = if merge_existing_mcp && path.exists() {
        read_project_codex_config(&path)?
    } else {
        ProjectCodexConfig::default()
    };
    remove_codex_marketplace_config(&mut config, MANAGED_MARKETPLACE_NAME);
    if legacy_marketplace != MANAGED_MARKETPLACE_NAME {
        remove_codex_marketplace_config(&mut config, &legacy_marketplace);
    }

    if let Some(registration) = plugin_registration {
        let source = absolute_codex_marketplace_source(project_root)?;
        config.marketplaces.insert(
            registration.marketplace.clone(),
            codex_local_marketplace_config(source),
        );
        for plugin in registration.enabled_plugins {
            config.plugins.insert(plugin, codex_enabled_plugin_config());
        }
    }

    if config.mcp_servers.is_empty()
        && config.features.is_empty()
        && config.marketplaces.is_empty()
        && config.plugins.is_empty()
        && config.extra.is_empty()
        && !needs_cleanup
    {
        return Ok(None);
    }

    let mut contents = toml::to_string_pretty(&config)
        .context("failed to serialize managed Codex user configuration")?
        .into_bytes();
    contents.push(b'\n');
    Ok(Some(ManagedFile { path, contents }))
}

fn legacy_project_marketplace_name(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
) -> String {
    normalize_marketplace_name(&native_marketplace_owner_name(project_root, packages))
}

fn remove_codex_marketplace_config(config: &mut ProjectCodexConfig, marketplace: &str) {
    let suffix = format!("@{marketplace}");
    config.plugins.retain(|key, _| !key.ends_with(&suffix));
    config.marketplaces.remove(marketplace);
}

fn codex_project_config_has_marketplace_entries(
    path: &Path,
    marketplace: Option<&str>,
) -> Result<bool> {
    let Some(marketplace) = marketplace else {
        return Ok(false);
    };
    if !path.exists() {
        return Ok(false);
    }

    let Ok(config) = read_project_codex_config(path) else {
        return Ok(false);
    };
    let suffix = format!("@{marketplace}");
    Ok(config.marketplaces.contains_key(marketplace)
        || config.plugins.keys().any(|key| key.ends_with(&suffix)))
}

fn codex_plugin_marketplace_registration(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    selected_adapters: Adapters,
) -> Result<Option<CodexMarketplaceRegistration>> {
    if !selected_adapters.contains(Adapter::Codex)
        || preferred_surface(Adapter::Codex) != PreferredSurface::PackagePluginWorkspaceMarketplace
    {
        return Ok(None);
    }

    let Some((marketplace, enabled_plugins)) = native_package_plugin_keys(
        project_root,
        packages,
        Adapter::Codex,
        &ManagedPackageIdentities::from_resolved_packages(
            packages.iter().map(|(package, _)| package),
        ),
    )?
    else {
        return Ok(None);
    };

    Ok(Some(CodexMarketplaceRegistration {
        marketplace,
        enabled_plugins,
    }))
}

fn absolute_codex_marketplace_source(project_root: &Path) -> Result<String> {
    let source = super::native_marketplace_root(project_root, Adapter::Codex);
    let absolute = if source.is_absolute() {
        source
    } else {
        env::current_dir()
            .context("failed to resolve current directory for Codex marketplace source")?
            .join(source)
    };
    let simplified = dunce::simplified(&absolute);
    Ok(display_path(simplified))
}

fn default_codex_user_config_path(project_root: &Path) -> PathBuf {
    if let Some(codex_home) = env::var_os("CODEX_HOME") {
        return PathBuf::from(codex_home).join("config.toml");
    }

    #[cfg(test)]
    {
        project_root.join(".codex-user/config.toml")
    }

    #[cfg(all(not(test), target_os = "windows"))]
    {
        if let Some(profile) = env::var_os("USERPROFILE") {
            return PathBuf::from(profile).join(".codex/config.toml");
        }
        if let (Some(drive), Some(path)) = (env::var_os("HOMEDRIVE"), env::var_os("HOMEPATH")) {
            return PathBuf::from(drive).join(path).join(".codex/config.toml");
        }
    }

    #[cfg(all(not(test), not(target_os = "windows")))]
    {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home).join(".codex/config.toml");
        }
    }

    #[cfg(not(test))]
    project_root.join(".codex-user/config.toml")
}

fn codex_local_marketplace_config(source: String) -> TomlValue {
    let mut table = toml::map::Map::new();
    table.insert("source_type".into(), TomlValue::String("local".into()));
    table.insert("source".into(), TomlValue::String(source));
    TomlValue::Table(table)
}

fn codex_enabled_plugin_config() -> TomlValue {
    let mut table = toml::map::Map::new();
    table.insert("enabled".into(), TomlValue::Boolean(true));
    TomlValue::Table(table)
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
        // v9 stored the config path in `legacy_managed_files`; v10 stores it in
        // some package's per-package `owned_files`. Either signal means Nodus
        // previously owned the file and may have written the unprefixed
        // `nodus` MCP server entry that we need to clean up.
        lockfile
            .legacy_managed_files
            .iter()
            .any(|managed_file| managed_file == config_path)
            || lockfile
                .packages
                .iter()
                .any(|package| package.owned_files.iter().any(|file| file == config_path))
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
    let Some(relative) = strip_path_prefix(path, project_root) else {
        return Ok(None);
    };
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
    let Some(relative) = strip_path_prefix(path, project_root) else {
        return Ok(None);
    };
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

#[allow(clippy::too_many_arguments)]
fn hook_files(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    hooks: &[ManagedHookSpec],
    managed_names: &ManagedArtifactNames,
    selected_adapters: Adapters,
    merge_existing_mcp: bool,
    warnings: &mut Vec<String>,
    package_identities: &ManagedPackageIdentities,
) -> Result<Vec<ManagedFile>> {
    let mut files = Vec::new();

    let claude_plugin_hook_packages: BTreeSet<String> = packages
        .iter()
        .filter(|(package, _)| package_emits_claude_plugin_hooks(package))
        .map(|(package, _)| package.alias.clone())
        .collect();
    let claude_hooks = hooks_for_adapter(hooks, selected_adapters, Adapter::Claude)
        .into_iter()
        .filter(|hook| {
            // Non-root hooks for packages that emit their own plugin hooks.json
            // are owned by the plugin; skip them here so they don't double up
            // in the workspace `.claude/settings.json`.
            hook.emitted_from_root || !claude_plugin_hook_packages.contains(&hook.package_alias)
        })
        .collect::<Vec<_>>();
    let claude_activation_hooks = if selected_adapters.contains(Adapter::Claude) {
        activation_hooks_for_adapter(packages, managed_names, Adapter::Claude)?
            .into_iter()
            .filter(|hook| !claude_plugin_hook_packages.contains(&hook.package_alias))
            .collect::<Vec<_>>()
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
        native_package_plugin_keys(project_root, packages, Adapter::Claude, package_identities)?
    } else {
        None
    };
    let (claude_plugin_marketplace, claude_enabled_plugins): (Option<&str>, &[String]) =
        claude_plugin_enablement
            .as_ref()
            .map_or((None, &[][..]), |(marketplace, plugins)| {
                (Some(marketplace.as_str()), plugins.as_slice())
            });
    let claude_plugin_marketplace_source = claude_plugin_marketplace
        .map(|_| super::native_marketplace_source_path(project_root, Adapter::Claude));
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
            claude_plugin_marketplace_source.as_deref(),
            claude_enabled_plugins,
            merge_existing_mcp,
            package_identities,
        )?;
        files.extend(claude_files);
        warnings.extend(claude_warnings);
    }
    let opencode_hooks = hooks_for_adapter(hooks, selected_adapters, Adapter::OpenCode);
    if !opencode_hooks.is_empty() {
        files.extend(super::opencode::hook_files(project_root, &opencode_hooks));
    }
    files.extend(virtual_plugin_files(
        project_root,
        packages,
        selected_adapters,
        package_identities,
    )?);
    if hooks
        .iter()
        .any(|hook| hook_targets_adapter(&hook.hook, selected_adapters, Adapter::Agents))
    {
        warnings.push(
            "hooks are not emitted for `agents`; no documented project hook surface is available"
                .into(),
        );
    }
    let codex_plugin_hook_packages: BTreeSet<String> = packages
        .iter()
        .filter(|(package, _)| {
            selected_adapters.contains(Adapter::Codex) && package_emits_codex_plugin_hooks(package)
        })
        .map(|(package, _)| package.alias.clone())
        .collect();
    let codex_hooks = hooks_for_adapter(hooks, selected_adapters, Adapter::Codex)
        .into_iter()
        .filter(|hook| {
            hook.emitted_from_root || !codex_plugin_hook_packages.contains(&hook.package_alias)
        })
        .collect::<Vec<_>>();
    let codex_activation_hooks = if selected_adapters.contains(Adapter::Codex) {
        activation_hooks_for_adapter(packages, managed_names, Adapter::Codex)?
            .into_iter()
            .filter(|hook| !codex_plugin_hook_packages.contains(&hook.package_alias))
            .collect::<Vec<_>>()
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

fn has_virtual_plugin_outputs(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    selected_adapters: Adapters,
    package_identities: &ManagedPackageIdentities,
) -> Result<bool> {
    for adapter in selected_adapters.iter() {
        if adapter == Adapter::Codex {
            continue;
        }
        let Some(backend) = super::virtual_plugin_backend(adapter) else {
            continue;
        };
        for (package, _) in packages {
            if super::virtual_plugin_install_root_for_package(
                backend,
                project_root,
                package,
                package_identities,
            )?
            .is_some()
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn virtual_plugin_files(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    selected_adapters: Adapters,
    package_identities: &ManagedPackageIdentities,
) -> Result<Vec<ManagedFile>> {
    let mut files = Vec::new();
    let plugin_packages = packages
        .iter()
        .map(|(package, snapshot_root)| (package, snapshot_root.as_path()))
        .collect::<Vec<_>>();

    for adapter in selected_adapters.iter() {
        if adapter == Adapter::Codex {
            continue;
        }
        let Some(backend) = super::virtual_plugin_backend(adapter) else {
            continue;
        };
        files.extend(super::emit_virtual_plugin_files(
            project_root,
            backend,
            &plugin_packages,
            package_identities,
        )?);
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

/// Alias that owns workspace-level shared outputs (`.mcp.json`, marketplace
/// JSONs, `.claude/settings.json`, generated `.gitignore` files, aggregator
/// plugin entrypoints like `.opencode/plugins/nodus-hooks.js`). Falls back to
/// `"root"` when no root package is in the resolution (vanishingly rare — we
/// always synthesize a root manifest — but keeps the per-package emission
/// total even in that degenerate case).
fn workspace_owner_alias(packages: &[(ResolvedPackage, PathBuf)]) -> String {
    packages
        .iter()
        .find(|(package, _)| matches!(package.source, PackageSource::Root))
        .map(|(package, _)| package.alias.clone())
        .unwrap_or_else(|| "root".to_string())
}

/// Attribute an actual on-disk file path to `alias` as a per-package
/// [`PackageOwnedPaths::files`] entry. The path is normalized to a string
/// relative to `project_root` via [`display_relative`] so it matches the form
/// stored in the lockfile.
fn track_owned_file(
    plan: &mut OutputAccumulator,
    project_root: &Path,
    alias: &str,
    file_path: &Path,
) {
    let entry = display_relative(project_root, file_path);
    plan.owned_by_package
        .entry(alias.to_string())
        .or_default()
        .files
        .insert(entry);
}

/// Attribute a directory `subtree_path` (on disk) to `alias` as a per-package
/// [`PackageOwnedPaths::subtrees`] entry. Use this for skill dirs, native
/// plugin folders, and other directories Nodus owns wholesale.
fn track_owned_subtree(
    plan: &mut OutputAccumulator,
    project_root: &Path,
    alias: &str,
    subtree_path: &Path,
) {
    let entry = display_relative(project_root, subtree_path);
    plan.owned_by_package
        .entry(alias.to_string())
        .or_default()
        .subtrees
        .insert(entry);
}

/// Attribute a directory-and-filename-prefix rule to `alias` as a per-package
/// [`PackageOwnedPaths::prefixes`] entry. Multiple inserts with the same
/// `(dir, prefix)` for the same `alias` collapse into a single rule.
fn track_owned_prefix(plan: &mut OutputAccumulator, alias: &str, dir: String, prefix: String) {
    plan.owned_by_package
        .entry(alias.to_string())
        .or_default()
        .prefixes
        .insert((dir, prefix));
}

/// Drop exact-file and filename-prefix claims covered by any owned subtree.
/// The ownership model treats subtrees as the strongest claim: a subtree owns
/// every descendant, and install-digest attribution consults subtrees before
/// exact files or prefix rules. Keeping narrower claims under those subtrees
/// only produces redundant lockfile warnings.
fn prune_redundant_owned_paths(owned_by_package: &mut BTreeMap<String, PackageOwnedAccumulator>) {
    let subtrees = owned_by_package
        .values()
        .flat_map(|bucket| {
            bucket
                .subtrees
                .iter()
                .map(|subtree| Path::new(subtree).to_owned())
        })
        .collect::<Vec<_>>();

    if subtrees.is_empty() {
        return;
    }

    for bucket in owned_by_package.values_mut() {
        bucket.subtrees.retain(|subtree| {
            let subtree = Path::new(subtree);
            !subtrees
                .iter()
                .any(|parent| subtree != parent.as_path() && subtree.starts_with(parent))
        });
        bucket.files.retain(|file| {
            let file = Path::new(file);
            !subtrees
                .iter()
                .any(|subtree| file != subtree.as_path() && file.starts_with(subtree))
        });
        bucket.prefixes.retain(|(dir, _)| {
            let dir = Path::new(dir);
            !subtrees.iter().any(|subtree| dir.starts_with(subtree))
        });
    }
}

/// Convenience wrapper for the common skill-root case: derive the on-disk
/// skill root from `managed_names` and attribute it to `package.alias` as an
/// owned subtree. Used by every per-adapter skill emission site so the
/// per-package ownership reflects the disambiguated runtime path (e.g.
/// `.claude/skills/review_abc`) rather than the raw `skill.id`.
fn track_owned_skill_root(
    plan: &mut OutputAccumulator,
    project_root: &Path,
    managed_names: &ManagedArtifactNames,
    adapter: Adapter,
    package: &ResolvedPackage,
    skill_id: &str,
) {
    let skill_root =
        super::managed_skill_root(managed_names, project_root, adapter, package, skill_id);
    track_owned_subtree(plan, project_root, &package.alias, &skill_root);
}

/// Pre-compute per-package ownership for every hook-related file the
/// downstream `hook_files` call will emit. We attribute against the typed
/// hooks list so we can reliably tell root hooks (their file stems are derived
/// from the hook id, with no package segment) from non-root hooks (file stems
/// contain a sanitized package alias). Filename parsing alone can't do this —
/// a root hook named `nodus.sync_on_startup` produces a script with stem
/// `nodus-hook-nodus-sync-on-startup-<digest>`, which a naive parser would
/// attribute to a nonexistent `nodus` package.
///
/// For each hook we emit:
/// - **Root hooks** → an exact owned file (Claude/Codex hook scripts and
///   their per-package activation hooks; OpenCode hook scripts).
/// - **Non-root hooks** → an owned-prefix rule keyed on the package alias.
///   Multiple hooks for the same `(package, dir)` collapse to a single rule.
///
/// We also attribute the Claude plugin-hook bridge scripts
/// (`nodus-plugin-hook-<alias>-...`) and the OpenCode plugin wrapper files
/// (`.opencode/plugins/nodus-<alias>-...js`, plus the per-package plugin
/// install root under `.nodus/packages/<alias>/opencode-plugin`).
fn attribute_hook_owned_paths(
    plan: &mut OutputAccumulator,
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    hooks: &[ManagedHookSpec],
    workspace_alias: &str,
    selected_adapters: Adapters,
    package_identities: &ManagedPackageIdentities,
) -> Result<()> {
    let codex_plugin_hook_packages: BTreeSet<String> = packages
        .iter()
        .filter(|(package, _)| {
            selected_adapters.contains(Adapter::Codex) && package_emits_codex_plugin_hooks(package)
        })
        .map(|(package, _)| package.alias.clone())
        .collect();

    for hook in hooks {
        attribute_single_hook(
            plan,
            project_root,
            hook,
            workspace_alias,
            selected_adapters,
            &codex_plugin_hook_packages,
        );
    }

    // Activation hooks always come from non-root packages — Claude attaches
    // them to the package plugin folder (which already owns its subtree) when
    // emitted_from_root is false, but the workspace `.claude/hooks/` /
    // `.codex/hooks/` activation scripts are owned via prefix rules.
    for (package, _) in packages {
        if !package.manifest.manifest.activation_enabled()
            || matches!(package.source, PackageSource::Root)
        {
            continue;
        }
        let alias = &package.alias;
        if selected_adapters.contains(Adapter::Claude) {
            track_owned_prefix(
                plan,
                alias,
                ".claude/hooks".to_string(),
                format!("nodus-hook-activation-{alias}-"),
            );
        }
        if selected_adapters.contains(Adapter::Codex) && !codex_plugin_hook_packages.contains(alias)
        {
            track_owned_prefix(
                plan,
                alias,
                ".codex/hooks".to_string(),
                format!("nodus-hook-activation-{alias}-"),
            );
        }
    }

    // Claude plugin-hook bridge scripts and OpenCode plugin wrappers live
    // alongside the package plugin install roots. The install roots
    // themselves are tracked elsewhere (via the native plugin emission and
    // the OpenCode plugin emission), so here we only need the workspace-
    // visible bridge scripts that share a directory with non-root hooks.
    for (package, _) in packages {
        if matches!(package.source, PackageSource::Root) {
            continue;
        }
        let alias = &package.alias;
        if selected_adapters.contains(Adapter::Claude)
            && !package
                .manifest
                .claude_plugin_hook_compat_sources()
                .is_empty()
        {
            track_owned_prefix(
                plan,
                alias,
                ".claude/hooks".to_string(),
                format!("nodus-plugin-hook-{alias}-"),
            );
        }
    }

    for (package, _) in packages {
        for adapter in selected_adapters.iter() {
            if adapter == Adapter::Codex {
                continue;
            }
            let Some(backend) = super::virtual_plugin_backend(adapter) else {
                continue;
            };
            attribute_virtual_plugin_owned_paths(
                plan,
                project_root,
                package,
                backend,
                package_identities,
            )?;
        }
    }

    // `.opencode/plugins/nodus-hooks.js` is a workspace aggregator emitted
    // whenever any OpenCode hooks exist. Attribute to the workspace owner so
    // sync's cleanup logic still recognises it as Nodus-owned.
    let any_opencode_hooks = hooks.iter().any(|hook| {
        hook_targets_adapter(&hook.hook, selected_adapters, Adapter::OpenCode)
            && hook_supported_by_adapter(&hook.hook, Adapter::OpenCode)
    });
    if any_opencode_hooks {
        track_owned_file(
            plan,
            project_root,
            workspace_alias,
            &project_root.join(".opencode/plugins/nodus-hooks.js"),
        );
    }

    Ok(())
}

fn attribute_virtual_plugin_owned_paths(
    plan: &mut OutputAccumulator,
    project_root: &Path,
    package: &ResolvedPackage,
    backend: &dyn super::VirtualPluginBackend,
    package_identities: &ManagedPackageIdentities,
) -> Result<()> {
    let Some(install_root) = super::virtual_plugin_install_root_for_package(
        backend,
        project_root,
        package,
        package_identities,
    )?
    else {
        return Ok(());
    };
    let entries = super::virtual_plugin_entries_for_package(
        backend,
        project_root,
        package,
        package_identities,
    )?;

    if !is_global_nodus_path(project_root, &install_root)
        && (strip_path_prefix(&install_root, project_root).is_some() || !install_root.is_absolute())
    {
        let install_root = if install_root.is_absolute() {
            install_root
        } else {
            project_root.join(install_root)
        };
        track_owned_subtree(plan, project_root, &package.alias, &install_root);
    }
    if !entries.is_empty() {
        track_owned_prefix(
            plan,
            &package.alias,
            backend.surface().loader_dir.to_string(),
            backend.loader_file_prefix(package),
        );
    }
    Ok(())
}

fn is_global_nodus_path(project_root: &Path, path: &Path) -> bool {
    let global_home = super::global_nodus_home(project_root);
    path == global_home || path.starts_with(global_home)
}

fn attribute_single_hook(
    plan: &mut OutputAccumulator,
    project_root: &Path,
    hook: &ManagedHookSpec,
    workspace_alias: &str,
    selected_adapters: Adapters,
    codex_plugin_hook_packages: &BTreeSet<String>,
) {
    for adapter in [Adapter::Claude, Adapter::Codex, Adapter::OpenCode] {
        if !hook_targets_adapter(&hook.hook, selected_adapters, adapter)
            || !hook_supported_by_adapter(&hook.hook, adapter)
        {
            continue;
        }
        if adapter == Adapter::Codex
            && !hook.emitted_from_root
            && codex_plugin_hook_packages.contains(&hook.package_alias)
        {
            continue;
        }
        let (dir, stem_prefix_for_package) = match adapter {
            Adapter::Claude => (".claude/hooks", "nodus-hook-"),
            Adapter::Codex => (".codex/hooks", "nodus-hook-"),
            Adapter::OpenCode => (".opencode/scripts", "nodus-hook-"),
            _ => continue,
        };
        if hook.emitted_from_root {
            // Root hooks: each script's stem is `nodus-hook-<sanitized-hook-id>-<digest>`,
            // and the sanitized hook id can collide with a non-root prefix
            // (e.g. id `nodus.sync_on_startup` → stem
            // `nodus-hook-nodus-sync-on-startup-...`). Use exact ownership.
            let stem = hook_script_stem_for_adapter(hook, adapter);
            let script_path = project_root.join(dir).join(format!("{stem}.sh"));
            track_owned_file(plan, project_root, workspace_alias, &script_path);
        } else {
            // Non-root hooks: every script for this package in this directory
            // shares the prefix `nodus-hook-<alias>-`. Multiple hooks collapse
            // into a single (dir, prefix) rule via the BTreeSet dedupe.
            let sanitized_alias = sanitized_alias_segment(&hook.package_alias);
            track_owned_prefix(
                plan,
                &hook.package_alias,
                dir.to_string(),
                format!("{stem_prefix_for_package}{sanitized_alias}-"),
            );
        }
        // Codex's shared config is attributed via `codex_mcp_config_file`;
        // hook scripts and plugin hook roots are attributed above.
        let _ = adapter;
    }
}

/// Stem matching the per-adapter `managed_script_stem` helpers used by
/// `claude::hook_files` / `codex::hook_files` / `opencode::hook_files`. We
/// reimplement the format here (rather than reaching into each adapter
/// module) so per-package ownership can be computed BEFORE we call those
/// emitters.
fn hook_script_stem_for_adapter(hook: &ManagedHookSpec, adapter: Adapter) -> String {
    use blake3::hash;
    let _ = adapter;
    let sanitized = sanitized_alias_segment(&hook.hook.id);
    if hook.emitted_from_root {
        format!(
            "nodus-hook-{sanitized}-{}",
            &hash(hook.hook.id.as_bytes()).to_hex()[..8]
        )
    } else {
        let package = sanitized_alias_segment(&hook.package_alias);
        format!(
            "nodus-hook-{package}-{sanitized}-{}",
            &hash(format!("{}:{}", hook.package_alias, hook.hook.id).as_bytes()).to_hex()[..8]
        )
    }
}

/// Sanitize a string for use as a script-stem segment. Mirrors the per-adapter
/// sanitizers in `claude.rs` / `codex.rs` / `opencode.rs`: lowercase ASCII
/// alphanumerics pass through, everything else becomes `-`.
fn sanitized_alias_segment(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            'a'..='z' | '0'..='9' => character,
            'A'..='Z' => character.to_ascii_lowercase(),
            _ => '-',
        })
        .collect()
}

/// True when `file_path` is already covered by a per-package ownership rule
/// in `plan.owned_by_package` (as an exact owned file, a subtree the file
/// lives under, or a prefix rule matching its parent + filename stem). Used
/// by the hooks emission tail to skip re-attributing files we already
/// described via `attribute_hook_owned_paths`.
fn already_attributed(plan: &OutputAccumulator, project_root: &Path, file_path: &Path) -> bool {
    let Some(relative) = strip_path_prefix(file_path, project_root) else {
        return false;
    };
    let relative_str = display_path(relative);
    for bucket in plan.owned_by_package.values() {
        if bucket.files.contains(&relative_str) {
            return true;
        }
        if bucket
            .subtrees
            .iter()
            .any(|subtree| relative.starts_with(Path::new(subtree)))
        {
            return true;
        }
        if bucket.prefixes.iter().any(|(dir, prefix)| {
            relative.parent() == Some(Path::new(dir))
                && relative
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(prefix))
        }) {
            return true;
        }
    }
    false
}

/// Invariant: every file we plan to write under `project_root` must be
/// attributable to some package via the per-package ownership view. Catches
/// emission sites that update `plan.files` without also calling one of the
/// `track_owned_*` helpers — without coverage, Nodus would leak ownership of
/// the file in the lockfile and sync's collision/cleanup logic would misbehave.
#[cfg(debug_assertions)]
fn debug_assert_owned_paths_cover_planned_files(
    planned_paths: &[PathBuf],
    project_root: &Path,
    managed_files_by_package: &[PackageOwnedPaths],
) {
    for path in planned_paths {
        // Files that live outside `project_root` aren't part of any package's
        // runtime-rooted ownership view.
        let Some(relative) = strip_path_prefix(path, project_root) else {
            continue;
        };

        let covered_by_subtree = managed_files_by_package.iter().any(|owned| {
            owned.subtrees.iter().any(|subtree| {
                relative == Path::new(subtree) || relative.starts_with(Path::new(subtree))
            })
        });
        if covered_by_subtree {
            continue;
        }
        let covered_by_file = managed_files_by_package.iter().any(|owned| {
            owned.files.iter().any(|file| {
                let owned = Path::new(file);
                relative == owned || relative.starts_with(owned)
            })
        });
        if covered_by_file {
            continue;
        }
        let covered_by_prefix = managed_files_by_package.iter().any(|owned| {
            owned.prefixes.iter().any(|rule| {
                relative.parent() == Some(Path::new(&rule.dir))
                    && relative
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(&rule.prefix))
            })
        });
        debug_assert!(
            covered_by_prefix,
            "planned file `{}` has no per-package ownership coverage in v10 emission",
            display_path(relative)
        );
    }
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
