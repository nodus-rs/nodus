use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::manifest::Capability;
use crate::store::write_atomic;

pub const LOCKFILE_NAME: &str = "nodus.lock";
const LOCKFILE_VERSION: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    pub version: u32,
    pub packages: Vec<LockedPackage>,
    #[serde(default)]
    pub managed_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPackage {
    pub alias: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_tag: Option<String>,
    pub source: LockedSource,
    pub digest: String,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedSource {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
}

impl Lockfile {
    pub fn new(mut packages: Vec<LockedPackage>, mut managed_files: Vec<String>) -> Self {
        packages.sort_by(|left, right| {
            left.alias
                .cmp(&right.alias)
                .then(left.name.cmp(&right.name))
                .then(left.source.kind.cmp(&right.source.kind))
                .then(left.source.path.cmp(&right.source.path))
                .then(left.source.url.cmp(&right.source.url))
                .then(left.source.tag.cmp(&right.source.tag))
                .then(left.source.rev.cmp(&right.source.rev))
        });
        managed_files.sort();
        managed_files.dedup();
        Self {
            version: LOCKFILE_VERSION,
            packages,
            managed_files,
        }
    }

    pub fn read(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read lockfile {}", path.display()))?;
        toml::from_str(&contents)
            .with_context(|| format!("failed to parse lockfile {}", path.display()))
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let contents = toml::to_string_pretty(self).context("failed to serialize lockfile")?;
        write_atomic(path, contents.as_bytes())
            .with_context(|| format!("failed to write lockfile {}", path.display()))
    }

    pub fn managed_paths(&self, project_root: &Path) -> Result<HashSet<PathBuf>> {
        self.managed_files
            .iter()
            .map(|relative| {
                let relative_path = Path::new(relative);
                if relative_path.is_absolute()
                    || relative_path
                        .components()
                        .any(|component| matches!(component, std::path::Component::ParentDir))
                {
                    bail!(
                        "managed path {} escapes project root {}",
                        relative,
                        project_root.display()
                    );
                }
                let path = project_root.join(relative_path);
                Ok(path)
            })
            .collect()
    }

    pub fn normalize_relative(project_root: &Path, path: &Path) -> Result<String> {
        let relative = path.strip_prefix(project_root).with_context(|| {
            format!(
                "managed path {} is not inside project root {}",
                path.display(),
                project_root.display()
            )
        })?;
        Ok(relative.to_string_lossy().replace('\\', "/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_lockfile_as_toml() {
        let lockfile = Lockfile::new(
            vec![LockedPackage {
                alias: "playbook_ios".into(),
                name: "playbook-ios".into(),
                version_tag: Some("v0.1.0".into()),
                source: LockedSource {
                    kind: "git".into(),
                    path: None,
                    url: Some("https://github.com/wenext-limited/playbook-ios".into()),
                    tag: Some("v0.1.0".into()),
                    rev: Some("abc123".into()),
                },
                digest: "sha256:abc".into(),
                skills: vec!["review".into()],
                agents: vec!["security-reviewer".into()],
                rules: vec!["safe-shell".into()],
                commands: vec!["build".into()],
                dependencies: vec![],
                capabilities: vec![Capability {
                    id: "shell.exec".into(),
                    sensitivity: "high".into(),
                    justification: Some("Needed for tests".into()),
                }],
            }],
            vec![
                ".claude/skills/review_a1b2c3/SKILL.md".into(),
                ".codex/rules/safe-shell.rules".into(),
            ],
        );

        let encoded = toml::to_string_pretty(&lockfile).unwrap();
        let decoded: Lockfile = toml::from_str(&encoded).unwrap();

        assert_eq!(decoded, lockfile);
    }
}
