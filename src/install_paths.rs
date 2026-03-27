use std::env;
use std::path::{Path, PathBuf};

use anyhow::Result;
#[cfg(not(target_os = "windows"))]
use anyhow::Context;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallScope {
    Project,
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPaths {
    pub scope: InstallScope,
    pub config_root: PathBuf,
    pub runtime_root: PathBuf,
    pub adapter_detection_root: PathBuf,
}

impl InstallPaths {
    pub fn project(root: &Path) -> Self {
        let root = root.to_path_buf();
        Self {
            scope: InstallScope::Project,
            config_root: root.clone(),
            runtime_root: root.clone(),
            adapter_detection_root: root,
        }
    }

    pub fn global(store_root: &Path) -> Result<Self> {
        let home = resolve_home_dir()?;
        Ok(Self::new(
            InstallScope::Global,
            store_root.join("global"),
            home.clone(),
            home,
        ))
    }

    pub fn new(
        scope: InstallScope,
        config_root: PathBuf,
        runtime_root: PathBuf,
        adapter_detection_root: PathBuf,
    ) -> Self {
        Self {
            scope,
            config_root,
            runtime_root,
            adapter_detection_root,
        }
    }

    pub const fn is_global(&self) -> bool {
        matches!(self.scope, InstallScope::Global)
    }
}

fn resolve_home_dir() -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        if let Some(profile) = env::var_os("USERPROFILE") {
            return Ok(PathBuf::from(profile));
        }
        if let (Some(drive), Some(path)) = (env::var_os("HOMEDRIVE"), env::var_os("HOMEPATH")) {
            return Ok(PathBuf::from(drive).join(path));
        }
        anyhow::bail!("failed to determine the home directory for global installs");
    }

    #[cfg(not(target_os = "windows"))]
    {
        env::var_os("HOME")
            .map(PathBuf::from)
            .context("failed to determine the home directory for global installs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_scope_reuses_the_same_root_for_all_paths() {
        let root = Path::new("/tmp/project");
        let paths = InstallPaths::project(root);

        assert_eq!(paths.scope, InstallScope::Project);
        assert_eq!(paths.config_root, root);
        assert_eq!(paths.runtime_root, root);
        assert_eq!(paths.adapter_detection_root, root);
    }

    #[test]
    fn global_scope_uses_store_root_for_config_and_home_for_runtime() {
        let store_root = Path::new("/tmp/nodus-store");
        let home = PathBuf::from("/tmp/home");
        let paths = InstallPaths::new(
            InstallScope::Global,
            store_root.join("global"),
            home.clone(),
            home.clone(),
        );

        assert_eq!(paths.scope, InstallScope::Global);
        assert_eq!(paths.config_root, PathBuf::from("/tmp/nodus-store/global"));
        assert_eq!(paths.runtime_root, home);
        assert_eq!(paths.adapter_detection_root, PathBuf::from("/tmp/home"));
    }
}
