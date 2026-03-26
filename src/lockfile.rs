use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::manifest::{Capability, DependencyComponent};
#[cfg(test)]
use crate::store::write_atomic;

pub const LOCKFILE_NAME: &str = "nodus.lock";
const LOCKFILE_VERSION: u32 = 8;
const MIN_SYNC_COMPATIBLE_LOCKFILE_VERSION: u32 = 4;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_components: Option<Vec<DependencyComponent>>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
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
    pub branch: Option<String>,
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
                .then(left.source.branch.cmp(&right.source.branch))
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
        let lockfile = Self::read_unvalidated(path)?;
        lockfile.ensure_current_version(path)?;
        Ok(lockfile)
    }

    pub fn read_for_sync(path: &Path) -> Result<Self> {
        let lockfile = Self::read_unvalidated(path)?;
        lockfile.ensure_sync_compatible_version(path)?;
        Ok(lockfile)
    }

    pub const fn current_version() -> u32 {
        LOCKFILE_VERSION
    }

    pub const fn uses_current_schema(&self) -> bool {
        self.version == LOCKFILE_VERSION
    }

    pub fn managed_paths_for_sync(&self, project_root: &Path) -> Result<HashSet<PathBuf>> {
        let mut managed_paths = self.managed_paths(project_root)?;
        if self.uses_current_schema() {
            return Ok(managed_paths);
        }

        for relative in &self.managed_files {
            let relative_path = Self::validate_managed_relative(relative, project_root)?;
            managed_paths.insert(project_root.join(relative_path));
            if let Some(paths) = self.expand_legacy_managed_root(project_root, relative_path) {
                managed_paths.extend(paths);
            }
        }

        Ok(managed_paths)
    }

    fn read_unvalidated(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read lockfile {}", path.display()))?;
        toml::from_str(&contents)
            .with_context(|| format!("failed to parse lockfile {}", path.display()))
    }

    #[cfg(test)]
    pub fn write(&self, path: &Path) -> Result<()> {
        let contents = toml::to_string_pretty(self).context("failed to serialize lockfile")?;
        write_atomic(path, contents.as_bytes())
            .with_context(|| format!("failed to write lockfile {}", path.display()))
    }

    pub fn managed_paths(&self, project_root: &Path) -> Result<HashSet<PathBuf>> {
        let mut managed_paths = HashSet::new();

        for relative in &self.managed_files {
            let relative_path = Self::validate_managed_relative(relative, project_root)?;
            if let Some(paths) = self.expand_managed_root(project_root, relative_path) {
                managed_paths.extend(paths);
            } else {
                managed_paths.insert(project_root.join(relative_path));
            }
        }

        Ok(managed_paths)
    }

    fn ensure_current_version(&self, path: &Path) -> Result<()> {
        if self.version != LOCKFILE_VERSION {
            bail!(
                "unsupported lockfile version {} in {}; expected {}",
                self.version,
                path.display(),
                LOCKFILE_VERSION
            );
        }

        Ok(())
    }

    fn ensure_sync_compatible_version(&self, path: &Path) -> Result<()> {
        if !(MIN_SYNC_COMPATIBLE_LOCKFILE_VERSION..=LOCKFILE_VERSION).contains(&self.version) {
            bail!(
                "unsupported lockfile version {} in {}; expected {} through {}",
                self.version,
                path.display(),
                MIN_SYNC_COMPATIBLE_LOCKFILE_VERSION,
                LOCKFILE_VERSION
            );
        }

        Ok(())
    }

    fn validate_managed_relative<'a>(relative: &'a str, project_root: &Path) -> Result<&'a Path> {
        let relative_path = Path::new(relative);
        if relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            bail!(
                "managed path {} escapes project root {}",
                relative,
                project_root.display()
            );
        }
        Ok(relative_path)
    }

    fn expand_managed_root(
        &self,
        project_root: &Path,
        relative_path: &Path,
    ) -> Option<Vec<PathBuf>> {
        let components = relative_path
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        let [runtime, artifact_dir, artifact_name] = components.as_slice() else {
            return None;
        };

        if *runtime != ".agents"
            && *runtime != ".claude"
            && *runtime != ".codex"
            && *runtime != ".github"
            && *runtime != ".cursor"
            && *runtime != ".opencode"
        {
            return None;
        }

        let paths = match artifact_dir.as_str() {
            "skills" => self
                .packages
                .iter()
                .filter(|package| {
                    package
                        .skills
                        .iter()
                        .any(|existing| existing == artifact_name)
                })
                .map(|package| {
                    project_root.join(format!(
                        "{runtime}/skills/{}_{}",
                        artifact_name,
                        locked_package_short_id(package)
                    ))
                })
                .collect::<Vec<_>>(),
            "agents" if runtime == ".github" => self
                .packages
                .iter()
                .filter(|package| {
                    package
                        .agents
                        .iter()
                        .any(|existing| existing == artifact_name)
                })
                .map(|package| {
                    project_root.join(format!(
                        "{runtime}/agents/{}_{}.agent.md",
                        artifact_name,
                        locked_package_short_id(package)
                    ))
                })
                .collect::<Vec<_>>(),
            "agents" | "rules" | "commands" => {
                let (artifact_id, extension) =
                    split_managed_file_name(runtime.as_str(), artifact_dir, artifact_name)?;
                self.packages
                    .iter()
                    .filter(|package| match artifact_dir.as_str() {
                        "agents" => package
                            .agents
                            .iter()
                            .any(|existing| existing == artifact_id),
                        "rules" => package.rules.iter().any(|existing| existing == artifact_id),
                        "commands" => package
                            .commands
                            .iter()
                            .any(|existing| existing == artifact_id),
                        _ => false,
                    })
                    .map(|package| {
                        project_root.join(format!(
                            "{runtime}/{artifact_dir}/{}_{}.{}",
                            artifact_id,
                            locked_package_short_id(package),
                            extension
                        ))
                    })
                    .collect::<Vec<_>>()
            }
            _ => return None,
        };

        if paths.is_empty() { None } else { Some(paths) }
    }

    fn expand_legacy_managed_root(
        &self,
        project_root: &Path,
        relative_path: &Path,
    ) -> Option<Vec<PathBuf>> {
        let components = relative_path
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let [runtime, artifact_dir, artifact_name] = components.as_slice() else {
            return None;
        };
        if *runtime != ".github" || *artifact_dir != "agents" {
            return None;
        }

        let artifact_id = artifact_name.strip_suffix(".agent.md")?;
        let paths = self
            .packages
            .iter()
            .filter(|package| {
                package
                    .agents
                    .iter()
                    .any(|existing| existing == artifact_id)
            })
            .map(|package| {
                project_root.join(format!(
                    ".github/agents/{}_{}.agent.md",
                    artifact_id,
                    locked_package_short_id(package)
                ))
            })
            .collect::<Vec<_>>();

        if paths.is_empty() { None } else { Some(paths) }
    }

    pub fn managed_mcp_server_names(&self) -> HashSet<String> {
        self.packages
            .iter()
            .flat_map(|package| {
                package
                    .mcp_servers
                    .iter()
                    .map(|server_id| managed_mcp_server_name(&package.alias, server_id))
            })
            .collect()
    }
}

fn split_managed_file_name<'a>(
    _runtime: &str,
    _artifact_dir: &str,
    artifact_name: &'a str,
) -> Option<(&'a str, &'a str)> {
    artifact_name.rsplit_once('.')
}

pub fn managed_mcp_server_name(package_alias: &str, server_id: &str) -> String {
    format!("{package_alias}__{server_id}")
}

fn locked_package_short_id(package: &LockedPackage) -> String {
    match package.source.kind.as_str() {
        "git" => short_source_id(
            package
                .source
                .rev
                .as_deref()
                .unwrap_or(package.digest.as_str()),
        ),
        _ => short_source_id(
            package
                .digest
                .strip_prefix("sha256:")
                .unwrap_or(&package.digest),
        ),
    }
}

fn short_source_id(value: &str) -> String {
    let short = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(6)
        .collect::<String>()
        .to_ascii_lowercase();

    if short.is_empty() {
        "local0".into()
    } else {
        short
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

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
                    branch: None,
                    rev: Some("abc123".into()),
                },
                digest: "sha256:abc".into(),
                selected_components: Some(vec![DependencyComponent::Skills]),
                skills: vec!["review".into()],
                agents: vec!["security-reviewer".into()],
                rules: vec!["safe-shell".into()],
                commands: vec!["build".into()],
                mcp_servers: vec!["firebase".into()],
                dependencies: vec![],
                capabilities: vec![Capability {
                    id: "shell.exec".into(),
                    sensitivity: "high".into(),
                    justification: Some("Needed for tests".into()),
                }],
            }],
            vec![
                ".claude/skills/review".into(),
                ".codex/skills/review".into(),
            ],
        );

        let encoded = toml::to_string_pretty(&lockfile).unwrap();
        let decoded: Lockfile = toml::from_str(&encoded).unwrap();

        assert_eq!(decoded, lockfile);
    }

    #[test]
    fn rejects_unsupported_lockfile_versions() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        fs::write(
            &path,
            r#"
version = 5
packages = []
managed_files = []
"#,
        )
        .unwrap();

        let error = Lockfile::read(&path).unwrap_err().to_string();

        assert!(error.contains("unsupported lockfile version 5"));
    }

    #[test]
    fn read_for_sync_accepts_legacy_lockfile_versions() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        fs::write(
            &path,
            r#"
version = 4
packages = []
managed_files = []
"#,
        )
        .unwrap();

        let lockfile = Lockfile::read_for_sync(&path).unwrap();

        assert_eq!(lockfile.version, 4);
    }

    #[test]
    fn managed_paths_for_sync_include_legacy_direct_paths() {
        let lockfile = Lockfile {
            version: 4,
            packages: vec![LockedPackage {
                alias: "shared".into(),
                name: "shared".into(),
                version_tag: Some("v0.1.0".into()),
                source: LockedSource {
                    kind: "git".into(),
                    path: None,
                    url: Some("https://github.com/example/shared".into()),
                    tag: Some("v0.1.0".into()),
                    branch: None,
                    rev: Some("01f556abcdef".into()),
                },
                digest: "sha256:abc".into(),
                selected_components: None,
                skills: vec!["review".into()],
                agents: vec!["security".into()],
                rules: vec![],
                commands: vec!["build".into()],
                mcp_servers: vec!["firebase".into()],
                dependencies: vec![],
                capabilities: vec![],
            }],
            managed_files: vec![
                ".claude/skills/review".into(),
                ".claude/agents/security.md".into(),
                ".opencode/commands/build.md".into(),
            ],
        };

        let managed_paths = lockfile
            .managed_paths_for_sync(Path::new("/tmp/project"))
            .unwrap();

        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.claude/skills/review")));
        assert!(
            managed_paths.contains(&PathBuf::from("/tmp/project/.claude/skills/review_01f556"))
        );
        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.claude/agents/security.md")));
        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.opencode/commands/build.md")));
    }

    #[test]
    fn expands_logical_skill_roots_to_namespaced_directories() {
        let lockfile = Lockfile::new(
            vec![LockedPackage {
                alias: "iframe_ad".into(),
                name: "iframe-ad".into(),
                version_tag: Some("v0.1.0".into()),
                source: LockedSource {
                    kind: "git".into(),
                    path: None,
                    url: Some("https://github.com/example/iframe-ad".into()),
                    tag: Some("v0.1.0".into()),
                    branch: None,
                    rev: Some("01f556abcdef".into()),
                },
                digest: "sha256:abc".into(),
                selected_components: None,
                skills: vec!["iframe-ad".into()],
                agents: vec![],
                rules: vec![],
                commands: vec![],
                mcp_servers: vec![],
                dependencies: vec![],
                capabilities: vec![],
            }],
            vec![
                ".agents/skills/iframe-ad".into(),
                ".claude/skills/iframe-ad".into(),
                ".codex/skills/iframe-ad".into(),
                ".github/skills/iframe-ad".into(),
                ".cursor/skills/iframe-ad".into(),
                ".opencode/skills/iframe-ad".into(),
            ],
        );

        let managed_paths = lockfile.managed_paths(Path::new("/tmp/project")).unwrap();

        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.agents/skills/iframe-ad_01f556"
        )));
        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.claude/skills/iframe-ad_01f556"
        )));
        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.codex/skills/iframe-ad_01f556"
        )));
        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.github/skills/iframe-ad_01f556"
        )));
        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.cursor/skills/iframe-ad_01f556"
        )));
        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.opencode/skills/iframe-ad_01f556"
        )));
    }

    #[test]
    fn expands_logical_file_outputs_to_namespaced_files() {
        let lockfile = Lockfile::new(
            vec![LockedPackage {
                alias: "shared".into(),
                name: "shared".into(),
                version_tag: Some("v0.1.0".into()),
                source: LockedSource {
                    kind: "git".into(),
                    path: None,
                    url: Some("https://github.com/example/shared".into()),
                    tag: Some("v0.1.0".into()),
                    branch: None,
                    rev: Some("01f556abcdef".into()),
                },
                digest: "sha256:abc".into(),
                selected_components: None,
                skills: vec![],
                agents: vec!["security".into()],
                rules: vec!["default".into()],
                commands: vec!["build".into()],
                mcp_servers: vec![],
                dependencies: vec![],
                capabilities: vec![],
            }],
            vec![
                ".agents/commands/build.md".into(),
                ".claude/agents/security.md".into(),
                ".claude/commands/build.md".into(),
                ".claude/rules/default.md".into(),
                ".github/agents/security".into(),
                ".cursor/commands/build.md".into(),
                ".cursor/rules/default.mdc".into(),
                ".opencode/agents/security.md".into(),
                ".opencode/commands/build.md".into(),
                ".opencode/rules/default.md".into(),
            ],
        );

        let managed_paths = lockfile.managed_paths(Path::new("/tmp/project")).unwrap();

        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.agents/commands/build_01f556.md"
        )));
        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.claude/agents/security_01f556.md"
        )));
        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.claude/commands/build_01f556.md"
        )));
        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.claude/rules/default_01f556.md"
        )));
        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.github/agents/security_01f556.agent.md"
        )));
        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.cursor/commands/build_01f556.md"
        )));
        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.cursor/rules/default_01f556.mdc"
        )));
        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.opencode/agents/security_01f556.md"
        )));
        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.opencode/commands/build_01f556.md"
        )));
        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.opencode/rules/default_01f556.md"
        )));
    }

    #[test]
    fn keeps_direct_github_agent_files_exact_in_current_lockfiles() {
        let lockfile = Lockfile::new(
            vec![LockedPackage {
                alias: "shared".into(),
                name: "shared".into(),
                version_tag: Some("v0.1.0".into()),
                source: LockedSource {
                    kind: "git".into(),
                    path: None,
                    url: Some("https://github.com/example/shared".into()),
                    tag: Some("v0.1.0".into()),
                    branch: None,
                    rev: Some("01f556abcdef".into()),
                },
                digest: "sha256:abc".into(),
                selected_components: None,
                skills: vec![],
                agents: vec!["security".into()],
                rules: vec![],
                commands: vec![],
                mcp_servers: vec![],
                dependencies: vec![],
                capabilities: vec![],
            }],
            vec![".github/agents/security.agent.md".into()],
        );

        let managed_paths = lockfile.managed_paths(Path::new("/tmp/project")).unwrap();

        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.github/agents/security.agent.md"
        )));
        assert!(!managed_paths.contains(&PathBuf::from(
            "/tmp/project/.github/agents/security_01f556.agent.md"
        )));
    }

    #[test]
    fn managed_paths_for_sync_expand_legacy_github_agent_roots() {
        let lockfile = Lockfile {
            version: 7,
            packages: vec![LockedPackage {
                alias: "shared".into(),
                name: "shared".into(),
                version_tag: Some("v0.1.0".into()),
                source: LockedSource {
                    kind: "git".into(),
                    path: None,
                    url: Some("https://github.com/example/shared".into()),
                    tag: Some("v0.1.0".into()),
                    branch: None,
                    rev: Some("01f556abcdef".into()),
                },
                digest: "sha256:abc".into(),
                selected_components: None,
                skills: vec![],
                agents: vec!["security".into()],
                rules: vec![],
                commands: vec![],
                mcp_servers: vec![],
                dependencies: vec![],
                capabilities: vec![],
            }],
            managed_files: vec![".github/agents/security.agent.md".into()],
        };

        let managed_paths = lockfile
            .managed_paths_for_sync(Path::new("/tmp/project"))
            .unwrap();

        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.github/agents/security.agent.md"
        )));
        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.github/agents/security_01f556.agent.md"
        )));
    }

    #[test]
    fn managed_mcp_server_names_include_alias_prefixes() {
        let lockfile = Lockfile {
            version: LOCKFILE_VERSION,
            packages: vec![LockedPackage {
                alias: "firebase".into(),
                name: "firebase-tools".into(),
                version_tag: Some("1.0.0".into()),
                source: LockedSource {
                    kind: "git".into(),
                    path: None,
                    url: Some("https://github.com/firebase/firebase-tools".into()),
                    tag: Some("v1.0.0".into()),
                    branch: None,
                    rev: Some("abc123".into()),
                },
                digest: "sha256:abc".into(),
                selected_components: None,
                skills: vec![],
                agents: vec![],
                rules: vec![],
                commands: vec![],
                mcp_servers: vec!["firebase".into()],
                dependencies: vec![],
                capabilities: vec![],
            }],
            managed_files: vec![".mcp.json".into()],
        };

        assert_eq!(
            lockfile.managed_mcp_server_names(),
            HashSet::from([String::from("firebase__firebase")])
        );
    }
}
