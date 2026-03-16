use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::manifest::Capability;

pub const LOCKFILE_NAME: &str = "agentpack.lock";
const LOCKFILE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    pub version: u32,
    pub packages: Vec<LockedPackage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPackage {
    pub name: String,
    pub package_version: Version,
    pub source: LockedSource,
    pub digest: String,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedSource {
    pub kind: String,
    pub path: String,
}

impl Lockfile {
    pub fn new(mut packages: Vec<LockedPackage>) -> Self {
        packages.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then(left.package_version.cmp(&right.package_version))
                .then(left.source.path.cmp(&right.source.path))
        });
        Self {
            version: LOCKFILE_VERSION,
            packages,
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
        fs::write(path, contents)
            .with_context(|| format!("failed to write lockfile {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_lockfile_as_toml() {
        let lockfile = Lockfile::new(vec![LockedPackage {
            name: "example".into(),
            package_version: Version::new(0, 1, 0),
            source: LockedSource {
                kind: "path".into(),
                path: ".".into(),
            },
            digest: "sha256:abc".into(),
            skills: vec!["review".into()],
            agents: vec!["security-reviewer".into()],
            rules: vec!["safe-shell".into()],
            dependencies: vec!["shared".into()],
            capabilities: vec![Capability {
                id: "shell.exec".into(),
                sensitivity: "high".into(),
                justification: Some("Needed for tests".into()),
            }],
        }]);

        let encoded = toml::to_string_pretty(&lockfile).unwrap();
        let decoded: Lockfile = toml::from_str(&encoded).unwrap();

        assert_eq!(decoded, lockfile);
    }
}
