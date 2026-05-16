use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::adapters::{ArtifactKind, ManagedArtifactNames, short_source_id};
use crate::manifest::{Capability, DependencyComponent};
#[cfg(test)]
use crate::store::write_atomic;

pub const LOCKFILE_NAME: &str = "nodus.lock";
const LOCKFILE_VERSION: u32 = 10;
const MIN_SYNC_COMPATIBLE_LOCKFILE_VERSION: u32 = 4;

/// Sentinel written in place of the root package's `name` in the lockfile. The
/// root package's display name is derived from the folder name today, which
/// makes the lockfile (and the root digest) non-stable across folder renames.
/// v10 lockfiles record the sentinel so renames do not produce diffs.
pub const ROOT_PACKAGE_NAME_SENTINEL: &str = "<root>";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    pub version: u32,
    pub packages: Vec<LockedPackage>,
    /// Legacy v9 read-only fallback. Empty for v10 writes.
    ///
    /// Populated when reading a pre-v10 lockfile so that the existing
    /// `managed_paths`/`managed_paths_for_sync` plumbing keeps working through
    /// the schema transition. v10 writes never emit this list — per-package
    /// `owned_subtrees`/`owned_prefixes`/`owned_files` take over the role of
    /// recording what Nodus owns.
    #[serde(
        rename = "managed_files",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub legacy_managed_files: Vec<String>,
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
    /// Directories whose entire contents Nodus owns. Each entry is a relative
    /// path from the workspace root. Populated by the emission slice (Slice 3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owned_subtrees: Vec<String>,
    /// Directory + filename-prefix rules. Nodus owns files inside `dir` whose
    /// names start with `prefix`. Strict prefix match — no globs. Populated by
    /// the emission slice (Slice 3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owned_prefixes: Vec<OwnedPrefix>,
    /// Exact owned file paths (relative to workspace root). For files that
    /// don't fit a subtree root or prefix rule. Populated by the emission slice
    /// (Slice 3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owned_files: Vec<String>,
    /// Drift detector. Computed by the emission slice (Slice 3) — not this
    /// one. Format: `"blake3:<hex>"`. `None` means "drift unknown, recompute on
    /// next sync".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedPrefix {
    pub dir: String,
    pub prefix: String,
}

/// In-memory ownership view derived from a [`Lockfile`]. Combines exact paths,
/// subtree roots, and filename-prefix rules so callers can answer "does Nodus
/// own this path?" without re-parsing the lockfile each call.
///
/// Slice 1 stored ownership claims as raw lockfile fields; this slice exposes
/// them through a single richer type that the sync/clean paths consult.
#[derive(Debug, Default, Clone)]
pub struct OwnedSet {
    /// Concrete owned paths. Sourced from per-package `owned_files`, the v9
    /// `legacy_managed_files` expansion, and the existing artifact-root
    /// expansion in [`Lockfile::managed_paths`].
    pub exact: HashSet<PathBuf>,
    /// Subtree roots — Nodus owns the root and everything nested under it.
    /// Sourced from per-package `owned_subtrees`.
    pub subtrees: Vec<PathBuf>,
    /// Filename-prefix rules — Nodus owns files in `dir` whose `file_name`
    /// starts with `prefix`. Strict prefix match, no globs, no recursion.
    pub prefixes: Vec<OwnedPrefixPath>,
}

/// Resolved (project-root-joined) form of [`OwnedPrefix`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedPrefixPath {
    pub dir: PathBuf,
    pub prefix: String,
}

impl OwnedSet {
    /// True if `path` is owned by any of the exact paths, subtree roots, or
    /// filename-prefix rules in this set.
    ///
    /// Exact-path matching also honors `starts_with` semantics so that a
    /// directory entry in `exact` covers everything nested under it. This
    /// matches the legacy [`Lockfile::managed_paths`] consumers, which today
    /// receive directory roots (e.g. `.agents/skills/review`) and rely on
    /// `path.starts_with(owned)` to claim child files. Slice 3's emission
    /// rewrite will move those roots into [`OwnedSet::subtrees`] explicitly.
    pub fn contains(&self, path: &Path) -> bool {
        if self.exact.contains(path) {
            return true;
        }
        for owned in &self.exact {
            if path.starts_with(owned) {
                return true;
            }
        }
        for subtree in &self.subtrees {
            if path == subtree.as_path() || path.starts_with(subtree) {
                return true;
            }
        }
        for rule in &self.prefixes {
            if path.parent() == Some(rule.dir.as_path())
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&rule.prefix))
            {
                return true;
            }
        }
        false
    }
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
    pub fn new(mut packages: Vec<LockedPackage>) -> Self {
        for package in packages.iter_mut() {
            package.owned_subtrees.sort();
            package.owned_subtrees.dedup();
            package.owned_prefixes.sort_by(|left, right| {
                left.dir
                    .cmp(&right.dir)
                    .then(left.prefix.cmp(&right.prefix))
            });
            package.owned_prefixes.dedup();
            package.owned_files.sort();
            package.owned_files.dedup();
        }
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
        Self {
            version: LOCKFILE_VERSION,
            packages,
            legacy_managed_files: Vec::new(),
        }
    }

    pub fn read(path: &Path) -> Result<Self> {
        let lockfile = Self::read_unvalidated(path)?;
        lockfile.ensure_current_version(path)?;
        lockfile.validate_install_digests(path)?;
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

    /// Build the strict (v10) ownership view: per-package `owned_files`,
    /// `owned_subtrees`, `owned_prefixes`, plus the artifact-root expansion in
    /// [`Lockfile::managed_paths`] (which itself draws on `legacy_managed_files`
    /// today and is the bridge being torn out in Slice 3).
    ///
    /// Validates that:
    /// - Empty filename prefixes are rejected (use `owned_subtrees`).
    /// - Two packages cannot claim the same subtree root.
    /// - Two packages cannot claim the same `(dir, prefix)` rule.
    ///
    /// Redundant claims (a `prefix` rule whose `dir` is inside an owned
    /// `subtree`, or an `owned_files` entry inside an owned `subtree`) are
    /// reported via `eprintln!` and otherwise ignored.
    pub fn owned_set(&self, project_root: &Path) -> Result<OwnedSet> {
        let mut set = OwnedSet {
            exact: self.managed_paths(project_root)?,
            subtrees: Vec::new(),
            prefixes: Vec::new(),
        };

        // Track first claimant for each subtree/prefix rule so we can produce
        // helpful conflict messages when a second package collides.
        let mut subtree_owners: std::collections::HashMap<PathBuf, String> =
            std::collections::HashMap::new();
        let mut prefix_owners: std::collections::HashMap<(PathBuf, String), String> =
            std::collections::HashMap::new();

        for package in &self.packages {
            for relative in &package.owned_files {
                let validated = Self::validate_managed_relative(relative, project_root)?;
                set.exact.insert(project_root.join(validated));
            }
            for relative in &package.owned_subtrees {
                let validated = Self::validate_managed_relative(relative, project_root)?;
                let subtree = project_root.join(validated);
                if let Some(existing) = subtree_owners.get(&subtree) {
                    if existing != &package.alias {
                        bail!(
                            "packages `{}` and `{}` both declare ownership of subtree `{}`",
                            existing,
                            package.alias,
                            relative
                        );
                    }
                } else {
                    subtree_owners.insert(subtree.clone(), package.alias.clone());
                    set.subtrees.push(subtree);
                }
            }
            for rule in &package.owned_prefixes {
                if rule.prefix.is_empty() {
                    bail!(
                        "package `{}` declares an empty filename prefix for `{}`; use owned_subtrees instead",
                        package.alias,
                        rule.dir
                    );
                }
                let validated_dir = Self::validate_managed_relative(&rule.dir, project_root)?;
                let dir = project_root.join(validated_dir);
                let key = (dir.clone(), rule.prefix.clone());
                if let Some(existing) = prefix_owners.get(&key) {
                    if existing != &package.alias {
                        bail!(
                            "packages `{}` and `{}` both declare ownership of prefix `{}` in `{}`",
                            existing,
                            package.alias,
                            rule.prefix,
                            rule.dir
                        );
                    }
                } else {
                    prefix_owners.insert(key, package.alias.clone());
                    set.prefixes.push(OwnedPrefixPath {
                        dir,
                        prefix: rule.prefix.clone(),
                    });
                }
            }
        }

        // Stable ordering for downstream deletion / iteration.
        set.subtrees.sort();
        set.subtrees.dedup();
        set.prefixes
            .sort_by(|a, b| a.dir.cmp(&b.dir).then(a.prefix.cmp(&b.prefix)));
        set.prefixes.dedup();

        // Warn about redundant claims that the subtree rules already cover.
        // Slice 5's review can route these through Reporter; today eprintln! is
        // good enough so tests can capture the warning behavior loosely.
        for rule in &set.prefixes {
            if set
                .subtrees
                .iter()
                .any(|subtree| rule.dir.starts_with(subtree))
            {
                eprintln!(
                    "warning: filename-prefix rule `{}` in `{}` is redundant; covered by an owned subtree",
                    rule.prefix,
                    rule.dir.display()
                );
            }
        }
        for owned in &set.exact {
            if set
                .subtrees
                .iter()
                .any(|subtree| owned != subtree.as_path() && owned.starts_with(subtree))
            {
                eprintln!(
                    "warning: owned file `{}` is redundant; covered by an owned subtree",
                    owned.display()
                );
            }
        }

        Ok(set)
    }

    /// Like [`Lockfile::owned_set`] but additionally folds the legacy v9
    /// expansion into `exact` for pre-current-schema lockfiles, mirroring the
    /// existing [`Lockfile::managed_paths_for_sync`] posture.
    pub fn owned_set_for_sync(&self, project_root: &Path) -> Result<OwnedSet> {
        let mut set = self.owned_set(project_root)?;
        if !self.uses_current_schema() {
            set.exact.extend(self.managed_paths_for_sync(project_root)?);
        }
        Ok(set)
    }

    pub fn managed_paths_for_sync(&self, project_root: &Path) -> Result<HashSet<PathBuf>> {
        let mut managed_paths = self.managed_paths(project_root)?;
        if self.uses_current_schema() {
            return Ok(managed_paths);
        }

        for relative in &self.legacy_managed_files {
            let relative_path = Self::validate_managed_relative(relative, project_root)?;
            managed_paths.insert(project_root.join(relative_path));
            if let Some(paths) =
                self.expand_previous_schema_managed_root(project_root, relative_path)
            {
                managed_paths.extend(paths);
            }
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

        for relative in &self.legacy_managed_files {
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

    fn validate_install_digests(&self, path: &Path) -> Result<()> {
        for package in &self.packages {
            if let Some(value) = package.install_digest.as_deref()
                && !value.starts_with("blake3:")
            {
                bail!(
                    "package `{}` in {} has install_digest `{}`; expected `blake3:<hex>` prefix",
                    package.alias,
                    path.display(),
                    value
                );
            }
        }
        Ok(())
    }

    pub(crate) fn validate_managed_relative<'a>(
        relative: &'a str,
        project_root: &Path,
    ) -> Result<&'a Path> {
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
        let names = ManagedArtifactNames::from_locked_packages(self.packages.iter());
        expand_managed_root_with_names(&names, &self.packages, project_root, relative_path)
    }

    fn expand_previous_schema_managed_root(
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
                .or_else(|| package.digest.strip_prefix("blake3:"))
                .unwrap_or(&package.digest),
        ),
    }
}

fn expand_managed_root_with_names(
    names: &ManagedArtifactNames,
    packages: &[LockedPackage],
    project_root: &Path,
    relative_path: &Path,
) -> Option<Vec<PathBuf>> {
    let components = relative_path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    match components.as_slice() {
        [runtime, artifact_dir] => {
            let paths = match artifact_dir.as_str() {
                "skills" => packages
                    .iter()
                    .flat_map(|package| {
                        let mut paths = package
                            .skills
                            .iter()
                            .map(|artifact_id| {
                                project_root.join(format!(
                                    "{runtime}/skills/{}",
                                    names.locked_managed_skill_id(package, artifact_id)
                                ))
                            })
                            .collect::<Vec<_>>();
                        if runtime == ".codex" {
                            paths.extend(package.commands.iter().map(|command_id| {
                                project_root.join(format!(
                                    "{runtime}/skills/{}",
                                    crate::adapters::codex::synthetic_locked_command_skill_id(
                                        names, package, command_id,
                                    )
                                ))
                            }));
                        }
                        paths
                    })
                    .collect::<Vec<_>>(),
                "agents" if *runtime == ".github" => packages
                    .iter()
                    .flat_map(|package| {
                        package.agents.iter().map(|artifact_id| {
                            project_root.join(format!(
                                "{runtime}/agents/{}",
                                names.locked_managed_file_name(
                                    package,
                                    ArtifactKind::Agent,
                                    artifact_id,
                                    "agent.md",
                                )
                            ))
                        })
                    })
                    .collect::<Vec<_>>(),
                "agents" | "rules" | "commands" => {
                    let kind = match artifact_dir.as_str() {
                        "agents" => ArtifactKind::Agent,
                        "rules" => ArtifactKind::Rule,
                        "commands" => ArtifactKind::Command,
                        _ => return None,
                    };
                    packages
                        .iter()
                        .flat_map(|package| {
                            let ids: Box<dyn Iterator<Item = &String> + '_> = match kind {
                                ArtifactKind::Agent => Box::new(package.agents.iter()),
                                ArtifactKind::Rule => Box::new(package.rules.iter()),
                                ArtifactKind::Command => Box::new(package.commands.iter()),
                                ArtifactKind::Skill => Box::new(std::iter::empty()),
                            };
                            ids.map(|artifact_id| {
                                let extension = match (runtime.as_str(), kind) {
                                    (".codex", ArtifactKind::Agent) => "toml",
                                    (".cursor", ArtifactKind::Rule) => "mdc",
                                    _ => "md",
                                };
                                project_root.join(format!(
                                    "{runtime}/{artifact_dir}/{}",
                                    names.locked_managed_file_name(
                                        package,
                                        kind,
                                        artifact_id,
                                        extension,
                                    )
                                ))
                            })
                        })
                        .collect::<Vec<_>>()
                }
                _ => return None,
            };

            let paths = paths
                .into_iter()
                .collect::<HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            if paths.is_empty() { None } else { Some(paths) }
        }
        [runtime, artifact_dir, artifact_name] => {
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
                "skills" => packages
                    .iter()
                    .filter_map(|package| {
                        if package
                            .skills
                            .iter()
                            .any(|existing| existing == artifact_name)
                        {
                            return Some(project_root.join(format!(
                                "{runtime}/skills/{}",
                                names.locked_managed_skill_id(package, artifact_name)
                            )));
                        }
                        if runtime == ".codex"
                            && package.commands.iter().any(|command_id| {
                                crate::adapters::codex::synthetic_locked_command_skill_id(
                                    names, package, command_id,
                                ) == *artifact_name
                            })
                        {
                            return Some(
                                project_root.join(format!("{runtime}/skills/{artifact_name}")),
                            );
                        }
                        None
                    })
                    .collect::<Vec<_>>(),
                "agents" if runtime == ".github" => packages
                    .iter()
                    .filter(|package| {
                        package
                            .agents
                            .iter()
                            .any(|existing| existing == artifact_name)
                    })
                    .map(|package| {
                        project_root.join(format!(
                            "{runtime}/agents/{}",
                            names.locked_managed_file_name(
                                package,
                                ArtifactKind::Agent,
                                artifact_name,
                                "agent.md"
                            )
                        ))
                    })
                    .collect::<Vec<_>>(),
                "agents" | "rules" | "commands" => {
                    let (artifact_id, extension) =
                        split_managed_file_name(runtime.as_str(), artifact_dir, artifact_name)?;
                    let kind = match artifact_dir.as_str() {
                        "agents" => ArtifactKind::Agent,
                        "rules" => ArtifactKind::Rule,
                        "commands" => ArtifactKind::Command,
                        _ => return None,
                    };
                    packages
                        .iter()
                        .filter(|package| match kind {
                            ArtifactKind::Agent => package
                                .agents
                                .iter()
                                .any(|existing| existing == artifact_id),
                            ArtifactKind::Rule => {
                                package.rules.iter().any(|existing| existing == artifact_id)
                            }
                            ArtifactKind::Command => package
                                .commands
                                .iter()
                                .any(|existing| existing == artifact_id),
                            ArtifactKind::Skill => false,
                        })
                        .map(|package| {
                            project_root.join(format!(
                                "{runtime}/{artifact_dir}/{}",
                                names.locked_managed_file_name(
                                    package,
                                    kind,
                                    artifact_id,
                                    extension
                                )
                            ))
                        })
                        .collect::<Vec<_>>()
                }
                _ => return None,
            };

            if paths.is_empty() { None } else { Some(paths) }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    /// Build a Lockfile with the given packages and a legacy v9 managed_files
    /// list. v10 writes never populate `legacy_managed_files`, but the read
    /// path (and the inline tests that exercise `managed_paths*`) still need a
    /// way to feed the list in.
    fn lockfile_with_legacy_files(
        packages: Vec<LockedPackage>,
        legacy_managed_files: Vec<String>,
    ) -> Lockfile {
        let mut lockfile = Lockfile::new(packages);
        lockfile.legacy_managed_files = legacy_managed_files;
        lockfile.legacy_managed_files.sort();
        lockfile.legacy_managed_files.dedup();
        lockfile
    }

    #[test]
    fn round_trips_lockfile_as_toml() {
        let lockfile = Lockfile::new(vec![LockedPackage {
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
            owned_subtrees: vec![".claude/skills/review".into()],
            owned_prefixes: vec![OwnedPrefix {
                dir: ".claude/hooks".into(),
                prefix: "nodus-hook-".into(),
            }],
            owned_files: vec![".claude/agents/security-reviewer.md".into()],
            install_digest: Some("blake3:deadbeef".into()),
        }]);

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
                owned_subtrees: vec![],
                owned_prefixes: vec![],
                owned_files: vec![],
                install_digest: None,
            }],
            legacy_managed_files: vec![
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
    fn expands_logical_skill_roots_to_runtime_directories() {
        let lockfile = lockfile_with_legacy_files(
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
                owned_subtrees: vec![],
                owned_prefixes: vec![],
                owned_files: vec![],
                install_digest: None,
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

        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.agents/skills/iframe-ad")));
        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.claude/skills/iframe-ad")));
        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.codex/skills/iframe-ad")));
        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.github/skills/iframe-ad")));
        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.cursor/skills/iframe-ad")));
        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.opencode/skills/iframe-ad")));
    }

    #[test]
    fn expands_codex_skill_roots_to_include_synthetic_command_skills() {
        let lockfile = lockfile_with_legacy_files(
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
                selected_components: Some(vec![DependencyComponent::Commands]),
                skills: vec![],
                agents: vec![],
                rules: vec![],
                commands: vec!["build".into()],
                mcp_servers: vec![],
                dependencies: vec![],
                capabilities: vec![],
                owned_subtrees: vec![],
                owned_prefixes: vec![],
                owned_files: vec![],
                install_digest: None,
            }],
            vec![".codex/skills".into()],
        );

        let managed_paths = lockfile.managed_paths(Path::new("/tmp/project")).unwrap();

        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.codex/skills/__cmd_build")));
    }

    #[test]
    fn expands_compressed_runtime_artifact_roots() {
        let lockfile = lockfile_with_legacy_files(
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
                skills: vec!["review".into()],
                agents: vec!["security".into()],
                rules: vec!["default".into()],
                commands: vec!["build".into()],
                mcp_servers: vec![],
                dependencies: vec![],
                capabilities: vec![],
                owned_subtrees: vec![],
                owned_prefixes: vec![],
                owned_files: vec![],
                install_digest: None,
            }],
            vec![
                ".claude/skills".into(),
                ".claude/agents".into(),
                ".claude/rules".into(),
                ".opencode/commands".into(),
            ],
        );

        let managed_paths = lockfile.managed_paths(Path::new("/tmp/project")).unwrap();

        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.claude/skills/review")));
        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.claude/agents/security.md")));
        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.claude/rules/default.md")));
        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.opencode/commands/build.md")));
    }

    #[test]
    fn expands_logical_file_outputs_to_runtime_files() {
        let lockfile = lockfile_with_legacy_files(
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
                owned_subtrees: vec![],
                owned_prefixes: vec![],
                owned_files: vec![],
                install_digest: None,
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

        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.agents/commands/build.md")));
        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.claude/agents/security.md")));
        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.claude/commands/build.md")));
        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.claude/rules/default.md")));
        assert!(managed_paths.contains(&PathBuf::from(
            "/tmp/project/.github/agents/security.agent.md"
        )));
        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.cursor/commands/build.md")));
        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.cursor/rules/default.mdc")));
        assert!(
            managed_paths.contains(&PathBuf::from("/tmp/project/.opencode/agents/security.md"))
        );
        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.opencode/commands/build.md")));
        assert!(managed_paths.contains(&PathBuf::from("/tmp/project/.opencode/rules/default.md")));
    }

    #[test]
    fn keeps_direct_github_agent_files_exact_in_current_lockfiles() {
        let lockfile = lockfile_with_legacy_files(
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
                owned_subtrees: vec![],
                owned_prefixes: vec![],
                owned_files: vec![],
                install_digest: None,
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
                owned_subtrees: vec![],
                owned_prefixes: vec![],
                owned_files: vec![],
                install_digest: None,
            }],
            legacy_managed_files: vec![".github/agents/security.agent.md".into()],
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
                owned_subtrees: vec![],
                owned_prefixes: vec![],
                owned_files: vec![],
                install_digest: None,
            }],
            legacy_managed_files: vec![".mcp.json".into()],
        };

        assert_eq!(
            lockfile.managed_mcp_server_names(),
            HashSet::from([String::from("firebase__firebase")])
        );
    }

    #[test]
    fn rejects_install_digest_without_blake3_prefix() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let lockfile = Lockfile::new(vec![LockedPackage {
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
            agents: vec![],
            rules: vec![],
            commands: vec![],
            mcp_servers: vec![],
            dependencies: vec![],
            capabilities: vec![],
            owned_subtrees: vec![],
            owned_prefixes: vec![],
            owned_files: vec![],
            install_digest: Some("deadbeef".into()),
        }]);
        lockfile.write(&path).unwrap();

        let error = Lockfile::read(&path).unwrap_err().to_string();

        assert!(
            error.contains("blake3:"),
            "expected error to mention blake3 prefix, got: {error}"
        );
    }

    #[test]
    fn round_trips_owned_prefix() {
        let lockfile = Lockfile::new(vec![LockedPackage {
            alias: "hooks".into(),
            name: "hooks".into(),
            version_tag: Some("v0.1.0".into()),
            source: LockedSource {
                kind: "git".into(),
                path: None,
                url: Some("https://github.com/example/hooks".into()),
                tag: Some("v0.1.0".into()),
                branch: None,
                rev: Some("01f556abcdef".into()),
            },
            digest: "sha256:abc".into(),
            selected_components: None,
            skills: vec![],
            agents: vec![],
            rules: vec![],
            commands: vec![],
            mcp_servers: vec![],
            dependencies: vec![],
            capabilities: vec![],
            owned_subtrees: vec![],
            owned_prefixes: vec![OwnedPrefix {
                dir: ".claude/hooks".into(),
                prefix: "nodus-hook-".into(),
            }],
            owned_files: vec![],
            install_digest: None,
        }]);

        let encoded = toml::to_string_pretty(&lockfile).unwrap();
        let decoded: Lockfile = toml::from_str(&encoded).unwrap();

        assert_eq!(decoded, lockfile);
        assert_eq!(decoded.packages[0].owned_prefixes.len(), 1);
        assert_eq!(decoded.packages[0].owned_prefixes[0].dir, ".claude/hooks");
        assert_eq!(decoded.packages[0].owned_prefixes[0].prefix, "nodus-hook-");
    }

    fn minimal_package(alias: &str) -> LockedPackage {
        LockedPackage {
            alias: alias.into(),
            name: alias.into(),
            version_tag: None,
            source: LockedSource {
                kind: "path".into(),
                path: Some(".".into()),
                url: None,
                tag: None,
                branch: None,
                rev: None,
            },
            digest: "sha256:abc".into(),
            selected_components: None,
            skills: vec![],
            agents: vec![],
            rules: vec![],
            commands: vec![],
            mcp_servers: vec![],
            dependencies: vec![],
            capabilities: vec![],
            owned_subtrees: vec![],
            owned_prefixes: vec![],
            owned_files: vec![],
            install_digest: None,
        }
    }

    /// Regression guard: removing `.sort()` calls in `Lockfile::new` must turn
    /// this test red. Insertion order of packages and of per-package ownership
    /// vectors must not affect the serialized output.
    #[test]
    fn lockfile_new_sort_order_is_deterministic_across_input_reorderings() {
        let make_package = |alias: &str| {
            let mut pkg = minimal_package(alias);
            pkg.owned_subtrees = vec![
                ".nodus/packages/zzz".into(),
                ".nodus/packages/aaa".into(),
                ".nodus/packages/mmm".into(),
            ];
            pkg.owned_prefixes = vec![
                OwnedPrefix {
                    dir: ".codex/hooks".into(),
                    prefix: "nodus-".into(),
                },
                OwnedPrefix {
                    dir: ".claude/hooks".into(),
                    prefix: "nodus-".into(),
                },
            ];
            pkg.owned_files = vec!["opencode.json".into(), ".claude/settings.json".into()];
            pkg
        };

        let one = Lockfile::new(vec![make_package("beta"), make_package("alpha")]);
        let two = Lockfile::new(vec![make_package("alpha"), make_package("beta")]);

        let encoded_one = toml::to_string_pretty(&one).unwrap();
        let encoded_two = toml::to_string_pretty(&two).unwrap();

        assert_eq!(
            encoded_one, encoded_two,
            "Lockfile::new must produce byte-identical TOML regardless of input order"
        );

        // Spot-check each package's vectors are sorted.
        for package in &one.packages {
            let mut expected_subtrees = package.owned_subtrees.clone();
            expected_subtrees.sort();
            assert_eq!(package.owned_subtrees, expected_subtrees);

            let mut expected_files = package.owned_files.clone();
            expected_files.sort();
            assert_eq!(package.owned_files, expected_files);

            let mut expected_prefixes = package.owned_prefixes.clone();
            expected_prefixes.sort_by(|left, right| {
                left.dir
                    .cmp(&right.dir)
                    .then(left.prefix.cmp(&right.prefix))
            });
            assert_eq!(package.owned_prefixes, expected_prefixes);
        }
    }

    /// Pins the `#[serde(skip_serializing_if = "Vec::is_empty")]` and
    /// `Option::is_none` posture on v10 outputs. An empty v10 lockfile must
    /// not emit `managed_files = []`, nor empty `owned_*` fields, nor a
    /// `null`-style `install_digest`.
    #[test]
    fn serialized_v10_lockfile_omits_empty_managed_files_and_owned_fields() {
        let lockfile = Lockfile::new(vec![minimal_package("solo")]);
        let encoded = toml::to_string_pretty(&lockfile).unwrap();

        assert!(
            !encoded.contains("managed_files"),
            "v10 writes must skip the legacy managed_files field on empty; got:\n{encoded}"
        );
        assert!(
            !encoded.contains("owned_subtrees"),
            "owned_subtrees must skip on empty; got:\n{encoded}"
        );
        assert!(
            !encoded.contains("owned_prefixes"),
            "owned_prefixes must skip on empty; got:\n{encoded}"
        );
        assert!(
            !encoded.contains("owned_files"),
            "owned_files must skip on empty; got:\n{encoded}"
        );
        assert!(
            !encoded.contains("install_digest"),
            "install_digest must skip on None; got:\n{encoded}"
        );
        assert!(
            encoded.contains("version = 10"),
            "v10 marker must be present; got:\n{encoded}"
        );
    }

    #[test]
    fn owned_set_collects_owned_files_from_all_packages() {
        let mut alpha = minimal_package("alpha");
        alpha.owned_files = vec![".claude/agents/security.md".into()];
        let mut beta = minimal_package("beta");
        beta.owned_files = vec![".codex/rules/default.md".into()];
        let lockfile = Lockfile::new(vec![alpha, beta]);

        let owned = lockfile.owned_set(Path::new("/tmp/project")).unwrap();

        assert!(
            owned
                .exact
                .contains(&PathBuf::from("/tmp/project/.claude/agents/security.md"))
        );
        assert!(
            owned
                .exact
                .contains(&PathBuf::from("/tmp/project/.codex/rules/default.md"))
        );
    }

    #[test]
    fn owned_set_collects_subtrees_and_supports_starts_with_match() {
        let mut pkg = minimal_package("foo");
        pkg.owned_subtrees = vec![".nodus/packages/foo".into()];
        let lockfile = Lockfile::new(vec![pkg]);

        let owned = lockfile.owned_set(Path::new("/tmp/project")).unwrap();

        assert!(owned.contains(Path::new("/tmp/project/.nodus/packages/foo/bar.rs")));
        assert!(owned.contains(Path::new("/tmp/project/.nodus/packages/foo")));
        assert!(!owned.contains(Path::new("/tmp/project/.nodus/packages/bar/x.rs")));
    }

    #[test]
    fn owned_set_filename_prefix_match_is_direct_children_only() {
        let mut pkg = minimal_package("hooks");
        pkg.owned_prefixes = vec![OwnedPrefix {
            dir: ".claude/hooks".into(),
            prefix: "nodus-hook-".into(),
        }];
        let lockfile = Lockfile::new(vec![pkg]);

        let owned = lockfile.owned_set(Path::new("/tmp/project")).unwrap();

        assert!(owned.contains(Path::new("/tmp/project/.claude/hooks/nodus-hook-foo.sh")));
        assert!(!owned.contains(Path::new("/tmp/project/.claude/hooks/user-thing.sh")));
        assert!(!owned.contains(Path::new(
            "/tmp/project/.claude/hooks/subdir/nodus-hook-foo.sh"
        )));
    }

    #[test]
    fn owned_set_rejects_empty_prefix() {
        let mut pkg = minimal_package("bad");
        pkg.owned_prefixes = vec![OwnedPrefix {
            dir: ".x".into(),
            prefix: String::new(),
        }];
        let lockfile = Lockfile::new(vec![pkg]);

        let error = lockfile
            .owned_set(Path::new("/tmp/project"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("empty filename prefix"),
            "expected error about empty prefix, got: {error}"
        );
    }

    #[test]
    fn owned_set_rejects_overlapping_subtree_claims() {
        let mut alpha = minimal_package("alpha");
        alpha.owned_subtrees = vec![".shared/dir".into()];
        let mut beta = minimal_package("beta");
        beta.owned_subtrees = vec![".shared/dir".into()];
        let lockfile = Lockfile::new(vec![alpha, beta]);

        let error = lockfile
            .owned_set(Path::new("/tmp/project"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("alpha") && error.contains("beta"),
            "expected error to mention both aliases, got: {error}"
        );
        assert!(
            error.contains(".shared/dir"),
            "expected error to mention the conflicting subtree, got: {error}"
        );
    }

    /// The root sentinel must survive a full serialize/deserialize cycle as
    /// the literal `"<root>"` string. Catches regressions where someone
    /// accidentally maps the sentinel to `None` or an alternate spelling.
    #[test]
    fn root_package_with_sentinel_name_round_trips() {
        let mut root = minimal_package("root");
        root.name = ROOT_PACKAGE_NAME_SENTINEL.to_string();
        let lockfile = Lockfile::new(vec![root]);

        let encoded = toml::to_string_pretty(&lockfile).unwrap();
        assert!(
            encoded.contains(r#"name = "<root>""#),
            "expected sentinel literal in TOML; got:\n{encoded}"
        );

        let decoded: Lockfile = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.packages.len(), 1);
        assert_eq!(decoded.packages[0].name, ROOT_PACKAGE_NAME_SENTINEL);
    }
}
