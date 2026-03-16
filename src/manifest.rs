use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use semver::Version;
use serde::{Deserialize, Serialize};
use toml::Table;

pub const MANIFEST_FILE: &str = "agentpack.toml";

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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, DependencySpec>,
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
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
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

pub fn scaffold_init() -> Result<()> {
    let cwd = env::current_dir().context("failed to determine the current directory")?;
    scaffold_init_in_dir(&cwd)
}

pub fn scaffold_init_in_dir(root: &Path) -> Result<()> {
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
    crate::store::write_atomic(&manifest_path, default_manifest_contents().as_bytes())?;
    crate::store::write_atomic(&skill_file, default_skill_contents().as_bytes())?;

    Ok(())
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

    let loaded = LoadedManifest {
        root: root.clone(),
        manifest_path,
        manifest,
        discovered: discover_package_contents(&root)?,
        warnings,
    };
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

    if !manifest.dependencies.is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str("[dependencies]\n");
        for (alias, dependency) in &manifest.dependencies {
            let mut fields = Vec::new();
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

        let allow_empty_package = role == PackageRole::Root;
        if self.discovered.is_empty() && !allow_empty_package {
            bail!(
                "package at {} must contain at least one of `agents/`, `commands/`, `rules/`, or `skills/`",
                self.root.display()
            );
        }

        for (alias, dependency) in &self.manifest.dependencies {
            if alias.trim().is_empty() {
                bail!("dependency names must not be empty");
            }
            match dependency.source_kind()? {
                DependencySourceKind::Git => {
                    let url = dependency.url.as_deref().unwrap_or_default();
                    if url.trim().is_empty() {
                        bail!("dependency `{alias}` has an empty `url`");
                    }
                    let tag = dependency.tag.as_deref().unwrap_or_default();
                    if tag.trim().is_empty() {
                        bail!("dependency `{alias}` must declare `tag` for git sources");
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
        }

        Ok(())
    }

    pub fn package_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = self.discovered.files(self)?;
        if let Some(manifest_path) = &self.manifest_path {
            files.push(manifest_path.clone());
        }
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

impl DependencySpec {
    pub fn source_kind(&self) -> Result<DependencySourceKind> {
        match (self.url.is_some(), self.path.is_some()) {
            (true, false) => Ok(DependencySourceKind::Git),
            (false, true) => Ok(DependencySourceKind::Path),
            (true, true) => bail!("dependency source must not declare both `url` and `path`"),
            (false, false) => bail!("dependency source must declare either `url` or `path`"),
        }
    }
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

fn discover_package_contents(root: &Path) -> Result<PackageContents> {
    Ok(PackageContents {
        skills: discover_skills(root)?,
        agents: discover_files(root, "agents", true)?,
        rules: discover_files(root, "rules", false)?,
        commands: discover_files(root, "commands", false)?,
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

fn discover_files(root: &Path, directory: &str, markdown_only: bool) -> Result<Vec<FileEntry>> {
    let dir_root = root.join(directory);
    if !dir_root.exists() {
        return Ok(Vec::new());
    }
    if !dir_root.is_dir() {
        bail!("`{directory}/` must be a directory");
    }

    let mut items = Vec::new();
    let mut ids = HashSet::new();
    for entry in
        fs::read_dir(&dir_root).with_context(|| format!("failed to read {}", dir_root.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            bail!("`{directory}/` entries must be files");
        }

        let path = entry.path();
        if markdown_only && path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            bail!("`{directory}/` entries must use the `.md` extension");
        }

        let id = path
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow!("failed to derive id from {}", path.display()))?
            .to_string();
        if !ids.insert(id.clone()) {
            bail!("duplicate {directory} id `{id}`");
        }

        let relative = PathBuf::from(directory).join(
            path.file_name()
                .ok_or_else(|| anyhow!("missing file name for {}", path.display()))?,
        );
        let canonical = canonicalize_existing_path(&root.join(&relative))?;
        if !canonical.starts_with(root) {
            bail!("`{directory}` item `{id}` escapes the package root");
        }
        items.push(FileEntry { id, path: relative });
    }

    items.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(items)
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
playbook_ios = { url = "https://github.com/wenext-limited/playbook-ios", tag = "v0.1.0" }
"#,
        );

        let loaded = load_root_from_dir(temp.path()).unwrap();
        assert!(loaded.discovered.is_empty());
        assert_eq!(loaded.manifest.dependencies.len(), 1);
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
    fn init_scaffolds_a_minimal_manifest_and_example_skill() {
        let temp = TempDir::new().unwrap();

        scaffold_init_in_dir(temp.path()).unwrap();

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
                url: Some("https://github.com/wenext-limited/playbook-ios".into()),
                path: None,
                tag: Some("v0.1.0".into()),
            },
        );

        let encoded = serialize_manifest(&manifest).unwrap();

        assert!(encoded.contains("[dependencies]"));
        assert!(encoded.contains("playbook_ios = {"));
    }
}
