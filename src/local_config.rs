use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use path_slash::PathBufExt as _;
use serde::{Deserialize, Serialize};

use crate::adapters::Adapter;
use crate::paths::display_path;
use crate::store::write_atomic;

const LOCAL_DIR: &str = ".nodus";
const LOCAL_CONFIG_FILE: &str = "local.toml";
const LOCAL_GITIGNORE_FILE: &str = ".gitignore";
const LOCAL_GITIGNORE_ENTRIES: [&str; 2] = [".gitignore", "local.toml"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalConfig {
    #[serde(default)]
    pub relay: BTreeMap<String, RelayLink>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayLink {
    #[serde(
        serialize_with = "serialize_repo_path",
        deserialize_with = "deserialize_repo_path"
    )]
    pub repo_path: PathBuf,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<Adapter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_digest: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, RelayedFileState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayedFileState {
    #[serde(alias = "source_sha256")]
    pub source_hash: String,
}

impl LocalConfig {
    pub fn load_in_dir(project_root: &Path) -> Result<Self> {
        let path = config_path(project_root);
        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read local config {}", path.display()))?;
        toml::from_str(&contents)
            .with_context(|| format!("failed to parse local config {}", path.display()))
    }

    pub fn save_in_dir(&self, project_root: &Path) -> Result<()> {
        let local_dir = local_dir(project_root);
        fs::create_dir_all(&local_dir)
            .with_context(|| format!("failed to create {}", local_dir.display()))?;

        let path = config_path(project_root);
        let contents = toml::to_string_pretty(self).context("failed to serialize local config")?;
        write_atomic(&path, contents.as_bytes())
            .with_context(|| format!("failed to write local config {}", path.display()))?;
        ensure_local_gitignore(project_root)
    }

    pub fn relay_link(&self, alias: &str) -> Option<&RelayLink> {
        self.relay.get(alias)
    }

    pub fn relay_link_mut(&mut self, alias: &str) -> Option<&mut RelayLink> {
        self.relay.get_mut(alias)
    }

    pub fn set_relay_link(&mut self, alias: impl Into<String>, link: RelayLink) {
        self.relay.insert(alias.into(), link);
    }
}

pub fn ensure_local_gitignore(project_root: &Path) -> Result<()> {
    let local_dir = local_dir(project_root);
    fs::create_dir_all(&local_dir)
        .with_context(|| format!("failed to create {}", local_dir.display()))?;
    let gitignore_path = local_dir.join(LOCAL_GITIGNORE_FILE);
    let mut lines = if gitignore_path.exists() {
        fs::read_to_string(&gitignore_path)
            .with_context(|| format!("failed to read {}", gitignore_path.display()))?
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    for entry in LOCAL_GITIGNORE_ENTRIES {
        if !lines.iter().any(|line| line.trim() == entry) {
            lines.push(entry.to_string());
        }
    }

    let mut contents = lines.join("\n");
    if !contents.is_empty() {
        contents.push('\n');
    }
    write_atomic(&gitignore_path, contents.as_bytes())
        .with_context(|| format!("failed to write {}", gitignore_path.display()))
}

pub fn config_path(project_root: &Path) -> PathBuf {
    local_dir(project_root).join(LOCAL_CONFIG_FILE)
}

pub fn local_dir(project_root: &Path) -> PathBuf {
    project_root.join(LOCAL_DIR)
}

fn serialize_repo_path<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&display_path(path))
}

fn deserialize_repo_path<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
where
    D: serde::Deserializer<'de>,
{
    String::deserialize(deserializer).map(PathBuf::from_slash)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn round_trips_local_config_and_gitignore() {
        let temp = TempDir::new().unwrap();
        let mut config = LocalConfig::default();
        config.set_relay_link(
            "playbook_ios",
            RelayLink {
                repo_path: PathBuf::from("/tmp/playbook-ios"),
                url: "https://github.com/wenext-limited/playbook-ios".into(),
                via: Some(Adapter::Claude),
                package_digest: Some("sha256:relay-state".into()),
                files: BTreeMap::from([(
                    "skills/review/SKILL.md".into(),
                    RelayedFileState {
                        source_hash: "abc123".into(),
                    },
                )]),
            },
        );

        config.save_in_dir(temp.path()).unwrap();
        config.save_in_dir(temp.path()).unwrap();

        let reloaded = LocalConfig::load_in_dir(temp.path()).unwrap();
        assert_eq!(reloaded, config);

        let gitignore = fs::read_to_string(temp.path().join(".nodus/.gitignore")).unwrap();
        assert_eq!(gitignore, ".gitignore\nlocal.toml\n");
    }

    #[test]
    fn serializes_relay_repo_paths_with_forward_slashes() {
        let config = LocalConfig {
            relay: BTreeMap::from([(
                "playbook_ios".into(),
                RelayLink {
                    repo_path: PathBuf::from(
                        r"C:\Users\runneradmin\AppData\Local\Temp\playbook-ios",
                    ),
                    url: "https://github.com/wenext-limited/playbook-ios".into(),
                    via: Some(Adapter::Codex),
                    package_digest: Some("sha256:def".into()),
                    files: BTreeMap::from([(
                        "skills/review/SKILL.md".into(),
                        RelayedFileState {
                            source_hash: "123456".into(),
                        },
                    )]),
                },
            )]),
        };

        let encoded = toml::to_string_pretty(&config).unwrap();
        let decoded: toml::Value = toml::from_str(&encoded).unwrap();
        let expected = display_path(&config.relay["playbook_ios"].repo_path);

        assert_eq!(
            decoded["relay"]["playbook_ios"]["repo_path"].as_str(),
            Some(expected.as_str())
        );
        assert_eq!(
            decoded["relay"]["playbook_ios"]["via"].as_str(),
            Some("codex")
        );
        assert_eq!(
            decoded["relay"]["playbook_ios"]["package_digest"].as_str(),
            Some("sha256:def")
        );
        assert_eq!(
            decoded["relay"]["playbook_ios"]["files"]["skills/review/SKILL.md"]["source_hash"]
                .as_str(),
            Some("123456")
        );
    }
}
