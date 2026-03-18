use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use semver::Version;
use toml::Table;

use super::types::{ClaudeMarketplace, ClaudePluginMetadata, SkillFrontmatter};
use super::*;

pub(super) fn load_manifest_str(path: &Path, contents: &str) -> Result<(Manifest, Vec<String>)> {
    let raw_value: toml::Value = toml::from_str(contents)
        .with_context(|| format!("failed to parse TOML in {}", path.display()))?;
    let raw_table = raw_value
        .as_table()
        .cloned()
        .ok_or_else(|| anyhow!("manifest root must be a TOML table"))?;
    let manifest: Manifest = raw_value.try_into()?;
    Ok((manifest, collect_ignored_field_warnings(&raw_table)))
}

pub(super) fn should_try_claude_marketplace_fallback(loaded: &LoadedManifest) -> bool {
    loaded.discovered.is_empty() && loaded.manifest.dependencies.is_empty()
}

pub(super) fn load_claude_marketplace_wrapper(
    loaded: &LoadedManifest,
) -> Result<Option<LoadedManifest>> {
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
                tag: None,
                branch: None,
                revision: None,
                version: declared_version
                    .clone()
                    .or_else(|| plugin_manifest.effective_version()),
                components: None,
                managed: None,
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
        manifest_contents_override: None,
    }))
}

pub(super) fn discover_package_contents(root: &Path) -> Result<PackageContents> {
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

pub(super) fn collect_ignored_field_warnings(table: &Table) -> Vec<String> {
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

pub(super) fn validate_dependency_managed_specs(
    alias: &str,
    managed: Option<&[ManagedPathSpec]>,
) -> Result<()> {
    let Some(managed) = managed else {
        return Ok(());
    };
    if managed.is_empty() {
        bail!("dependency `{alias}` field `managed` must not be empty");
    }

    let mut seen = HashSet::new();
    for mapping in managed {
        let normalized_source = mapping
            .normalized_source()
            .with_context(|| format!("dependency `{alias}` field `managed.source` is invalid"))?;
        let normalized_target = mapping
            .normalized_target()
            .with_context(|| format!("dependency `{alias}` field `managed.target` is invalid"))?;
        if !seen.insert((normalized_source, normalized_target)) {
            bail!(
                "dependency `{alias}` field `managed` must not contain duplicate source/target pairs"
            );
        }
    }

    Ok(())
}

pub(super) fn normalize_manifest_relative_path(value: &Path, label: &str) -> Result<PathBuf> {
    if value.as_os_str().is_empty() {
        bail!("{label} must not be empty");
    }
    if value.is_absolute() {
        bail!("{label} must be relative");
    }

    let mut normalized = PathBuf::new();
    for component in value.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => bail!("{label} must not contain `..`"),
            Component::RootDir | Component::Prefix(_) => bail!("{label} must be relative"),
        }
    }

    if normalized.as_os_str().is_empty() {
        bail!("{label} must not be empty");
    }

    Ok(normalized)
}

pub(super) fn collect_files(root: &Path) -> Result<Vec<PathBuf>> {
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

pub(super) fn load_claude_plugin_version(root: &Path) -> Result<Option<Version>> {
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

pub(super) fn canonicalize_existing_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("failed to canonicalize {}", path.display()))
}

pub(super) fn default_manifest_contents() -> &'static str {
    ""
}

pub(super) fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub(super) fn default_skill_contents() -> &'static str {
    "---\nname: Example\ndescription: Describe what this skill helps with.\n---\n# Example\n"
}

pub(super) fn default_package_name(root: &Path) -> String {
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
