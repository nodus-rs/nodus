use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::lockfile::LockedPackage;
use crate::manifest::{DependencyComponent, HookEvent, HookSessionSource, HookSpec, HookTool};
use crate::paths::{display_path, strip_path_prefix};
use crate::resolver::{PackageSource, ResolvedPackage};

mod output;
mod profile;
mod virtual_plugin;

pub mod agents;
pub mod claude;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod opencode;

#[cfg(test)]
pub(crate) use output::build_output_plan;
pub(crate) use output::{
    OutputPlan, OutputPlanOptions, PackageOwnedPaths, build_output_plan_with_options,
    codex_user_plugin_config_file,
};
pub(crate) use profile::{
    PreferredSurface, VirtualPluginSurface, artifact_supported, preferred_surface,
    virtual_plugin_surface,
};
pub(crate) use virtual_plugin::{
    VirtualPluginBackend, VirtualPluginEntry, emit_virtual_plugin_files,
    virtual_plugin_entries_for_package, virtual_plugin_install_root_relative,
};

#[derive(Debug, Clone)]
pub struct ManagedFile {
    pub path: PathBuf,
    pub contents: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedHookSpec {
    pub package_alias: String,
    pub emitted_from_root: bool,
    pub hook: HookSpec,
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedActivationHook {
    pub package_alias: String,
    pub context: String,
}

pub(crate) fn hook_event_supported_by_adapter(adapter: Adapter, event: HookEvent) -> bool {
    profile::hook_event_supported(adapter, event)
}

pub(crate) fn session_start_source_supported_by_adapter(
    adapter: Adapter,
    source: HookSessionSource,
) -> bool {
    profile::session_start_source_supported(adapter, source)
}

pub(crate) fn hook_supported_by_adapter(hook: &HookSpec, adapter: Adapter) -> bool {
    hook_event_supported_by_adapter(adapter, hook.event)
        && (!matches!(hook.event, HookEvent::SessionStart)
            || hook
                .matcher
                .as_ref()
                .map(|matcher| matcher.sources.is_empty())
                .unwrap_or(true)
            || hook.matcher.as_ref().is_some_and(|matcher| {
                matcher
                    .sources
                    .iter()
                    .any(|source| session_start_source_supported_by_adapter(adapter, *source))
            }))
        && tool_matchers_supported_by_adapter(hook, adapter)
}

pub(crate) fn effective_session_start_sources(
    hook: &HookSpec,
    adapter: Adapter,
) -> Vec<HookSessionSource> {
    if !matches!(hook.event, HookEvent::SessionStart) {
        return Vec::new();
    }

    let configured = hook
        .matcher
        .as_ref()
        .map(|matcher| matcher.sources.as_slice())
        .unwrap_or_default();
    let mut sources = if configured.is_empty() {
        vec![HookSessionSource::Startup, HookSessionSource::Resume]
    } else {
        configured
            .iter()
            .copied()
            .filter(|source| session_start_source_supported_by_adapter(adapter, *source))
            .collect::<Vec<_>>()
    };
    sources.sort_by_key(|source| match source {
        HookSessionSource::Startup => 0,
        HookSessionSource::Resume => 1,
        HookSessionSource::Clear => 2,
        HookSessionSource::Compact => 3,
    });
    sources.dedup_by_key(|source| source.as_str());
    sources
}

fn tool_matchers_supported_by_adapter(hook: &HookSpec, adapter: Adapter) -> bool {
    if !matches!(
        hook.event,
        HookEvent::PreToolUse | HookEvent::PermissionRequest | HookEvent::PostToolUse
    ) {
        return true;
    }

    let configured = hook
        .matcher
        .as_ref()
        .map(|matcher| matcher.tool_names.as_slice())
        .unwrap_or_default();
    configured.is_empty()
        || configured
            .iter()
            .any(|tool| hook_tool_matcher_for_adapter(adapter, *tool).is_some())
}

pub(crate) fn hook_tool_matchers_for_adapter(
    hook: &HookSpec,
    adapter: Adapter,
) -> Vec<&'static str> {
    hook.matcher
        .as_ref()
        .map(|matcher| matcher.tool_names.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|tool| hook_tool_matcher_for_adapter(adapter, *tool))
        .collect()
}

pub(crate) fn hook_tool_matcher_for_adapter(
    adapter: Adapter,
    tool: HookTool,
) -> Option<&'static str> {
    profile::hook_tool_matcher(adapter, tool)
}

pub(crate) fn virtual_plugin_backend(
    adapter: Adapter,
) -> Option<&'static dyn VirtualPluginBackend> {
    match adapter {
        Adapter::OpenCode => Some(&opencode::VIRTUAL_PLUGIN_BACKEND),
        Adapter::Agents | Adapter::Claude | Adapter::Codex | Adapter::Copilot | Adapter::Cursor => {
            None
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Adapter {
    #[value(name = "agents")]
    Agents,
    #[value(name = "claude")]
    Claude,
    #[value(name = "codex")]
    Codex,
    #[value(name = "copilot")]
    Copilot,
    #[value(name = "cursor")]
    Cursor,
    #[value(name = "opencode", alias = "open-code")]
    OpenCode,
}

impl Adapter {
    pub const ALL: [Self; 6] = [
        Self::Agents,
        Self::Claude,
        Self::Codex,
        Self::Copilot,
        Self::Cursor,
        Self::OpenCode,
    ];

    const fn bit(self) -> u8 {
        match self {
            Self::Agents => 1 << 0,
            Self::Claude => 1 << 1,
            Self::Codex => 1 << 2,
            Self::Copilot => 1 << 3,
            Self::Cursor => 1 << 4,
            Self::OpenCode => 1 << 5,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Copilot => "copilot",
            Self::Cursor => "cursor",
            Self::OpenCode => "opencode",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Adapters(u8);

impl Adapters {
    pub const NONE: Self = Self(0);
    pub const AGENTS: Self = Self(Adapter::Agents.bit());
    pub const CLAUDE: Self = Self(Adapter::Claude.bit());
    pub const CODEX: Self = Self(Adapter::Codex.bit());
    pub const COPILOT: Self = Self(Adapter::Copilot.bit());
    pub const CURSOR: Self = Self(Adapter::Cursor.bit());
    pub const OPENCODE: Self = Self(Adapter::OpenCode.bit());

    pub const fn contains(self, adapter: Adapter) -> bool {
        self.0 & adapter.bit() != 0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[cfg(test)]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn from_slice(adapters: &[Adapter]) -> Self {
        adapters
            .iter()
            .copied()
            .fold(Self::NONE, |selected, adapter| {
                selected.union(adapter.into())
            })
    }

    pub fn to_vec(self) -> Vec<Adapter> {
        self.iter().collect()
    }

    pub fn iter(self) -> impl Iterator<Item = Adapter> {
        Adapter::ALL
            .into_iter()
            .filter(move |adapter| self.contains(*adapter))
    }
}

impl From<Adapter> for Adapters {
    fn from(value: Adapter) -> Self {
        Self(value.bit())
    }
}

impl std::fmt::Display for Adapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    Skill,
    Agent,
    Rule,
    Command,
}

#[derive(Debug, Clone, Default)]
pub struct ManagedArtifactNames {
    duplicate_skills: HashSet<String>,
    duplicate_agents: HashSet<String>,
    duplicate_rules: HashSet<String>,
    duplicate_commands: HashSet<String>,
}

impl ManagedArtifactNames {
    pub fn from_resolved_packages<'a>(
        packages: impl IntoIterator<Item = &'a ResolvedPackage>,
    ) -> Self {
        let mut duplicate_skills = HashMap::new();
        let mut duplicate_agents = HashMap::new();
        let mut duplicate_rules = HashMap::new();
        let mut duplicate_commands = HashMap::new();

        for package in packages {
            if !package.emits_runtime_outputs() {
                continue;
            }

            if package.selects_component(DependencyComponent::Skills) {
                track_duplicates(
                    &mut duplicate_skills,
                    package
                        .manifest
                        .discovered
                        .skills
                        .iter()
                        .map(|skill| &skill.id),
                );
            }
            if package.selects_component(DependencyComponent::Agents) {
                track_duplicates(
                    &mut duplicate_agents,
                    package.manifest.discovered.unique_agent_ids(),
                );
            }
            if package.selects_component(DependencyComponent::Rules) {
                track_duplicates(
                    &mut duplicate_rules,
                    package
                        .manifest
                        .discovered
                        .rules
                        .iter()
                        .map(|rule| &rule.id),
                );
            }
            if package.selects_component(DependencyComponent::Commands) {
                track_duplicates(
                    &mut duplicate_commands,
                    package
                        .manifest
                        .discovered
                        .commands
                        .iter()
                        .map(|command| &command.id),
                );
            }
        }

        Self {
            duplicate_skills: collect_duplicates(duplicate_skills),
            duplicate_agents: collect_duplicates(duplicate_agents),
            duplicate_rules: collect_duplicates(duplicate_rules),
            duplicate_commands: collect_duplicates(duplicate_commands),
        }
    }

    pub fn from_locked_packages<'a>(packages: impl IntoIterator<Item = &'a LockedPackage>) -> Self {
        let mut duplicate_skills = HashMap::new();
        let mut duplicate_agents = HashMap::new();
        let mut duplicate_rules = HashMap::new();
        let mut duplicate_commands = HashMap::new();

        for package in packages {
            track_duplicates(&mut duplicate_skills, package.skills.iter());
            track_duplicates(&mut duplicate_agents, package.agents.iter());
            track_duplicates(&mut duplicate_rules, package.rules.iter());
            track_duplicates(&mut duplicate_commands, package.commands.iter());
        }

        Self {
            duplicate_skills: collect_duplicates(duplicate_skills),
            duplicate_agents: collect_duplicates(duplicate_agents),
            duplicate_rules: collect_duplicates(duplicate_rules),
            duplicate_commands: collect_duplicates(duplicate_commands),
        }
    }

    pub fn managed_skill_id(&self, package: &ResolvedPackage, skill_id: &str) -> String {
        self.artifact_id(ArtifactKind::Skill, skill_id, package_short_id(package))
    }

    pub fn managed_artifact_id(
        &self,
        package: &ResolvedPackage,
        kind: ArtifactKind,
        artifact_id: &str,
    ) -> String {
        self.artifact_id(kind, artifact_id, package_short_id(package))
    }

    pub fn managed_file_name(
        &self,
        package: &ResolvedPackage,
        kind: ArtifactKind,
        artifact_id: &str,
        extension: &str,
    ) -> String {
        format!(
            "{}.{}",
            self.artifact_id(kind, artifact_id, package_short_id(package)),
            extension.trim_start_matches('.')
        )
    }

    pub fn locked_managed_skill_id(&self, package: &LockedPackage, skill_id: &str) -> String {
        self.artifact_id(
            ArtifactKind::Skill,
            skill_id,
            locked_package_short_id(package),
        )
    }

    pub fn locked_managed_artifact_id(
        &self,
        package: &LockedPackage,
        kind: ArtifactKind,
        artifact_id: &str,
    ) -> String {
        self.artifact_id(kind, artifact_id, locked_package_short_id(package))
    }

    pub fn locked_managed_file_name(
        &self,
        package: &LockedPackage,
        kind: ArtifactKind,
        artifact_id: &str,
        extension: &str,
    ) -> String {
        format!(
            "{}.{}",
            self.artifact_id(kind, artifact_id, locked_package_short_id(package)),
            extension.trim_start_matches('.')
        )
    }

    fn artifact_id(
        &self,
        kind: ArtifactKind,
        artifact_id: &str,
        package_short_id: String,
    ) -> String {
        if self.requires_suffix(kind, artifact_id) {
            format!("{artifact_id}_{package_short_id}")
        } else {
            artifact_id.to_string()
        }
    }

    fn requires_suffix(&self, kind: ArtifactKind, artifact_id: &str) -> bool {
        match kind {
            ArtifactKind::Skill => self.duplicate_skills.contains(artifact_id),
            ArtifactKind::Agent => self.duplicate_agents.contains(artifact_id),
            ArtifactKind::Rule => self.duplicate_rules.contains(artifact_id),
            ArtifactKind::Command => self.duplicate_commands.contains(artifact_id),
        }
    }
}

impl ArtifactKind {
    pub fn supported_adapters(self) -> Adapters {
        profile::supported_adapters(self)
    }

    pub const fn plural_name(self) -> &'static str {
        match self {
            Self::Skill => "skills",
            Self::Agent => "agents",
            Self::Rule => "rules",
            Self::Command => "commands",
        }
    }
}

fn track_duplicates<'a>(
    counts: &mut HashMap<String, usize>,
    ids: impl IntoIterator<Item = &'a String>,
) {
    for id in ids {
        *counts.entry(id.clone()).or_default() += 1;
    }
}

fn collect_duplicates(counts: HashMap<String, usize>) -> HashSet<String> {
    counts
        .into_iter()
        .filter_map(|(id, count)| (count > 1).then_some(id))
        .collect()
}

pub fn managed_skill_id(
    names: &ManagedArtifactNames,
    package: &ResolvedPackage,
    skill_id: &str,
) -> String {
    names.managed_skill_id(package, skill_id)
}

pub(crate) fn managed_runtime_skill_id(
    names: &ManagedArtifactNames,
    adapter: Adapter,
    package: &ResolvedPackage,
    skill_id: &str,
) -> String {
    let local_names;
    let names = if preferred_surface(adapter) == PreferredSurface::PackagePluginWorkspaceMarketplace
    {
        local_names = ManagedArtifactNames::from_resolved_packages([package]);
        &local_names
    } else {
        names
    };
    managed_skill_id(names, package, skill_id)
}

pub fn managed_artifact_id(
    names: &ManagedArtifactNames,
    package: &ResolvedPackage,
    kind: ArtifactKind,
    artifact_id: &str,
) -> String {
    names.managed_artifact_id(package, kind, artifact_id)
}

pub fn locked_managed_artifact_id(
    names: &ManagedArtifactNames,
    package: &LockedPackage,
    kind: ArtifactKind,
    artifact_id: &str,
) -> String {
    names.locked_managed_artifact_id(package, kind, artifact_id)
}

pub fn runtime_root(project_root: &Path, adapter: Adapter) -> PathBuf {
    project_root.join(profile::runtime_root_name(adapter))
}

const NATIVE_MARKETPLACE_ROOT: &str = ".nodus";

pub(crate) fn native_marketplace_root(project_root: &Path) -> PathBuf {
    project_root.join(NATIVE_MARKETPLACE_ROOT)
}

pub(crate) fn native_marketplace_source_path() -> &'static str {
    "./.nodus"
}

pub(crate) fn native_marketplace_plugin_source_path(
    project_root: &Path,
    plugin_root: &Path,
) -> String {
    let marketplace_root = native_marketplace_root(project_root);
    if let Some(relative) = strip_path_prefix(plugin_root, &marketplace_root) {
        return local_marketplace_path(relative);
    }
    if let Some(relative) = strip_path_prefix(plugin_root, project_root) {
        return format!("../{}", display_path(relative));
    }
    display_path(plugin_root)
}

fn local_marketplace_path(relative: &Path) -> String {
    let path = display_path(relative);
    if path.starts_with("./") || path.starts_with("../") {
        path
    } else {
        format!("./{path}")
    }
}

pub(crate) fn native_marketplace_path(project_root: &Path, adapter: Adapter) -> Option<PathBuf> {
    let root = native_marketplace_root(project_root);
    match adapter {
        Adapter::Claude => Some(root.join(".claude-plugin").join("marketplace.json")),
        Adapter::Codex => Some(
            root.join(".agents")
                .join("plugins")
                .join("marketplace.json"),
        ),
        Adapter::Agents | Adapter::Copilot | Adapter::Cursor | Adapter::OpenCode => None,
    }
}

pub(crate) fn native_package_plugin_root(
    project_root: &Path,
    adapter: Adapter,
    package: &ResolvedPackage,
) -> PathBuf {
    if matches!(package.source, PackageSource::Root)
        || project_root_is_native_package_plugin_root(project_root, adapter, package)
    {
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

fn project_root_is_native_package_plugin_root(
    project_root: &Path,
    adapter: Adapter,
    package: &ResolvedPackage,
) -> bool {
    let Some(plugin_dir) = project_root.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    let expected_plugin_dir = match adapter {
        Adapter::Claude => "claude-plugin",
        Adapter::Codex => "codex-plugin",
        Adapter::Agents | Adapter::Copilot | Adapter::Cursor | Adapter::OpenCode => return false,
    };
    if plugin_dir != expected_plugin_dir {
        return false;
    }

    project_root
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        == Some(package.alias.as_str())
}

pub(crate) fn managed_runtime_root(
    project_root: &Path,
    adapter: Adapter,
    package: &ResolvedPackage,
) -> PathBuf {
    if preferred_surface(adapter) == PreferredSurface::PackagePluginWorkspaceMarketplace {
        let plugin_root = native_package_plugin_root(project_root, adapter, package);
        if !matches!(package.source, PackageSource::Root) {
            return plugin_root;
        }
        return runtime_root(&plugin_root, adapter);
    }

    runtime_root(project_root, adapter)
}

pub fn managed_skill_root(
    names: &ManagedArtifactNames,
    project_root: &Path,
    adapter: Adapter,
    package: &ResolvedPackage,
    skill_id: &str,
) -> PathBuf {
    let local_names;
    let names = if preferred_surface(adapter) == PreferredSurface::PackagePluginWorkspaceMarketplace
    {
        local_names = ManagedArtifactNames::from_resolved_packages([package]);
        &local_names
    } else {
        names
    };
    managed_runtime_root(project_root, adapter, package)
        .join("skills")
        .join(managed_skill_id(names, package, skill_id))
}

pub fn managed_artifact_path(
    names: &ManagedArtifactNames,
    project_root: &Path,
    adapter: Adapter,
    kind: ArtifactKind,
    package: &ResolvedPackage,
    artifact_id: &str,
) -> Option<PathBuf> {
    // Codex's native plugin format does not declare agents in plugin.json, so
    // agents must be emitted under the project's `.codex/agents/` runtime root
    // (and tracked with globally-unique names) regardless of Codex's preferred
    // marketplace surface.
    let codex_agent_override = matches!((adapter, kind), (Adapter::Codex, ArtifactKind::Agent));
    let local_names;
    let names = if !codex_agent_override
        && preferred_surface(adapter) == PreferredSurface::PackagePluginWorkspaceMarketplace
    {
        local_names = ManagedArtifactNames::from_resolved_packages([package]);
        &local_names
    } else {
        names
    };
    let runtime_root = if codex_agent_override {
        runtime_root(project_root, adapter)
    } else {
        managed_runtime_root(project_root, adapter, package)
    };
    match (adapter, kind) {
        (Adapter::Agents, ArtifactKind::Command) => {
            Some(runtime_root.join("commands").join(managed_file_name(
                names,
                package,
                kind,
                artifact_id,
                "md",
            )))
        }
        (Adapter::Claude, ArtifactKind::Agent) => {
            Some(runtime_root.join("agents").join(managed_file_name(
                names,
                package,
                kind,
                artifact_id,
                "md",
            )))
        }
        (Adapter::Codex, ArtifactKind::Agent) => {
            Some(runtime_root.join("agents").join(managed_file_name(
                names,
                package,
                kind,
                artifact_id,
                "toml",
            )))
        }
        (Adapter::Claude, ArtifactKind::Command) => {
            Some(runtime_root.join("commands").join(managed_file_name(
                names,
                package,
                kind,
                artifact_id,
                "md",
            )))
        }
        (Adapter::Copilot, ArtifactKind::Agent) => Some(runtime_root.join("agents").join(
            managed_file_name(names, package, kind, artifact_id, "agent.md"),
        )),
        (Adapter::Claude, ArtifactKind::Rule) => {
            Some(runtime_root.join("rules").join(managed_file_name(
                names,
                package,
                kind,
                artifact_id,
                "md",
            )))
        }
        (Adapter::Cursor, ArtifactKind::Command) => {
            Some(runtime_root.join("commands").join(managed_file_name(
                names,
                package,
                kind,
                artifact_id,
                "md",
            )))
        }
        (Adapter::Cursor, ArtifactKind::Rule) => {
            Some(runtime_root.join("rules").join(managed_file_name(
                names,
                package,
                kind,
                artifact_id,
                "mdc",
            )))
        }
        (Adapter::OpenCode, ArtifactKind::Agent) => {
            Some(runtime_root.join("agents").join(managed_file_name(
                names,
                package,
                kind,
                artifact_id,
                "md",
            )))
        }
        (Adapter::OpenCode, ArtifactKind::Command) => {
            Some(runtime_root.join("commands").join(managed_file_name(
                names,
                package,
                kind,
                artifact_id,
                "md",
            )))
        }
        (Adapter::OpenCode, ArtifactKind::Rule) => {
            Some(runtime_root.join("rules").join(managed_file_name(
                names,
                package,
                kind,
                artifact_id,
                "md",
            )))
        }
        _ => None,
    }
}

#[cfg(test)]
pub fn namespaced_artifact_id(package: &ResolvedPackage, artifact_id: &str) -> String {
    format!("{artifact_id}_{}", package_short_id(package))
}

pub fn managed_file_name(
    names: &ManagedArtifactNames,
    package: &ResolvedPackage,
    kind: ArtifactKind,
    artifact_id: &str,
    extension: &str,
) -> String {
    names.managed_file_name(package, kind, artifact_id, extension)
}

#[cfg(test)]
pub fn namespaced_skill_id(package: &ResolvedPackage, skill_id: &str) -> String {
    namespaced_artifact_id(package, skill_id)
}

#[cfg(test)]
pub fn namespaced_file_name(
    package: &ResolvedPackage,
    artifact_id: &str,
    extension: &str,
) -> String {
    format!(
        "{}.{}",
        namespaced_artifact_id(package, artifact_id),
        extension.trim_start_matches('.')
    )
}

pub fn package_short_id(package: &ResolvedPackage) -> String {
    match &package.source {
        PackageSource::Git { rev, .. } => short_source_id(rev),
        PackageSource::Path { .. } | PackageSource::Root => {
            short_source_id(strip_digest_prefix(&package.digest))
        }
    }
}

pub fn short_source_id(value: &str) -> String {
    let short = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(6)
        .collect::<String>()
        .to_ascii_lowercase();

    if short.is_empty() {
        "local0".into()
    } else {
        short
    }
}

fn locked_package_short_id(package: &LockedPackage) -> String {
    match package.source.kind.as_str() {
        "git" => short_source_id(
            package
                .source
                .rev
                .as_deref()
                .unwrap_or(package.digest.as_str()),
        ),
        _ => short_source_id(strip_digest_prefix(&package.digest)),
    }
}

fn strip_digest_prefix(digest: &str) -> &str {
    digest
        .strip_prefix("sha256:")
        .or_else(|| digest.strip_prefix("blake3:"))
        .unwrap_or(digest)
}
