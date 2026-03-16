use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::store::write_atomic;

pub const STATE_FILE: &str = ".agen/state.json";
const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncState {
    pub version: u32,
    #[serde(default)]
    pub files: Vec<String>,
}

impl SyncState {
    pub fn load(project_root: &Path) -> Result<Self> {
        let path = project_root.join(STATE_FILE);
        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read sync state {}", path.display()))?;
        let state: SyncState = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse sync state {}", path.display()))?;
        Ok(state)
    }

    pub fn save(project_root: &Path, file_paths: impl IntoIterator<Item = PathBuf>) -> Result<()> {
        let mut files = file_paths
            .into_iter()
            .map(|path| normalize_relative(project_root, &path))
            .collect::<Result<Vec<_>>>()?;
        files.sort();
        files.dedup();

        let state = SyncState {
            version: STATE_VERSION,
            files,
        };
        let encoded =
            serde_json::to_vec_pretty(&state).context("failed to serialize sync state")?;
        write_atomic(&project_root.join(STATE_FILE), &encoded)
    }

    pub fn owned_paths(&self, project_root: &Path) -> HashSet<PathBuf> {
        self.files
            .iter()
            .map(|relative| project_root.join(relative))
            .collect()
    }
}

fn normalize_relative(project_root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(project_root).with_context(|| {
        format!(
            "managed path {} is not inside project root {}",
            path.display(),
            project_root.display()
        )
    })?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn persists_relative_owned_paths() {
        let temp = TempDir::new().unwrap();
        let owned = vec![
            temp.path().join(".claude/skills/review_a1b2c3/SKILL.md"),
            temp.path().join(".codex/rules/default.rules"),
        ];

        SyncState::save(temp.path(), owned.clone()).unwrap();
        let reloaded = SyncState::load(temp.path()).unwrap();

        assert_eq!(reloaded.version, STATE_VERSION);
        assert_eq!(reloaded.owned_paths(temp.path()).len(), 2);
        assert!(reloaded.owned_paths(temp.path()).contains(&owned[0]));
    }
}
