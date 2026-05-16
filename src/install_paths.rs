use std::env;
use std::path::{Path, PathBuf};

#[cfg(not(target_os = "windows"))]
use anyhow::Context;
use anyhow::Result;

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
    pub codex_user_config: Option<PathBuf>,
}

impl InstallPaths {
    pub fn project(root: &Path) -> Self {
        let root = root.to_path_buf();
        #[cfg(test)]
        let codex_user_config = None;
        #[cfg(not(test))]
        let codex_user_config = resolve_codex_user_config_path();
        Self {
            scope: InstallScope::Project,
            config_root: root.clone(),
            runtime_root: root.clone(),
            adapter_detection_root: root,
            codex_user_config,
        }
    }

    pub fn global(store_root: &Path) -> Result<Self> {
        let home = resolve_home_dir()?;
        #[cfg(test)]
        let codex_user_config = None;
        #[cfg(not(test))]
        let codex_user_config = resolve_codex_user_config_path();
        Ok(Self::new(
            InstallScope::Global,
            store_root.join("global"),
            home.clone(),
            home,
        )
        .with_codex_user_config(codex_user_config))
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
            codex_user_config: None,
        }
    }

    pub fn with_codex_user_config(mut self, path: Option<PathBuf>) -> Self {
        self.codex_user_config = path;
        self
    }

    pub const fn is_global(&self) -> bool {
        matches!(self.scope, InstallScope::Global)
    }
}

#[cfg(not(test))]
fn resolve_codex_user_config_path() -> Option<PathBuf> {
    resolve_codex_user_config_path_from_env(
        env::var_os("NODUS_DISABLE_CODEX_USER_CONFIG").as_deref(),
        env::var_os("NODUS_ENABLE_CODEX_USER_CONFIG").as_deref(),
        env::var_os("CODEX_HOME").map(PathBuf::from),
        resolve_home_dir().ok(),
    )
}

fn resolve_codex_user_config_path_from_env(
    disable: Option<&std::ffi::OsStr>,
    legacy_enable: Option<&std::ffi::OsStr>,
    codex_home: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    if !codex_user_config_writes_enabled_from_env(disable, legacy_enable) {
        return None;
    }

    codex_home
        .map(|home| home.join("config.toml"))
        .or_else(|| home.map(|home| home.join(".codex").join("config.toml")))
}

pub(crate) fn codex_user_config_writes_enabled() -> bool {
    codex_user_config_writes_enabled_from_env(
        env::var_os("NODUS_DISABLE_CODEX_USER_CONFIG").as_deref(),
        env::var_os("NODUS_ENABLE_CODEX_USER_CONFIG").as_deref(),
    )
}

fn codex_user_config_writes_enabled_from_env(
    disable: Option<&std::ffi::OsStr>,
    legacy_enable: Option<&std::ffi::OsStr>,
) -> bool {
    if env_value_truthy(disable) {
        return false;
    }

    legacy_enable.is_none_or(|value| env_value_truthy(Some(value)))
}

fn env_value_truthy(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|raw| raw == "1" || raw.eq_ignore_ascii_case("true"))
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
    use std::ffi::OsStr;

    #[test]
    fn project_scope_reuses_the_same_root_for_all_paths() {
        let root = Path::new("/tmp/project");
        let paths = InstallPaths::project(root);

        assert_eq!(paths.scope, InstallScope::Project);
        assert_eq!(paths.config_root, root);
        assert_eq!(paths.runtime_root, root);
        assert_eq!(paths.adapter_detection_root, root);
        assert_eq!(paths.codex_user_config, None);
    }

    #[test]
    fn codex_user_config_is_enabled_by_default() {
        assert!(codex_user_config_writes_enabled_from_env(None, None));
    }

    #[test]
    fn codex_user_config_legacy_enable_accepts_truthy_values() {
        assert!(codex_user_config_writes_enabled_from_env(
            None,
            Some(OsStr::new("1"))
        ));
        assert!(codex_user_config_writes_enabled_from_env(
            None,
            Some(OsStr::new("true"))
        ));
        assert!(codex_user_config_writes_enabled_from_env(
            None,
            Some(OsStr::new("TRUE"))
        ));
        assert!(codex_user_config_writes_enabled_from_env(
            None,
            Some(OsStr::new("True"))
        ));
    }

    #[test]
    fn codex_user_config_legacy_enable_disables_for_other_values() {
        assert!(!codex_user_config_writes_enabled_from_env(
            None,
            Some(OsStr::new("0"))
        ));
        assert!(!codex_user_config_writes_enabled_from_env(
            None,
            Some(OsStr::new("false"))
        ));
        assert!(!codex_user_config_writes_enabled_from_env(
            None,
            Some(OsStr::new(""))
        ));
        assert!(!codex_user_config_writes_enabled_from_env(
            None,
            Some(OsStr::new("yes"))
        ));
    }

    #[test]
    fn codex_user_config_disable_env_wins() {
        assert!(!codex_user_config_writes_enabled_from_env(
            Some(OsStr::new("1")),
            None
        ));
        assert!(!codex_user_config_writes_enabled_from_env(
            Some(OsStr::new("true")),
            Some(OsStr::new("1"))
        ));
    }

    #[test]
    fn codex_user_config_resolves_codex_home_first() {
        assert_eq!(
            resolve_codex_user_config_path_from_env(
                None,
                None,
                Some(PathBuf::from("/tmp/codex-home")),
                Some(PathBuf::from("/tmp/home"))
            ),
            Some(PathBuf::from("/tmp/codex-home/config.toml"))
        );
    }

    #[test]
    fn codex_user_config_resolves_home_by_default() {
        assert_eq!(
            resolve_codex_user_config_path_from_env(
                None,
                None,
                None,
                Some(PathBuf::from("/tmp/home"))
            ),
            Some(PathBuf::from("/tmp/home/.codex/config.toml"))
        );
    }

    #[test]
    fn codex_user_config_resolution_can_be_disabled() {
        assert_eq!(
            resolve_codex_user_config_path_from_env(
                Some(OsStr::new("true")),
                None,
                Some(PathBuf::from("/tmp/codex-home")),
                Some(PathBuf::from("/tmp/home"))
            ),
            None
        );
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
        assert_eq!(paths.codex_user_config, None);
    }
}
