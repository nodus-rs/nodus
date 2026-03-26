use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::lockfile::{Lockfile, managed_mcp_server_name};
use crate::manifest::DependencyComponent;
use crate::manifest::McpServerConfig;
use crate::paths::display_path;
use crate::resolver::{PackageSource, ResolvedPackage};

pub mod agents;
pub mod claude;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod opencode;

#[derive(Debug, Clone)]
pub struct ManagedFile {
    pub path: PathBuf,
    pub contents: Vec<u8>,
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

impl ArtifactKind {
    pub const fn supported_adapters(self) -> Adapters {
        match self {
            Self::Skill => Adapters::AGENTS
                .union(Adapters::CLAUDE)
                .union(Adapters::CODEX)
                .union(Adapters::COPILOT)
                .union(Adapters::CURSOR)
                .union(Adapters::OPENCODE),
            Self::Agent => Adapters::CLAUDE
                .union(Adapters::COPILOT)
                .union(Adapters::OPENCODE),
            Self::Rule => Adapters::CLAUDE
                .union(Adapters::CURSOR)
                .union(Adapters::OPENCODE),
            Self::Command => Adapters::AGENTS
                .union(Adapters::CLAUDE)
                .union(Adapters::CURSOR)
                .union(Adapters::OPENCODE),
        }
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

#[derive(Debug, Default)]
pub struct OutputPlan {
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
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EmittedMcpServerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
}

pub fn namespaced_skill_id(package: &ResolvedPackage, skill_id: &str) -> String {
    namespaced_artifact_id(package, skill_id)
}

pub fn runtime_root(project_root: &Path, adapter: Adapter) -> PathBuf {
    project_root.join(match adapter {
        Adapter::Agents => ".agents",
        Adapter::Claude => ".claude",
        Adapter::Codex => ".codex",
        Adapter::Copilot => ".github",
        Adapter::Cursor => ".cursor",
        Adapter::OpenCode => ".opencode",
    })
}

pub fn managed_skill_root(
    project_root: &Path,
    adapter: Adapter,
    package: &ResolvedPackage,
    skill_id: &str,
) -> PathBuf {
    runtime_root(project_root, adapter)
        .join("skills")
        .join(namespaced_skill_id(package, skill_id))
}

pub fn managed_artifact_path(
    project_root: &Path,
    adapter: Adapter,
    kind: ArtifactKind,
    package: &ResolvedPackage,
    artifact_id: &str,
) -> Option<PathBuf> {
    let runtime_root = runtime_root(project_root, adapter);
    match (adapter, kind) {
        (Adapter::Agents, ArtifactKind::Command) => Some(
            runtime_root
                .join("commands")
                .join(namespaced_file_name(package, artifact_id, "md")),
        ),
        (Adapter::Claude, ArtifactKind::Agent) => Some(
            runtime_root
                .join("agents")
                .join(namespaced_file_name(package, artifact_id, "md")),
        ),
        (Adapter::Claude, ArtifactKind::Command) => Some(
            runtime_root
                .join("commands")
                .join(namespaced_file_name(package, artifact_id, "md")),
        ),
        (Adapter::Copilot, ArtifactKind::Agent) => {
            Some(runtime_root.join("agents").join(namespaced_file_name(
                package,
                artifact_id,
                "agent.md",
            )))
        }
        (Adapter::Claude, ArtifactKind::Rule) => Some(
            runtime_root
                .join("rules")
                .join(namespaced_file_name(package, artifact_id, "md")),
        ),
        (Adapter::Cursor, ArtifactKind::Command) => Some(
            runtime_root
                .join("commands")
                .join(namespaced_file_name(package, artifact_id, "md")),
        ),
        (Adapter::Cursor, ArtifactKind::Rule) => Some(
            runtime_root
                .join("rules")
                .join(namespaced_file_name(package, artifact_id, "mdc")),
        ),
        (Adapter::OpenCode, ArtifactKind::Agent) => Some(
            runtime_root
                .join("agents")
                .join(namespaced_file_name(package, artifact_id, "md")),
        ),
        (Adapter::OpenCode, ArtifactKind::Command) => Some(
            runtime_root
                .join("commands")
                .join(namespaced_file_name(package, artifact_id, "md")),
        ),
        (Adapter::OpenCode, ArtifactKind::Rule) => Some(
            runtime_root
                .join("rules")
                .join(namespaced_file_name(package, artifact_id, "md")),
        ),
        _ => None,
    }
}

pub fn namespaced_artifact_id(package: &ResolvedPackage, artifact_id: &str) -> String {
    format!("{artifact_id}_{}", package_short_id(package))
}

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
        PackageSource::Path { .. } | PackageSource::Root => short_source_id(
            package
                .digest
                .strip_prefix("sha256:")
                .unwrap_or(&package.digest),
        ),
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

pub fn build_output_plan(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    selected_adapters: Adapters,
    existing_lockfile: Option<&Lockfile>,
    merge_existing_mcp: bool,
) -> Result<OutputPlan> {
    let mut plan = OutputAccumulator::default();

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
                    agents::skill_files(project_root, package, snapshot_root, skill)?,
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
                    claude::skill_files(project_root, package, snapshot_root, skill)?,
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
                    codex::skill_files(project_root, package, snapshot_root, skill)?,
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
                    copilot::skill_files(project_root, package, snapshot_root, skill)?,
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
                    cursor::skill_files(project_root, package, snapshot_root, skill)?,
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
                    opencode::skill_files(project_root, package, snapshot_root, skill)?,
                )?;
                plan.managed_files
                    .insert(format!(".opencode/skills/{}", skill.id));
            }
        }

        for agent in &package.manifest.discovered.agents {
            if !package.selects_component(DependencyComponent::Agents) {
                continue;
            }

            if selected_adapters.contains(Adapter::Claude)
                && ArtifactKind::Agent
                    .supported_adapters()
                    .contains(Adapter::Claude)
            {
                merge_file(
                    &mut plan.files,
                    claude::agent_file(project_root, package, snapshot_root, agent)?,
                )?;
                plan.managed_files
                    .insert(format!(".claude/agents/{}.md", agent.id));
            }

            if selected_adapters.contains(Adapter::Copilot)
                && ArtifactKind::Agent
                    .supported_adapters()
                    .contains(Adapter::Copilot)
            {
                merge_file(
                    &mut plan.files,
                    copilot::agent_file(project_root, package, snapshot_root, agent)?,
                )?;
                plan.managed_files
                    .insert(format!(".github/agents/{}", agent.id));
            }

            if selected_adapters.contains(Adapter::OpenCode)
                && ArtifactKind::Agent
                    .supported_adapters()
                    .contains(Adapter::OpenCode)
            {
                merge_file(
                    &mut plan.files,
                    opencode::agent_file(project_root, package, snapshot_root, agent)?,
                )?;
                plan.managed_files
                    .insert(format!(".opencode/agents/{}.md", agent.id));
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
                    claude::rule_file(project_root, package, snapshot_root, rule)?,
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
                    opencode::rule_file(project_root, package, snapshot_root, rule)?,
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
                    cursor::rule_file(project_root, package, snapshot_root, rule)?,
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
                    agents::command_file(project_root, package, snapshot_root, command)?,
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
                    claude::command_file(project_root, package, snapshot_root, command)?,
                )?;
                plan.managed_files
                    .insert(format!(".claude/commands/{}.md", command.id));
            }

            if selected_adapters.contains(Adapter::Cursor)
                && ArtifactKind::Command
                    .supported_adapters()
                    .contains(Adapter::Cursor)
            {
                merge_file(
                    &mut plan.files,
                    cursor::command_file(project_root, package, snapshot_root, command)?,
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
                    opencode::command_file(project_root, package, snapshot_root, command)?,
                )?;
                plan.managed_files
                    .insert(format!(".opencode/commands/{}.md", command.id));
            }
        }

        merge_files(
            &mut plan.files,
            direct_managed_files(project_root, package, snapshot_root)?,
        )?;
        register_direct_managed_paths(project_root, &mut plan.managed_files, package)?;
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

    if packages
        .iter()
        .any(|(package, _)| matches!(package.source, PackageSource::Root))
        && packages.iter().any(|(package, _)| {
            matches!(package.source, PackageSource::Root)
                && package.manifest.manifest.sync_on_launch_enabled()
        })
    {
        for file in sync_on_startup_files(project_root, selected_adapters, &mut plan.warnings)? {
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

fn direct_managed_files(
    project_root: &Path,
    package: &ResolvedPackage,
    snapshot_root: &Path,
) -> Result<Vec<ManagedFile>> {
    let mut files = Vec::new();

    for mapping in package.direct_managed_paths() {
        for file in &mapping.files {
            let contents =
                fs::read(snapshot_root.join(&file.source_relative)).with_context(|| {
                    format!(
                        "failed to read direct-managed source {} for `{}`",
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

fn register_direct_managed_paths(
    project_root: &Path,
    managed_files: &mut BTreeSet<String>,
    package: &ResolvedPackage,
) -> Result<()> {
    for mapping in package.direct_managed_paths() {
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
    let previously_managed = existing_lockfile
        .map(Lockfile::managed_mcp_server_names)
        .unwrap_or_default();
    let mut desired_servers = BTreeMap::new();
    for (package, _) in packages {
        if matches!(package.source, PackageSource::Root) && !package.manifest.manifest.publish_root
        {
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

fn emitted_mcp_server(server: &McpServerConfig) -> EmittedMcpServerConfig {
    EmittedMcpServerConfig {
        command: server.command.clone(),
        url: server.url.clone(),
        args: server.args.clone(),
        env: server.env.clone(),
        cwd: server.cwd.as_ref().map(|cwd| display_path(cwd)),
    }
}

fn gitignore_entry(project_root: &Path, path: &Path) -> Result<Option<(PathBuf, String)>> {
    let relative = path
        .strip_prefix(project_root)
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
    let relative = path
        .strip_prefix(project_root)
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

fn sync_on_startup_files(
    project_root: &Path,
    selected_adapters: Adapters,
    warnings: &mut Vec<String>,
) -> Result<Vec<ManagedFile>> {
    let mut files = Vec::new();

    if selected_adapters.contains(Adapter::Claude) {
        files.extend(claude::sync_on_startup_files(project_root));
    }
    if selected_adapters.contains(Adapter::OpenCode) {
        files.extend(opencode::sync_on_startup_files(project_root));
    }
    if selected_adapters.contains(Adapter::Agents) {
        warnings.push(
            "launch sync is not emitted for `agents`; no documented project startup hook surface is available".into(),
        );
    }
    if selected_adapters.contains(Adapter::Codex) {
        warnings.push(
            "launch sync is not emitted for `codex`; project config is supported, but no documented startup hook is available".into(),
        );
    }
    if selected_adapters.contains(Adapter::Copilot) {
        warnings.push(
            "launch sync is not emitted for `copilot`; repo-scoped assets are supported, but no documented startup hook is available".into(),
        );
    }
    if selected_adapters.contains(Adapter::Cursor) {
        warnings.push(
            "launch sync is not emitted for `cursor`; project hooks exist, but no documented auto-start hook is available for repo-local config".into(),
        );
    }

    Ok(files)
}

fn display_relative(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
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
        assert!(!agent.contains(Adapter::Codex));
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
}
