use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::ValueEnum;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adapters::Adapter;

pub const MANIFEST_FILE: &str = "nodus.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<Version>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_roots: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub publish_root: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub managed_exports: Vec<ManagedExportSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<Capability>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapters: Option<AdapterConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_hooks: Option<LaunchHookConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<HookSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claude_plugin_hooks: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, DependencySpec>,
    #[serde(
        default,
        rename = "dev-dependencies",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub dev_dependencies: BTreeMap<String, DependencySpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterConfig {
    pub enabled: Vec<Adapter>,
}

impl AdapterConfig {
    pub fn normalized(adapters: &[Adapter]) -> Self {
        let mut enabled = adapters.to_vec();
        enabled.sort();
        enabled.dedup();
        Self { enabled }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchHookConfig {
    pub sync_on_startup: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookSpec {
    pub id: String,
    pub event: HookEvent,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adapters: Vec<Adapter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<HookMatcher>,
    pub handler: HookHandler,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub blocking: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    Stop,
    SessionEnd,
}

impl HookEvent {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::PreToolUse => "pre_tool_use",
            Self::PermissionRequest => "permission_request",
            Self::PostToolUse => "post_tool_use",
            Self::Stop => "stop",
            Self::SessionEnd => "session_end",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HookMatcher {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<HookSessionSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_names: Vec<HookTool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookSessionSource {
    Startup,
    Resume,
    Clear,
    Compact,
}

impl HookSessionSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Resume => "resume",
            Self::Clear => "clear",
            Self::Compact => "compact",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookTool {
    Bash,
}

impl HookTool {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bash => "bash",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookHandler {
    #[serde(rename = "type")]
    pub handler_type: HookHandlerType,
    pub command: String,
    #[serde(
        default = "default_hook_working_directory",
        skip_serializing_if = "is_default_hook_working_directory"
    )]
    pub cwd: HookWorkingDirectory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookHandlerType {
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookWorkingDirectory {
    GitRoot,
    Session,
}

fn default_hook_working_directory() -> HookWorkingDirectory {
    HookWorkingDirectory::GitRoot
}

fn is_default_hook_working_directory(value: &HookWorkingDirectory) -> bool {
    *value == HookWorkingDirectory::GitRoot
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub package: BTreeMap<String, WorkspaceMemberSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMemberSpec {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<WorkspaceMemberCodexSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMemberCodexSpec {
    pub category: String,
    pub installation: String,
    pub authentication: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    pub id: String,
    pub sensitivity: String,
    #[serde(default)]
    pub justification: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub transport_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencySpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, alias = "rev", skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<VersionReq>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub components: Option<Vec<DependencyComponent>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<Vec<ManagedPathSpec>>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedPathSpec {
    pub source: PathBuf,
    pub target: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedExportSpec {
    pub source: PathBuf,
    pub target: PathBuf,
    #[serde(default, skip_serializing_if = "ManagedPlacement::is_package")]
    pub placement: ManagedPlacement,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedPlacement {
    #[default]
    Package,
    Project,
}

impl ManagedPlacement {
    pub const fn is_package(value: &Self) -> bool {
        matches!(value, Self::Package)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum DependencyComponent {
    #[value(name = "skills")]
    Skills,
    #[value(name = "agents")]
    Agents,
    #[value(name = "rules")]
    Rules,
    #[value(name = "commands")]
    Commands,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    Dependency,
    DevDependency,
}

impl DependencyKind {
    pub const fn manifest_section(self) -> &'static str {
        match self {
            Self::Dependency => "dependencies",
            Self::DevDependency => "dev-dependencies",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Dependency => "dependency",
            Self::DevDependency => "dev-dependency",
        }
    }

    pub const fn is_dev(self) -> bool {
        matches!(self, Self::DevDependency)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DependencyEntry<'a> {
    pub alias: &'a str,
    pub spec: &'a DependencySpec,
    pub kind: DependencyKind,
}

impl DependencyComponent {
    pub const ALL: [Self; 4] = [Self::Skills, Self::Agents, Self::Rules, Self::Commands];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Skills => "skills",
            Self::Agents => "agents",
            Self::Rules => "rules",
            Self::Commands => "commands",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencySourceKind {
    Git,
    Path,
}

#[derive(Debug, Clone)]
pub struct LoadedManifest {
    pub root: PathBuf,
    pub manifest_path: Option<PathBuf>,
    pub manifest: Manifest,
    pub discovered: PackageContents,
    pub warnings: Vec<String>,
    pub(super) claude_plugin: Option<ClaudePluginExtras>,
    pub(super) extra_package_files: Vec<PathBuf>,
    pub(super) allows_empty_dependency_wrapper: bool,
    pub(super) allows_unpinned_git_dependencies: bool,
    pub(super) manifest_contents_override: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWorkspaceMember {
    pub id: String,
    pub path: PathBuf,
    pub name: Option<String>,
    pub codex: Option<WorkspaceMemberCodexSpec>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ClaudePluginExtras {
    pub(super) skills: Vec<PathBuf>,
    pub(super) agents: Vec<PathBuf>,
    pub(super) commands: Vec<ClaudePluginCommandSpec>,
    pub(super) hook_compat_sources: Vec<ClaudePluginHookCompatSource>,
    pub(super) mcp_servers: Vec<ClaudePluginMcpSource>,
}

impl ClaudePluginExtras {
    pub(super) fn is_empty(&self) -> bool {
        self.skills.is_empty()
            && self.agents.is_empty()
            && self.commands.is_empty()
            && self.hook_compat_sources.is_empty()
            && self.mcp_servers.is_empty()
    }

    pub(super) fn has_nodus_manageable_content(&self) -> bool {
        !self.skills.is_empty()
            || !self.agents.is_empty()
            || !self.commands.is_empty()
            || !self.hook_compat_sources.is_empty()
            || !self.mcp_servers.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClaudePluginCommandSpec {
    pub(super) id: Option<String>,
    pub(super) path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ClaudePluginMcpSource {
    Inline(BTreeMap<String, McpServerConfig>),
    Path(PathBuf),
}

// Claude plugin hook configs are tracked separately from Manifest::hooks because
// they rely on Claude-specific plugin-root semantics like `CLAUDE_PLUGIN_ROOT`.
// Nodus imports them as an adapter-specific compatibility surface rather than
// treating them as portable hook intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaudePluginHookCompatSource {
    Inline(Value),
    Path(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMemberStatus {
    pub id: String,
    pub path: PathBuf,
    pub name: Option<String>,
    pub codex: Option<WorkspaceMemberCodexSpec>,
    pub enabled: bool,
    pub warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InitSummary {
    pub created_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageContents {
    pub skills: Vec<SkillEntry>,
    pub agents: Vec<AgentEntry>,
    pub rules: Vec<FileEntry>,
    pub commands: Vec<FileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    pub id: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEntry {
    pub id: String,
    pub path: PathBuf,
    pub qualifiers: Vec<String>,
    pub format: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub id: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageRole {
    Root,
    Dependency,
}

#[derive(Debug, Deserialize)]
pub(super) struct SkillFrontmatter {
    #[serde(default)]
    pub(super) name: Option<String>,
    pub(super) description: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct ClaudeMarketplace {
    pub(super) plugins: Vec<ClaudeMarketplacePlugin>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ClaudeMarketplacePlugin {
    pub(super) name: String,
    pub(super) source: ClaudeMarketplaceSource,
    #[serde(default)]
    pub(super) version: Option<String>,
    #[serde(default, rename = "mcpServers")]
    pub(super) mcp_servers: Option<ClaudeMarketplaceMcpServers>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum ClaudeMarketplaceSource {
    LocalPath(String),
    Remote(ClaudeMarketplaceRemoteSource),
}

#[derive(Debug, Deserialize)]
pub(super) struct ClaudeMarketplaceRemoteSource {
    pub(super) source: String,
    #[serde(default)]
    pub(super) url: Option<String>,
    #[serde(default)]
    pub(super) repo: Option<String>,
    #[serde(default)]
    pub(super) path: Option<PathBuf>,
    #[serde(default)]
    pub(super) sha: Option<String>,
    #[serde(default, rename = "ref")]
    pub(super) git_ref: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum ClaudeMarketplaceMcpServers {
    Inline(BTreeMap<String, McpServerConfig>),
    Path(String),
}

#[derive(Debug, Deserialize)]
pub(super) struct ClaudePluginMetadata {
    #[serde(default)]
    pub(super) version: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum ClaudePluginMcpConfig {
    Wrapped {
        #[serde(rename = "mcpServers")]
        mcp_servers: BTreeMap<String, McpServerConfig>,
    },
    Flat(BTreeMap<String, McpServerConfig>),
}

#[derive(Debug, Deserialize)]
pub(super) struct CodexMarketplace {
    pub(super) plugins: Vec<CodexMarketplacePlugin>,
}

#[derive(Debug, Deserialize)]
pub(super) struct CodexMarketplacePlugin {
    pub(super) name: String,
    pub(super) source: CodexMarketplaceSource,
}

#[derive(Debug, Deserialize)]
pub(super) struct CodexMarketplaceSource {
    pub(super) source: String,
    pub(super) path: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct CodexPluginMetadata {
    #[serde(default)]
    pub(super) version: Option<String>,
    #[serde(default, rename = "mcpServers")]
    pub(super) mcp_servers: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct CodexPluginMcpConfig {
    #[serde(default, rename = "mcpServers")]
    pub(super) mcp_servers: BTreeMap<String, McpServerConfig>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_true(value: &bool) -> bool {
    *value
}

fn default_true() -> bool {
    true
}
