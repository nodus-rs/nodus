use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::ValueEnum;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

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
    pub capabilities: Vec<Capability>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapters: Option<AdapterConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_hooks: Option<LaunchHookConfig>,
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
pub struct Capability {
    pub id: String,
    pub sensitivity: String,
    #[serde(default)]
    pub justification: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
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
    pub managed: Option<Vec<ManagedPathSpec>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedPathSpec {
    pub source: PathBuf,
    pub target: PathBuf,
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
    pub(super) extra_package_files: Vec<PathBuf>,
    pub(super) allows_empty_dependency_wrapper: bool,
    pub(super) manifest_contents_override: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct InitSummary {
    pub created_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageContents {
    pub skills: Vec<SkillEntry>,
    pub agents: Vec<FileEntry>,
    pub rules: Vec<FileEntry>,
    pub commands: Vec<FileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    pub id: String,
    pub path: PathBuf,
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
    pub(super) name: String,
    pub(super) description: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct ClaudeMarketplace {
    pub(super) plugins: Vec<ClaudeMarketplacePlugin>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ClaudeMarketplacePlugin {
    pub(super) name: String,
    pub(super) source: String,
    #[serde(default)]
    pub(super) version: Option<String>,
    #[serde(default, rename = "mcpServers")]
    pub(super) mcp_servers: Option<ClaudeMarketplaceMcpServers>,
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
