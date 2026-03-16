use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use semver::Version;
use serde::{Deserialize, Serialize};
use toml::Table;

pub const MANIFEST_FILE: &str = "agentpack.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub api_version: String,
    pub name: String,
    pub version: Version,
    pub exports: Exports,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub dependencies: Dependencies,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Exports {
    #[serde(default)]
    pub skills: Vec<SkillExport>,
    #[serde(default)]
    pub agents: Vec<AgentExport>,
    #[serde(default)]
    pub rules: Vec<RuleExport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillExport {
    pub id: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentExport {
    pub id: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleExport {
    pub id: String,
    #[serde(default)]
    pub sources: Vec<RuleSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleSource {
    #[serde(rename = "type")]
    pub kind: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub id: String,
    pub sensitivity: String,
    #[serde(default)]
    pub justification: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Dependencies {
    #[serde(default)]
    pub agentpacks: BTreeMap<String, DependencySpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencySpec {
    pub path: PathBuf,
    #[serde(default)]
    pub requirement: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LoadedManifest {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: Manifest,
    pub warnings: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
}

pub fn scaffold_init() -> Result<()> {
    bail!("init is not implemented yet")
}

pub fn load_from_dir(root: &Path) -> Result<LoadedManifest> {
    let root = canonicalize_existing_path(root)
        .with_context(|| format!("failed to access project root {}", root.display()))?;
    let manifest_path = root.join(MANIFEST_FILE);
    let contents = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read manifest {}", manifest_path.display()))?;
    load_from_str(&root, &manifest_path, &contents)
}

fn load_from_str(root: &Path, manifest_path: &Path, contents: &str) -> Result<LoadedManifest> {
    let raw_value: toml::Value = toml::from_str(contents)
        .with_context(|| format!("failed to parse TOML in {}", manifest_path.display()))?;
    let raw_table = raw_value
        .as_table()
        .cloned()
        .ok_or_else(|| anyhow!("manifest root must be a TOML table"))?;
    let manifest: Manifest = raw_value.try_into()?;
    let warnings = collect_ignored_field_warnings(&raw_table);

    let loaded = LoadedManifest {
        root: root.to_path_buf(),
        manifest_path: manifest_path.to_path_buf(),
        manifest,
        warnings,
    };
    loaded.validate()?;
    Ok(loaded)
}

impl LoadedManifest {
    pub fn validate(&self) -> Result<()> {
        if self.manifest.api_version.trim().is_empty() {
            bail!("manifest field `api_version` must not be empty");
        }
        if self.manifest.name.trim().is_empty() {
            bail!("manifest field `name` must not be empty");
        }

        self.validate_export_ids(
            "skills",
            self.manifest.exports.skills.iter().map(|item| &item.id),
        )?;
        self.validate_export_ids(
            "agents",
            self.manifest.exports.agents.iter().map(|item| &item.id),
        )?;
        self.validate_export_ids(
            "rules",
            self.manifest.exports.rules.iter().map(|item| &item.id),
        )?;

        for skill in &self.manifest.exports.skills {
            let skill_dir = self.resolve_existing_path(&skill.path)?;
            if !skill_dir.is_dir() {
                bail!(
                    "skill export `{}` must point to a directory, found {}",
                    skill.id,
                    skill_dir.display()
                );
            }
            validate_skill_directory(&skill_dir)
                .with_context(|| format!("skill export `{}` is invalid", skill.id))?;
        }

        for agent in &self.manifest.exports.agents {
            let agent_path = self.resolve_existing_path(&agent.path)?;
            if !agent_path.is_file() {
                bail!(
                    "agent export `{}` must point to a file, found {}",
                    agent.id,
                    agent_path.display()
                );
            }
        }

        for rule in &self.manifest.exports.rules {
            if rule.sources.is_empty() {
                bail!("rule export `{}` must declare at least one source", rule.id);
            }
            for source in &rule.sources {
                if source.kind.trim().is_empty() {
                    bail!("rule export `{}` has an empty source type", rule.id);
                }
                let source_path = self.resolve_existing_path(&source.path)?;
                if !source_path.is_file() {
                    bail!(
                        "rule export `{}` source `{}` must point to a file",
                        rule.id,
                        source.kind
                    );
                }
            }
        }

        for (name, dependency) in &self.manifest.dependencies.agentpacks {
            if name.trim().is_empty() {
                bail!("dependency names must not be empty");
            }
            let dependency_root = self.resolve_existing_path(&dependency.path)?;
            if !dependency_root.is_dir() {
                bail!(
                    "dependency `{}` path must point to a directory, found {}",
                    name,
                    dependency_root.display()
                );
            }
        }

        Ok(())
    }

    pub fn export_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();

        for skill in &self.manifest.exports.skills {
            files.extend(collect_files(&self.resolve_existing_path(&skill.path)?)?);
        }

        for agent in &self.manifest.exports.agents {
            files.push(self.resolve_existing_path(&agent.path)?);
        }

        for rule in &self.manifest.exports.rules {
            for source in &rule.sources {
                files.push(self.resolve_existing_path(&source.path)?);
            }
        }

        files.sort();
        files.dedup();
        Ok(files)
    }

    fn validate_export_ids<'a>(
        &self,
        kind: &str,
        ids: impl Iterator<Item = &'a String>,
    ) -> Result<()> {
        let mut seen = HashSet::new();
        for id in ids {
            if id.trim().is_empty() {
                bail!("{kind} export ids must not be empty");
            }
            if !seen.insert(id.clone()) {
                bail!("duplicate {kind} export id `{id}`");
            }
        }
        Ok(())
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
            return serde_yaml::from_str(&yaml)
                .context("failed to parse SKILL.md frontmatter as YAML");
        }
        yaml.push(line);
    }

    bail!("`SKILL.md` frontmatter is missing a closing `---`");
}

fn collect_ignored_field_warnings(table: &Table) -> Vec<String> {
    const SUPPORTED_ROOT_FIELDS: &[&str] = &[
        "api_version",
        "name",
        "version",
        "exports",
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
    for entry in walkdir::WalkDir::new(root) {
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

    fn sample_manifest() -> &'static str {
        r#"
api_version = "agentpack/v0"
name = "example"
version = "0.1.0"
compatibility = { ignored = true }

[[exports.skills]]
id = "review"
path = "skills/review"

[[exports.agents]]
id = "security-reviewer"
path = "agents/security-reviewer.md"

[[exports.rules]]
id = "safe-shell"

[[exports.rules.sources]]
type = "codex.ruleset"
path = "rules/default.rules"

[[capabilities]]
id = "shell.exec"
sensitivity = "high"

[dependencies.agentpacks.shared]
path = "vendor/shared"
requirement = "^1.0.0"
"#
    }

    fn write_valid_skill(root: &Path) {
        write_file(
            &root.join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
        );
    }

    fn write_supporting_files(root: &Path) {
        write_valid_skill(root);
        write_file(
            &root.join("agents/security-reviewer.md"),
            "# Security Reviewer\n",
        );
        write_file(&root.join("rules/default.rules"), "allow = []\n");
        fs::create_dir_all(root.join("vendor/shared")).unwrap();
    }

    #[test]
    fn loads_valid_manifest_and_surfaces_ignored_fields() {
        let temp = TempDir::new().unwrap();
        write_supporting_files(temp.path());

        let manifest_path = temp.path().join(MANIFEST_FILE);
        write_file(&manifest_path, sample_manifest());

        let loaded = load_from_dir(temp.path()).unwrap();

        assert_eq!(loaded.manifest.name, "example");
        assert_eq!(loaded.manifest.version, Version::new(0, 1, 0));
        assert_eq!(
            loaded.warnings,
            vec!["ignoring unsupported manifest field `compatibility`"]
        );
    }

    #[test]
    fn rejects_skill_without_frontmatter_description() {
        let temp = TempDir::new().unwrap();
        write_file(
            &temp.path().join("skills/review/SKILL.md"),
            "---\nname: Review\n---\n# Review\n",
        );
        write_file(
            &temp.path().join("agents/security-reviewer.md"),
            "# Security Reviewer\n",
        );
        write_file(&temp.path().join("rules/default.rules"), "allow = []\n");
        fs::create_dir_all(temp.path().join("vendor/shared")).unwrap();
        write_file(&temp.path().join(MANIFEST_FILE), sample_manifest());

        let error = load_from_dir(temp.path()).unwrap_err().to_string();

        assert!(error.contains("skill export `review` is invalid"));
    }

    #[test]
    fn rejects_paths_that_escape_the_root() {
        let temp = TempDir::new().unwrap();
        write_supporting_files(temp.path());

        let manifest = r#"
api_version = "agentpack/v0"
name = "example"
version = "0.1.0"

[[exports.skills]]
id = "review"
path = "../outside"
"#;
        write_file(&temp.path().join(MANIFEST_FILE), manifest);

        let error = load_from_dir(temp.path()).unwrap_err().to_string();
        assert!(error.contains("escapes the package root") || error.contains("missing path"));
    }
}
