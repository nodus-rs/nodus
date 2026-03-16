use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use semver::Version;
use serde::{Deserialize, Serialize};
use toml::Table;

use crate::adapters::Adapter;
use crate::report::Reporter;

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
    pub capabilities: Vec<Capability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapters: Option<AdapterConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, DependencySpec>,
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
pub struct Capability {
    pub id: String,
    pub sensitivity: String,
    #[serde(default)]
    pub justification: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub components: Option<Vec<DependencyComponent>>,
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
    extra_package_files: Vec<PathBuf>,
    allows_empty_dependency_wrapper: bool,
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
struct SkillFrontmatter {
    name: String,
    description: String,
}

#[derive(Debug, Deserialize)]
struct ClaudeMarketplace {
    plugins: Vec<ClaudeMarketplacePlugin>,
}

#[derive(Debug, Deserialize)]
struct ClaudeMarketplacePlugin {
    name: String,
    source: String,
    #[serde(default)]
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaudePluginMetadata {
    #[serde(default)]
    version: Option<String>,
}

#[allow(dead_code)]
pub fn scaffold_init(reporter: &Reporter) -> Result<InitSummary> {
    let cwd = env::current_dir().context("failed to determine the current directory")?;
    scaffold_init_in_dir(&cwd, reporter)
}

pub fn scaffold_init_in_dir(root: &Path, reporter: &Reporter) -> Result<InitSummary> {
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to access {}", root.display()))?;
    let manifest_path = root.join(MANIFEST_FILE);
    if manifest_path.exists() {
        bail!("{} already exists", manifest_path.display());
    }

    let skill_dir = root.join("skills").join("example");
    let skill_file = skill_dir.join("SKILL.md");
    if skill_file.exists() {
        bail!("{} already exists", skill_file.display());
    }

    fs::create_dir_all(&skill_dir)
        .with_context(|| format!("failed to create {}", skill_dir.display()))?;
    reporter.status("Creating", manifest_path.display())?;
    crate::store::write_atomic(&manifest_path, default_manifest_contents().as_bytes())?;
    reporter.status("Creating", skill_file.display())?;
    crate::store::write_atomic(&skill_file, default_skill_contents().as_bytes())?;

    Ok(InitSummary {
        created_paths: vec![manifest_path, skill_file],
    })
}

pub fn load_root_from_dir(root: &Path) -> Result<LoadedManifest> {
    load_from_dir(root, PackageRole::Root)
}

pub fn load_dependency_from_dir(root: &Path) -> Result<LoadedManifest> {
    load_from_dir(root, PackageRole::Dependency)
}

pub fn load_from_dir(root: &Path, role: PackageRole) -> Result<LoadedManifest> {
    let root = canonicalize_existing_path(root)
        .with_context(|| format!("failed to access project root {}", root.display()))?;
    let manifest_path = root.join(MANIFEST_FILE);
    let (manifest, warnings, manifest_path) = if manifest_path.exists() {
        let contents = fs::read_to_string(&manifest_path)
            .with_context(|| format!("failed to read manifest {}", manifest_path.display()))?;
        let (manifest, warnings) = load_manifest_str(&manifest_path, &contents)?;
        (manifest, warnings, Some(manifest_path))
    } else {
        (Manifest::default(), Vec::new(), None)
    };

    let mut loaded = LoadedManifest {
        root: root.clone(),
        manifest_path,
        manifest,
        discovered: discover_package_contents(&root)?,
        warnings,
        extra_package_files: Vec::new(),
        allows_empty_dependency_wrapper: false,
    };

    if should_try_claude_marketplace_fallback(&loaded) {
        if let Some(marketplace_loaded) = load_claude_marketplace_wrapper(&loaded)? {
            loaded = marketplace_loaded;
        }
    }

    if loaded.manifest.version.is_none() {
        loaded.manifest.version = load_claude_plugin_version(&loaded.root)?;
    }

    loaded.validate(role)?;
    Ok(loaded)
}

pub fn write_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    let contents = serialize_manifest(manifest)?;
    crate::store::write_atomic(path, contents.as_bytes())
        .with_context(|| format!("failed to write manifest {}", path.display()))
}

pub fn serialize_manifest(manifest: &Manifest) -> Result<String> {
    let mut output = String::new();

    if let Some(api_version) = &manifest.api_version {
        output.push_str(&format!("api_version = {}\n", quote(api_version)));
    }
    if let Some(name) = &manifest.name {
        output.push_str(&format!("name = {}\n", quote(name)));
    }
    if let Some(version) = &manifest.version {
        output.push_str(&format!("version = {}\n", quote(&version.to_string())));
    }

    if !manifest.capabilities.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        for capability in &manifest.capabilities {
            output.push_str("[[capabilities]]\n");
            output.push_str(&format!("id = {}\n", quote(&capability.id)));
            output.push_str(&format!(
                "sensitivity = {}\n",
                quote(&capability.sensitivity)
            ));
            if let Some(justification) = &capability.justification {
                output.push_str(&format!("justification = {}\n", quote(justification)));
            }
            output.push('\n');
        }
    }

    if let Some(adapters) = &manifest.adapters {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str("[adapters]\n");
        let mut enabled = adapters.enabled.clone();
        enabled.sort();
        let encoded = enabled
            .into_iter()
            .map(|adapter| quote(adapter.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!("enabled = [{encoded}]\n"));
    }

    if !manifest.dependencies.is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str("[dependencies]\n");
        for (alias, dependency) in &manifest.dependencies {
            let mut fields = Vec::new();
            if let Some(github) = &dependency.github {
                fields.push(format!("github = {}", quote(github)));
            }
            if let Some(url) = &dependency.url {
                fields.push(format!("url = {}", quote(url)));
            }
            if let Some(path) = &dependency.path {
                fields.push(format!(
                    "path = {}",
                    quote(&path.to_string_lossy().replace('\\', "/"))
                ));
            }
            if let Some(tag) = &dependency.tag {
                fields.push(format!("tag = {}", quote(tag)));
            }
            if let Some(branch) = &dependency.branch {
                fields.push(format!("branch = {}", quote(branch)));
            }
            if let Some(components) = dependency.explicit_components_sorted() {
                let encoded = components
                    .into_iter()
                    .map(|component| quote(component.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ");
                fields.push(format!("components = [{encoded}]"));
            }
            output.push_str(&format!("{alias} = {{ {} }}\n", fields.join(", ")));
        }
    }

    Ok(output)
}

impl LoadedManifest {
    pub fn validate(&self, role: PackageRole) -> Result<()> {
        if let Some(api_version) = &self.manifest.api_version
            && api_version.trim().is_empty()
        {
            bail!("manifest field `api_version` must not be empty");
        }
        if let Some(name) = &self.manifest.name
            && name.trim().is_empty()
        {
            bail!("manifest field `name` must not be empty");
        }
        if let Some(adapters) = &self.manifest.adapters {
            if adapters.enabled.is_empty() {
                bail!("manifest field `adapters.enabled` must not be empty");
            }

            let mut sorted = adapters.enabled.clone();
            sorted.sort();
            sorted.dedup();
            if sorted.len() != adapters.enabled.len() {
                bail!("manifest field `adapters.enabled` must not contain duplicates");
            }
        }

        let allow_empty_package = match role {
            PackageRole::Root => true,
            PackageRole::Dependency => {
                (self.manifest_path.is_some() || self.allows_empty_dependency_wrapper)
                    && !self.manifest.dependencies.is_empty()
            }
        };
        if self.discovered.is_empty() && !allow_empty_package {
            bail!(
                "package at {} must contain at least one of `agents/`, `commands/`, `rules/`, or `skills/`, or declare dependencies in nodus.toml",
                self.root.display()
            );
        }

        for (alias, dependency) in &self.manifest.dependencies {
            if alias.trim().is_empty() {
                bail!("dependency names must not be empty");
            }
            match dependency.source_kind()? {
                DependencySourceKind::Git => {
                    let url = dependency.resolved_git_url()?;
                    if url.trim().is_empty() {
                        bail!("dependency `{alias}` has an empty git source");
                    }
                    let tag = dependency.tag.as_deref().map(str::trim).unwrap_or_default();
                    let branch = dependency
                        .branch
                        .as_deref()
                        .map(str::trim)
                        .unwrap_or_default();
                    match (tag.is_empty(), branch.is_empty()) {
                        (false, false) => {
                            bail!("dependency `{alias}` must not declare both `tag` and `branch`")
                        }
                        (true, true) => {
                            bail!(
                                "dependency `{alias}` must declare `tag` or `branch` for git sources"
                            )
                        }
                        _ => {}
                    }
                }
                DependencySourceKind::Path => {
                    let Some(path) = &dependency.path else {
                        bail!("dependency `{alias}` must declare `path`");
                    };
                    let dependency_root = self.resolve_existing_path(path)?;
                    if !dependency_root.is_dir() {
                        bail!(
                            "dependency `{alias}` path must point to a directory, found {}",
                            dependency_root.display()
                        );
                    }
                }
            }

            let Some(components) = &dependency.components else {
                continue;
            };
            if components.is_empty() {
                bail!("dependency `{alias}` field `components` must not be empty");
            }

            let mut sorted = components.clone();
            sorted.sort();
            sorted.dedup();
            if sorted.len() != components.len() {
                bail!("dependency `{alias}` field `components` must not contain duplicates");
            }
        }

        Ok(())
    }

    pub fn package_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = self.discovered.files(self)?;
        if let Some(manifest_path) = &self.manifest_path {
            files.push(manifest_path.clone());
        }
        files.extend(self.extra_package_files.iter().cloned());
        files.sort();
        files.dedup();
        Ok(files)
    }

    pub fn resolve_path(&self, value: &Path) -> Result<PathBuf> {
        self.resolve_existing_path(value)
    }

    pub fn effective_name(&self) -> String {
        self.manifest
            .name
            .clone()
            .unwrap_or_else(|| default_package_name(&self.root))
    }

    pub fn effective_version(&self) -> Option<Version> {
        self.manifest.version.clone()
    }

    fn resolve_existing_path(&self, value: &Path) -> Result<PathBuf> {
        if value.is_absolute() {
            bail!(
                "manifest path `{}` must be relative to {}",
                value.display(),
                self.root.display()
            );
        }

        let joined = self.root.join(value);
        let canonical = canonicalize_existing_path(&joined)
            .with_context(|| format!("missing path `{}`", value.display()))?;
        if !canonical.starts_with(&self.root) {
            bail!(
                "path `{}` escapes the package root {}",
                value.display(),
                self.root.display()
            );
        }

        Ok(canonical)
    }
}

impl Manifest {
    pub fn enabled_adapters(&self) -> Option<&[Adapter]> {
        self.adapters
            .as_ref()
            .map(|config| config.enabled.as_slice())
    }

    pub fn set_enabled_adapters(&mut self, adapters: &[Adapter]) {
        self.adapters = Some(AdapterConfig::normalized(adapters));
    }
}

impl DependencySpec {
    pub fn explicit_components_sorted(&self) -> Option<Vec<DependencyComponent>> {
        let mut components = self.components.clone()?;
        components.sort();
        Some(components)
    }

    pub fn normalized_components(&self) -> Vec<DependencyComponent> {
        self.explicit_components_sorted()
            .unwrap_or_else(|| DependencyComponent::ALL.to_vec())
    }

    pub fn effective_selected_components(&self) -> Option<Vec<DependencyComponent>> {
        let components = self.normalized_components();
        (components.len() != DependencyComponent::ALL.len()).then_some(components)
    }

    pub fn source_kind(&self) -> Result<DependencySourceKind> {
        let git_sources = usize::from(self.github.is_some()) + usize::from(self.url.is_some());
        match (git_sources, self.path.is_some()) {
            (1, false) => Ok(DependencySourceKind::Git),
            (0, true) => Ok(DependencySourceKind::Path),
            (0, false) => {
                bail!("dependency source must declare either `github`, `url`, or `path`")
            }
            (_, true) => {
                bail!(
                    "dependency source must not declare both a git source (`github` or `url`) and `path`"
                )
            }
            _ => bail!("dependency source must not declare both `github` and `url`"),
        }
    }

    pub fn resolved_git_url(&self) -> Result<String> {
        if let Some(url) = &self.url {
            let trimmed = url.trim();
            if trimmed.is_empty() {
                bail!("git dependency `url` must not be empty");
            }
            return Ok(trimmed.to_string());
        }

        if let Some(github) = &self.github {
            let trimmed = github.trim().trim_matches('/');
            let Some((owner, repo)) = trimmed.split_once('/') else {
                bail!("git dependency `github` must use the format `owner/repo`");
            };
            if owner.is_empty() || repo.is_empty() || repo.contains('/') {
                bail!("git dependency `github` must use the format `owner/repo`");
            }
            return Ok(format!("https://github.com/{owner}/{repo}"));
        }

        bail!("dependency source must declare either `github` or `url` for git dependencies")
    }

    pub fn requested_git_ref(&self) -> Result<RequestedGitRef<'_>> {
        match (
            self.tag
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
            self.branch
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        ) {
            (Some(tag), None) => Ok(RequestedGitRef::Tag(tag)),
            (None, Some(branch)) => Ok(RequestedGitRef::Branch(branch)),
            (Some(_), Some(_)) => bail!("git dependency must not declare both `tag` and `branch`"),
            (None, None) => bail!("git dependency must declare `tag` or `branch`"),
        }
    }
}

pub enum RequestedGitRef<'a> {
    Tag(&'a str),
    Branch(&'a str),
}

impl PackageContents {
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
            && self.agents.is_empty()
            && self.rules.is_empty()
            && self.commands.is_empty()
    }

    pub fn files(&self, package: &LoadedManifest) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for skill in &self.skills {
            files.extend(collect_files(&package.resolve_existing_path(&skill.path)?)?);
        }
        for agent in &self.agents {
            files.push(package.resolve_existing_path(&agent.path)?);
        }
        for rule in &self.rules {
            files.push(package.resolve_existing_path(&rule.path)?);
        }
        for command in &self.commands {
            files.push(package.resolve_existing_path(&command.path)?);
        }
        files.sort();
        files.dedup();
        Ok(files)
    }
}

fn load_manifest_str(path: &Path, contents: &str) -> Result<(Manifest, Vec<String>)> {
    let raw_value: toml::Value = toml::from_str(contents)
        .with_context(|| format!("failed to parse TOML in {}", path.display()))?;
    let raw_table = raw_value
        .as_table()
        .cloned()
        .ok_or_else(|| anyhow!("manifest root must be a TOML table"))?;
    let manifest: Manifest = raw_value.try_into()?;
    Ok((manifest, collect_ignored_field_warnings(&raw_table)))
}

fn should_try_claude_marketplace_fallback(loaded: &LoadedManifest) -> bool {
    loaded.discovered.is_empty() && loaded.manifest.dependencies.is_empty()
}

fn load_claude_marketplace_wrapper(loaded: &LoadedManifest) -> Result<Option<LoadedManifest>> {
    let marketplace_path = loaded.root.join(".claude-plugin").join("marketplace.json");
    if !marketplace_path.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&marketplace_path)
        .with_context(|| format!("failed to read {}", marketplace_path.display()))?;
    let marketplace: ClaudeMarketplace = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse JSON in {}", marketplace_path.display()))?;
    if marketplace.plugins.is_empty() {
        bail!(
            "{} must declare at least one plugin",
            marketplace_path.display()
        );
    }

    let mut manifest = loaded.manifest.clone();
    let mut single_plugin_version = None;
    let mut aliases = HashSet::new();
    let plugin_count = marketplace.plugins.len();
    for plugin in marketplace.plugins {
        let name = plugin.name.trim();
        if name.is_empty() {
            bail!(
                "{} plugin names must not be empty",
                marketplace_path.display()
            );
        }

        let source = plugin.source.trim();
        if source.is_empty() {
            bail!(
                "{} plugin `{name}` must declare a non-empty `source`",
                marketplace_path.display()
            );
        }

        let declared_version = plugin
            .version
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| {
                Version::parse(value).with_context(|| {
                    format!(
                        "failed to parse plugin `{name}` version `{value}` in {}",
                        marketplace_path.display()
                    )
                })
            })
            .transpose()?;

        let alias = normalize_dependency_alias(name)?;
        if !aliases.insert(alias.clone()) {
            bail!(
                "{} contains duplicate plugin alias `{alias}` after normalization",
                marketplace_path.display()
            );
        }

        let source_path = PathBuf::from(source);
        let plugin_root = loaded
            .resolve_existing_path(&source_path)
            .with_context(|| format!("plugin `{name}` has invalid source `{source}`"))?;
        if !plugin_root.is_dir() {
            bail!(
                "plugin `{name}` source `{source}` must point to a directory, found {}",
                plugin_root.display()
            );
        }
        if plugin_root == loaded.root {
            bail!("plugin `{name}` source `{source}` must not point at the package root");
        }

        let plugin_manifest = load_dependency_from_dir(&plugin_root).with_context(|| {
            format!("plugin `{name}` source `{source}` does not match the Nodus package layout")
        })?;

        manifest.dependencies.insert(
            alias,
            DependencySpec {
                github: None,
                url: None,
                path: Some(source_path),
                tag: declared_version
                    .clone()
                    .or_else(|| plugin_manifest.effective_version())
                    .map(|version| version.to_string()),
                branch: None,
                components: None,
            },
        );

        if plugin_count == 1 {
            single_plugin_version =
                declared_version.or_else(|| plugin_manifest.effective_version());
        }
    }

    if manifest.version.is_none() {
        manifest.version = single_plugin_version;
    }

    Ok(Some(LoadedManifest {
        root: loaded.root.clone(),
        manifest_path: loaded.manifest_path.clone(),
        manifest,
        discovered: PackageContents::default(),
        warnings: loaded.warnings.clone(),
        extra_package_files: vec![marketplace_path],
        allows_empty_dependency_wrapper: true,
    }))
}

fn discover_package_contents(root: &Path) -> Result<PackageContents> {
    Ok(PackageContents {
        skills: discover_skills(root)?,
        agents: discover_files(root, "agents", true, false)?,
        rules: discover_files(root, "rules", false, true)?,
        commands: discover_files(root, "commands", false, true)?,
    })
}

fn discover_skills(root: &Path) -> Result<Vec<SkillEntry>> {
    let skills_root = root.join("skills");
    if !skills_root.exists() {
        return Ok(Vec::new());
    }
    if !skills_root.is_dir() {
        bail!("`skills/` must be a directory");
    }

    let mut skills = Vec::new();
    let mut ids = HashSet::new();
    for entry in fs::read_dir(&skills_root)
        .with_context(|| format!("failed to read {}", skills_root.display()))?
    {
        let entry = entry?;
        if should_ignore_discovery_entry(&entry.path()) {
            continue;
        }
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            bail!("`skills/` entries must be directories");
        }

        let id = entry.file_name().to_string_lossy().to_string();
        if !ids.insert(id.clone()) {
            bail!("duplicate skill id `{id}`");
        }
        let relative = PathBuf::from("skills").join(&id);
        let skill_dir = canonicalize_existing_path(&root.join(&relative))?;
        if !skill_dir.starts_with(root) {
            bail!("skill `{id}` escapes the package root");
        }
        validate_skill_directory(&skill_dir).with_context(|| format!("skill `{id}` is invalid"))?;
        skills.push(SkillEntry { id, path: relative });
    }

    skills.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(skills)
}

fn discover_files(
    root: &Path,
    directory: &str,
    markdown_only: bool,
    recursive: bool,
) -> Result<Vec<FileEntry>> {
    let dir_root = root.join(directory);
    if !dir_root.exists() {
        return Ok(Vec::new());
    }
    if !dir_root.is_dir() {
        bail!("`{directory}/` must be a directory");
    }

    let mut items = Vec::new();
    let mut ids = HashSet::new();
    let walker = if recursive {
        walkdir::WalkDir::new(&dir_root).min_depth(1)
    } else {
        walkdir::WalkDir::new(&dir_root).min_depth(1).max_depth(1)
    };

    for entry in walker {
        let entry = entry?;
        let path = entry.path();
        if should_ignore_discovery_entry(path) {
            if entry.file_type().is_dir() {
                continue;
            }
            continue;
        }
        if entry.file_type().is_dir() {
            continue;
        }
        if !entry.file_type().is_file() {
            bail!("`{directory}/` entries must be files");
        }

        if markdown_only && path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            bail!("`{directory}/` entries must use the `.md` extension");
        }

        let relative = path
            .strip_prefix(&dir_root)
            .with_context(|| format!("failed to make {} relative", path.display()))?;
        let id = derive_file_entry_id(relative)?;
        if !ids.insert(id.clone()) {
            bail!("duplicate {directory} id `{id}`");
        }

        let relative = PathBuf::from(directory).join(relative);
        let canonical = canonicalize_existing_path(&root.join(&relative))?;
        if !canonical.starts_with(root) {
            bail!("`{directory}` item `{id}` escapes the package root");
        }
        items.push(FileEntry { id, path: relative });
    }

    items.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(items)
}

fn should_ignore_discovery_entry(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    name.starts_with('.') || name.eq_ignore_ascii_case("README.md")
}

fn derive_file_entry_id(relative: &Path) -> Result<String> {
    let stemmed = relative.with_extension("");
    let parts = stemmed
        .iter()
        .map(|value| {
            value
                .to_str()
                .ok_or_else(|| anyhow!("failed to derive id from {}", relative.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    let id = parts.join("__");
    if id.is_empty() {
        bail!("failed to derive id from {}", relative.display());
    }
    Ok(id)
}

fn validate_skill_directory(skill_dir: &Path) -> Result<()> {
    let skill_file = skill_dir.join("SKILL.md");
    let contents = fs::read_to_string(&skill_file)
        .with_context(|| format!("failed to read {}", skill_file.display()))?;
    let frontmatter = parse_skill_frontmatter(&contents)?;
    if frontmatter.name.trim().is_empty() {
        bail!("`SKILL.md` frontmatter field `name` must not be empty");
    }
    if frontmatter.description.trim().is_empty() {
        bail!("`SKILL.md` frontmatter field `description` must not be empty");
    }
    Ok(())
}

fn parse_skill_frontmatter(contents: &str) -> Result<SkillFrontmatter> {
    let mut lines = contents.lines();
    if lines.next() != Some("---") {
        bail!("`SKILL.md` must begin with YAML frontmatter");
    }

    let mut yaml = Vec::new();
    for line in lines {
        if line == "---" {
            let yaml = yaml.join("\n");
            return match serde_yaml::from_str(&yaml) {
                Ok(frontmatter) => Ok(frontmatter),
                Err(error) => parse_simple_frontmatter(&yaml).with_context(|| {
                    format!("failed to parse SKILL.md frontmatter as YAML: {error}")
                }),
            };
        }
        yaml.push(line);
    }

    bail!("`SKILL.md` frontmatter is missing a closing `---`");
}

fn parse_simple_frontmatter(yaml: &str) -> Result<SkillFrontmatter> {
    let mut name = None;
    let mut description = None;

    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let Some((key, value)) = trimmed.split_once(':') else {
            bail!("frontmatter line `{trimmed}` is not a simple `key: value` entry");
        };
        let key = key.trim();
        let value = unquote_frontmatter_value(value.trim());

        match key {
            "name" => name = Some(value),
            "description" => description = Some(value),
            _ => {}
        }
    }

    Ok(SkillFrontmatter {
        name: name.ok_or_else(|| anyhow!("missing `name` in frontmatter"))?,
        description: description.ok_or_else(|| anyhow!("missing `description` in frontmatter"))?,
    })
}

fn unquote_frontmatter_value(value: &str) -> String {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let first = bytes[0] as char;
        let last = bytes[value.len() - 1] as char;
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

fn collect_ignored_field_warnings(table: &Table) -> Vec<String> {
    const SUPPORTED_ROOT_FIELDS: &[&str] = &[
        "api_version",
        "name",
        "version",
        "capabilities",
        "adapters",
        "dependencies",
    ];

    let mut warnings = Vec::new();
    for key in table.keys() {
        if !SUPPORTED_ROOT_FIELDS.contains(&key.as_str()) {
            warnings.push(format!("ignoring unsupported manifest field `{key}`"));
        }
    }
    warnings.sort();
    warnings
}

fn collect_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() {
            files.push(canonicalize_existing_path(entry.path())?);
        }
    }
    files.sort();
    Ok(files)
}

pub fn normalize_dependency_alias(value: &str) -> Result<String> {
    let mut alias = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            alias.push(character.to_ascii_lowercase());
        } else if !alias.ends_with('_') {
            alias.push('_');
        }
    }

    let alias = alias.trim_matches('_').to_string();
    if alias.is_empty() {
        bail!("failed to derive a valid dependency alias from `{value}`");
    }
    Ok(alias)
}

fn load_claude_plugin_version(root: &Path) -> Result<Option<Version>> {
    let metadata_path = root.join("claude-code.json");
    if !metadata_path.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&metadata_path)
        .with_context(|| format!("failed to read {}", metadata_path.display()))?;
    let metadata: ClaudePluginMetadata = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse JSON in {}", metadata_path.display()))?;
    let Some(version) = metadata.version else {
        return Ok(None);
    };

    let version = version.trim();
    if version.is_empty() {
        return Ok(None);
    }

    Ok(Some(Version::parse(version).with_context(|| {
        format!(
            "failed to parse Claude plugin version `{version}` in {}",
            metadata_path.display()
        )
    })?))
}

fn canonicalize_existing_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("failed to canonicalize {}", path.display()))
}

fn default_manifest_contents() -> &'static str {
    ""
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn default_skill_contents() -> &'static str {
    "---\nname: Example\ndescription: Describe what this skill helps with.\n---\n# Example\n"
}

fn default_package_name(root: &Path) -> String {
    let name = root
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("agentpack");
    let mut normalized = String::new();

    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            normalized.push(character.to_ascii_lowercase());
        } else if !normalized.ends_with('-') {
            normalized.push('-');
        }
    }

    normalized.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use tempfile::TempDir;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::File::create(path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }

    fn write_valid_skill(root: &Path) {
        write_file(
            &root.join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
        );
    }

    fn write_marketplace(root: &Path, contents: &str) {
        write_file(&root.join(".claude-plugin/marketplace.json"), contents);
    }

    fn write_claude_plugin_json(root: &Path, version: &str) {
        write_file(
            &root.join("claude-code.json"),
            &format!("{{\n  \"name\": \"plugin\",\n  \"version\": \"{version}\"\n}}\n"),
        );
    }

    #[test]
    fn loads_root_manifest_without_required_metadata() {
        let temp = TempDir::new().unwrap();
        write_valid_skill(temp.path());
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[dependencies]
playbook_ios = { url = "https://github.com/wenext-limited/playbook-ios", tag = "v0.1.0" }
"#,
        );

        let loaded = load_root_from_dir(temp.path()).unwrap();

        assert!(loaded.manifest.api_version.is_none());
        assert!(loaded.manifest.name.is_none());
        assert!(loaded.manifest.version.is_none());
        assert_eq!(loaded.discovered.skills[0].id, "review");
    }

    #[test]
    fn accepts_root_project_with_only_dependencies() {
        let temp = TempDir::new().unwrap();
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", tag = "v0.1.0" }
"#,
        );

        let loaded = load_root_from_dir(temp.path()).unwrap();
        assert!(loaded.discovered.is_empty());
        assert_eq!(loaded.manifest.dependencies.len(), 1);
        assert_eq!(
            loaded
                .manifest
                .dependencies
                .get("playbook_ios")
                .unwrap()
                .resolved_git_url()
                .unwrap(),
            "https://github.com/wenext-limited/playbook-ios"
        );
    }

    #[test]
    fn rejects_dependency_repo_without_supported_directories() {
        let temp = TempDir::new().unwrap();
        let error = load_dependency_from_dir(temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("must contain at least one of"));
    }

    #[test]
    fn accepts_dependency_repo_with_only_nested_dependencies() {
        let temp = TempDir::new().unwrap();
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", tag = "v0.1.0" }
"#,
        );

        let loaded = load_dependency_from_dir(temp.path()).unwrap();

        assert!(loaded.discovered.is_empty());
        assert_eq!(loaded.manifest.dependencies.len(), 1);
    }

    #[test]
    fn accepts_dependency_repo_with_claude_marketplace_wrapper() {
        let temp = TempDir::new().unwrap();
        write_marketplace(
            temp.path(),
            r#"{
  "plugins": [
    {
      "name": "Axiom",
      "version": "2.34.0",
      "source": "./.claude-plugin/plugins/axiom"
    }
  ]
}"#,
        );
        write_file(
            &temp
                .path()
                .join(".claude-plugin/plugins/axiom/agents/reviewer.md"),
            "# Reviewer\n",
        );
        write_file(
            &temp
                .path()
                .join(".claude-plugin/plugins/axiom/commands/build.md"),
            "# Build\n",
        );
        write_file(
            &temp
                .path()
                .join(".claude-plugin/plugins/axiom/skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
        );
        write_claude_plugin_json(&temp.path().join(".claude-plugin/plugins/axiom"), "2.34.0");

        let loaded = load_dependency_from_dir(temp.path()).unwrap();

        assert!(loaded.discovered.is_empty());
        let dependency = loaded.manifest.dependencies.get("axiom").unwrap();
        assert_eq!(
            dependency.path.as_deref(),
            Some(Path::new("./.claude-plugin/plugins/axiom"))
        );
        assert_eq!(dependency.tag.as_deref(), Some("2.34.0"));
        assert_eq!(
            loaded.manifest.version,
            Some(Version::parse("2.34.0").unwrap())
        );

        let package_files = loaded.package_files().unwrap();
        assert!(
            package_files.contains(
                &temp
                    .path()
                    .join(".claude-plugin/marketplace.json")
                    .canonicalize()
                    .unwrap()
            )
        );
    }

    #[test]
    fn imports_all_marketplace_plugins_in_sorted_alias_order() {
        let temp = TempDir::new().unwrap();
        write_marketplace(
            temp.path(),
            r#"{
  "plugins": [
    {
      "name": "Zeta Plugin",
      "source": "./plugins/zeta"
    },
    {
      "name": "Alpha Plugin",
      "source": "./plugins/alpha"
    }
  ]
}"#,
        );
        write_file(
            &temp.path().join("plugins/zeta/skills/zeta/SKILL.md"),
            "---\nname: Zeta\ndescription: Zeta skill.\n---\n# Zeta\n",
        );
        write_file(
            &temp.path().join("plugins/alpha/skills/alpha/SKILL.md"),
            "---\nname: Alpha\ndescription: Alpha skill.\n---\n# Alpha\n",
        );

        let loaded = load_dependency_from_dir(temp.path()).unwrap();

        assert_eq!(
            loaded
                .manifest
                .dependencies
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["alpha_plugin", "zeta_plugin"]
        );
    }

    #[test]
    fn marketplace_sources_are_resolved_from_repo_root() {
        let temp = TempDir::new().unwrap();
        write_marketplace(
            temp.path(),
            r#"{
  "plugins": [
    {
      "name": "Axiom",
      "source": "./plugins/axiom"
    }
  ]
}"#,
        );
        write_file(
            &temp.path().join("plugins/axiom/skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
        );

        let loaded = load_dependency_from_dir(temp.path()).unwrap();

        assert_eq!(
            loaded
                .manifest
                .dependencies
                .get("axiom")
                .and_then(|dependency| dependency.path.as_deref()),
            Some(Path::new("./plugins/axiom"))
        );
    }

    #[test]
    fn reads_claude_plugin_version_from_json() {
        let temp = TempDir::new().unwrap();
        write_valid_skill(temp.path());
        write_claude_plugin_json(temp.path(), "2.34.0");

        let loaded = load_dependency_from_dir(temp.path()).unwrap();

        assert_eq!(
            loaded.manifest.version,
            Some(Version::parse("2.34.0").unwrap())
        );
    }

    #[test]
    fn rejects_marketplace_with_invalid_json() {
        let temp = TempDir::new().unwrap();
        write_marketplace(temp.path(), "{");

        let error = load_dependency_from_dir(temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("failed to parse JSON"));
    }

    #[test]
    fn rejects_marketplace_without_plugins() {
        let temp = TempDir::new().unwrap();
        write_marketplace(temp.path(), r#"{ "plugins": [] }"#);

        let error = load_dependency_from_dir(temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("must declare at least one plugin"));
    }

    #[test]
    fn rejects_marketplace_with_duplicate_plugin_aliases() {
        let temp = TempDir::new().unwrap();
        write_marketplace(
            temp.path(),
            r#"{
  "plugins": [
    {
      "name": "Axiom",
      "source": "./plugins/one"
    },
    {
      "name": "axiom",
      "source": "./plugins/two"
    }
  ]
}"#,
        );
        write_file(
            &temp.path().join("plugins/one/skills/one/SKILL.md"),
            "---\nname: One\ndescription: One skill.\n---\n# One\n",
        );
        write_file(
            &temp.path().join("plugins/two/skills/two/SKILL.md"),
            "---\nname: Two\ndescription: Two skill.\n---\n# Two\n",
        );

        let error = load_dependency_from_dir(temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("duplicate plugin alias `axiom`"));
    }

    #[test]
    fn rejects_marketplace_with_escaping_source_path() {
        let temp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        write_file(
            &outside.path().join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
        );
        let escaping_source = format!(
            "../{}",
            outside.path().file_name().unwrap().to_string_lossy()
        );
        write_marketplace(
            temp.path(),
            &format!(
                r#"{{
  "plugins": [
    {{
      "name": "Axiom",
      "source": "{escaping_source}"
    }}
  ]
}}"#
            ),
        );

        let error = load_dependency_from_dir(temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("plugin `Axiom` has invalid source"));
    }

    #[test]
    fn rejects_marketplace_with_missing_source_directory() {
        let temp = TempDir::new().unwrap();
        write_marketplace(
            temp.path(),
            r#"{
  "plugins": [
    {
      "name": "Axiom",
      "source": "./plugins/missing"
    }
  ]
}"#,
        );

        let error = load_dependency_from_dir(temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("has invalid source `./plugins/missing`"));
    }

    #[test]
    fn rejects_marketplace_with_plugin_source_that_is_not_a_directory() {
        let temp = TempDir::new().unwrap();
        write_marketplace(
            temp.path(),
            r#"{
  "plugins": [
    {
      "name": "Axiom",
      "source": "./plugins/axiom"
    }
  ]
}"#,
        );
        write_file(&temp.path().join("plugins/axiom"), "not a directory\n");

        let error = load_dependency_from_dir(temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("must point to a directory"));
    }

    #[test]
    fn rejects_marketplace_with_plugin_source_that_is_not_a_nodus_package() {
        let temp = TempDir::new().unwrap();
        write_marketplace(
            temp.path(),
            r#"{
  "plugins": [
    {
      "name": "Axiom",
      "source": "./plugins/axiom"
    }
  ]
}"#,
        );
        write_file(
            &temp.path().join("plugins/axiom/README.md"),
            "# Not a package\n",
        );

        let error = load_dependency_from_dir(temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not match the Nodus package layout"));
    }

    #[test]
    fn prefers_standard_layout_over_marketplace_fallback() {
        let temp = TempDir::new().unwrap();
        write_valid_skill(temp.path());
        write_marketplace(
            temp.path(),
            r#"{
  "plugins": [
    {
      "name": "Axiom",
      "source": "./plugins/axiom"
    }
  ]
}"#,
        );
        write_file(
            &temp.path().join("plugins/axiom/skills/axiom/SKILL.md"),
            "---\nname: Axiom\ndescription: Axiom skill.\n---\n# Axiom\n",
        );

        let loaded = load_dependency_from_dir(temp.path()).unwrap();

        assert_eq!(
            loaded
                .discovered
                .skills
                .iter()
                .map(|skill| skill.id.as_str())
                .collect::<Vec<_>>(),
            vec!["review"]
        );
        assert!(loaded.manifest.dependencies.is_empty());
    }

    #[test]
    fn rejects_invalid_git_dependency_without_tag() {
        let temp = TempDir::new().unwrap();
        write_valid_skill(temp.path());
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[dependencies]
playbook_ios = { url = "https://github.com/wenext-limited/playbook-ios" }
"#,
        );

        let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
        assert!(error.contains("must declare `tag`"));
    }

    #[test]
    fn rejects_invalid_github_dependency_reference() {
        let temp = TempDir::new().unwrap();
        write_valid_skill(temp.path());
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[dependencies]
playbook_ios = { github = "wenext-limited", tag = "v0.1.0" }
"#,
        );

        let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
        assert!(error.contains("must use the format `owner/repo`"));
    }

    #[test]
    fn rejects_invalid_skill_frontmatter() {
        let temp = TempDir::new().unwrap();
        write_file(
            &temp.path().join("skills/review/SKILL.md"),
            "---\nname: Review\n---\n# Review\n",
        );

        let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
        assert!(error.contains("skill `review` is invalid"));
    }

    #[test]
    fn accepts_unquoted_description_with_colon() {
        let temp = TempDir::new().unwrap();
        write_file(
            &temp.path().join("skills/ios-websocket/SKILL.md"),
            "---\nname: ios-websocket\ndescription: Use when a task involves WebSocket push-notification subscriptions. Trigger this skill for any of: subscribing to a new server push URI.\n---\n# iOS WebSocket\n",
        );

        let loaded = load_root_from_dir(temp.path()).unwrap();

        assert_eq!(loaded.discovered.skills[0].id, "ios-websocket");
    }

    #[test]
    fn discovers_agents_rules_and_commands() {
        let temp = TempDir::new().unwrap();
        write_valid_skill(temp.path());
        write_file(&temp.path().join("agents/security.md"), "# Security\n");
        write_file(&temp.path().join("rules/default.rules"), "allow = []\n");
        write_file(&temp.path().join("commands/build.txt"), "cargo test\n");

        let loaded = load_root_from_dir(temp.path()).unwrap();

        assert_eq!(loaded.discovered.skills[0].id, "review");
        assert_eq!(loaded.discovered.agents[0].id, "security");
        assert_eq!(loaded.discovered.rules[0].id, "default");
        assert_eq!(loaded.discovered.commands[0].id, "build");
    }

    #[test]
    fn discovers_nested_rules_with_stable_ids() {
        let temp = TempDir::new().unwrap();
        write_valid_skill(temp.path());
        write_file(
            &temp.path().join("rules/common/coding-style.md"),
            "# Common\n",
        );
        write_file(&temp.path().join("rules/swift/patterns.md"), "# Swift\n");

        let loaded = load_root_from_dir(temp.path()).unwrap();

        let ids = loaded
            .discovered
            .rules
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["common__coding-style", "swift__patterns"]);
    }

    #[test]
    fn ignores_readme_and_dotfiles_in_discovery_directories() {
        let temp = TempDir::new().unwrap();
        write_valid_skill(temp.path());
        write_file(&temp.path().join("skills/README.md"), "# Skills\n");
        write_file(&temp.path().join("skills/.DS_Store"), "binary\n");
        write_file(&temp.path().join("agents/.DS_Store"), "binary\n");
        write_file(&temp.path().join("agents/README.md"), "# Agents\n");
        write_file(&temp.path().join("agents/security.md"), "# Security\n");

        let loaded = load_root_from_dir(temp.path()).unwrap();

        assert_eq!(loaded.discovered.skills.len(), 1);
        assert_eq!(loaded.discovered.skills[0].id, "review");
        assert_eq!(loaded.discovered.agents.len(), 1);
        assert_eq!(loaded.discovered.agents[0].id, "security");
    }

    #[test]
    fn init_scaffolds_a_minimal_manifest_and_example_skill() {
        let temp = TempDir::new().unwrap();
        let reporter = Reporter::silent();

        scaffold_init_in_dir(temp.path(), &reporter).unwrap();

        assert!(temp.path().join(MANIFEST_FILE).exists());
        assert!(temp.path().join("skills/example/SKILL.md").exists());
        let loaded = load_root_from_dir(temp.path()).unwrap();
        assert_eq!(loaded.discovered.skills[0].id, "example");
    }

    #[test]
    fn serializes_dependencies_as_inline_tables() {
        let mut manifest = Manifest::default();
        manifest.dependencies.insert(
            "playbook_ios".into(),
            DependencySpec {
                github: Some("wenext-limited/playbook-ios".into()),
                url: None,
                path: None,
                tag: Some("v0.1.0".into()),
                branch: None,
                components: Some(vec![
                    DependencyComponent::Rules,
                    DependencyComponent::Skills,
                ]),
            },
        );

        let encoded = serialize_manifest(&manifest).unwrap();

        assert!(encoded.contains("[dependencies]"));
        assert!(encoded.contains("playbook_ios = {"));
        assert!(encoded.contains("github = \"wenext-limited/playbook-ios\""));
        assert!(encoded.contains("components = [\"skills\", \"rules\"]"));
        assert!(!encoded.contains("url = "));
    }

    #[test]
    fn serializes_adapters_in_stable_sorted_order() {
        let manifest = Manifest {
            adapters: Some(AdapterConfig {
                enabled: vec![Adapter::OpenCode, Adapter::Claude, Adapter::Codex],
            }),
            ..Manifest::default()
        };

        let encoded = serialize_manifest(&manifest).unwrap();

        assert!(encoded.contains("[adapters]"));
        assert!(encoded.contains("enabled = [\"claude\", \"codex\", \"opencode\"]"));
    }

    #[test]
    fn rejects_empty_adapter_selection() {
        let temp = TempDir::new().unwrap();
        write_valid_skill(temp.path());
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[adapters]
enabled = []
"#,
        );

        let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
        assert!(error.contains("adapters.enabled"));
    }

    #[test]
    fn rejects_duplicate_adapter_selection() {
        let temp = TempDir::new().unwrap();
        write_valid_skill(temp.path());
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[adapters]
enabled = ["codex", "codex"]
"#,
        );

        let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
        assert!(error.contains("must not contain duplicates"));
    }

    #[test]
    fn rejects_unknown_adapter_selection() {
        let temp = TempDir::new().unwrap();
        write_valid_skill(temp.path());
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[adapters]
enabled = ["unknown"]
"#,
        );

        let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
        assert!(error.contains("unknown variant"));
    }

    #[test]
    fn rejects_dependencies_with_multiple_git_sources() {
        let dependency = DependencySpec {
            github: Some("wenext-limited/playbook-ios".into()),
            url: Some("https://github.com/wenext-limited/playbook-ios".into()),
            path: None,
            tag: Some("v0.1.0".into()),
            branch: None,
            components: None,
        };

        let error = dependency.source_kind().unwrap_err().to_string();
        assert!(error.contains("must not declare both `github` and `url`"));
    }

    #[test]
    fn parses_dependency_components() {
        let temp = TempDir::new().unwrap();
        write_valid_skill(temp.path());
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", tag = "v0.1.0", components = ["skills", "agents"] }
"#,
        );

        let loaded = load_root_from_dir(temp.path()).unwrap();
        let dependency = loaded.manifest.dependencies.get("playbook_ios").unwrap();
        assert_eq!(
            dependency.explicit_components_sorted().unwrap(),
            vec![DependencyComponent::Skills, DependencyComponent::Agents]
        );
    }

    #[test]
    fn rejects_empty_dependency_components() {
        let temp = TempDir::new().unwrap();
        write_valid_skill(temp.path());
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", tag = "v0.1.0", components = [] }
"#,
        );

        let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
        assert!(error.contains("field `components` must not be empty"));
    }

    #[test]
    fn rejects_duplicate_dependency_components() {
        let temp = TempDir::new().unwrap();
        write_valid_skill(temp.path());
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", tag = "v0.1.0", components = ["skills", "skills"] }
"#,
        );

        let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
        assert!(error.contains("must not contain duplicates"));
    }

    #[test]
    fn rejects_unknown_dependency_component() {
        let temp = TempDir::new().unwrap();
        write_valid_skill(temp.path());
        write_file(
            &temp.path().join(MANIFEST_FILE),
            r#"
[dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", tag = "v0.1.0", components = ["widgets"] }
"#,
        );

        let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
        assert!(error.contains("unknown variant"));
    }
}
