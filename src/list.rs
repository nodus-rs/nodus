use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::lockfile::{LOCKFILE_NAME, LockedPackage, Lockfile};
use crate::manifest::{
    DependencyComponent, DependencyKind, DependencySourceKind, RequestedGitRef, load_root_from_dir,
};
use crate::paths::display_path;
use crate::report::Reporter;

#[derive(Debug, Clone, Serialize)]
pub struct DependencyList {
    pub dependencies: Vec<DependencyListEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DependencyListEntry {
    pub alias: String,
    pub kind: DependencyKind,
    #[serde(skip_serializing_if = "is_true")]
    pub enabled: bool,
    pub source: DependencyListSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_ref: Option<DependencyListRequestedRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_components: Option<Vec<DependencyComponent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked: Option<DependencyListLocked>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum DependencyListSource {
    Path { path: String },
    Git { url: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct DependencyListRequestedRef {
    pub kind: &'static str,
    pub value: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DependencyListLocked {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_tag: Option<String>,
    pub digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
}

pub fn list_dependencies_in_dir(cwd: &Path, reporter: &Reporter) -> Result<()> {
    let list = list_dependencies_json_in_dir(cwd)?;
    if list.dependencies.is_empty() {
        reporter.note("no dependencies configured")?;
        return Ok(());
    }

    for dependency in &list.dependencies {
        reporter.line(render_dependency_line(dependency))?;
    }
    Ok(())
}

pub fn list_dependencies_json_in_dir(cwd: &Path) -> Result<DependencyList> {
    let root = load_root_from_dir(cwd)?;
    let lockfile = load_lockfile(cwd)?;
    let mut dependencies = root
        .manifest
        .all_dependency_entries()
        .into_iter()
        .map(|entry| {
            let alias = entry.alias;
            let spec = entry.spec;
            let source = match spec.source_kind()? {
                DependencySourceKind::Path => DependencyListSource::Path {
                    path: display_path(spec.path.as_deref().ok_or_else(|| {
                        anyhow::anyhow!("dependency `{alias}` must declare `path`")
                    })?),
                },
                DependencySourceKind::Git => DependencyListSource::Git {
                    url: spec.resolved_git_url()?,
                },
            };
            let requested_ref = match spec.source_kind()? {
                DependencySourceKind::Path => None,
                DependencySourceKind::Git => Some(match spec.requested_git_ref()? {
                    RequestedGitRef::Tag(tag) => DependencyListRequestedRef {
                        kind: "tag",
                        value: tag.to_string(),
                    },
                    RequestedGitRef::Branch(branch) => DependencyListRequestedRef {
                        kind: "branch",
                        value: branch.to_string(),
                    },
                    RequestedGitRef::Revision(revision) => DependencyListRequestedRef {
                        kind: "revision",
                        value: revision.to_string(),
                    },
                    RequestedGitRef::VersionReq(version) => DependencyListRequestedRef {
                        kind: "version",
                        value: version.to_string(),
                    },
                }),
            };

            Ok(DependencyListEntry {
                alias: alias.to_string(),
                kind: entry.kind,
                enabled: spec.is_enabled(),
                source,
                requested_ref,
                selected_components: spec.effective_selected_components(),
                locked: lockfile
                    .as_ref()
                    .and_then(|lockfile| find_locked_package(lockfile, alias))
                    .map(DependencyListLocked::from),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    dependencies.sort_by(|left, right| left.alias.cmp(&right.alias));

    Ok(DependencyList { dependencies })
}

impl From<&LockedPackage> for DependencyListLocked {
    fn from(value: &LockedPackage) -> Self {
        Self {
            version_tag: value.version_tag.clone(),
            digest: value.digest.clone(),
            rev: value.source.rev.clone(),
        }
    }
}

fn load_lockfile(cwd: &Path) -> Result<Option<Lockfile>> {
    let path = cwd.join(LOCKFILE_NAME);
    if path.exists() {
        Ok(Some(Lockfile::read(&path)?))
    } else {
        Ok(None)
    }
}

fn find_locked_package<'a>(lockfile: &'a Lockfile, alias: &str) -> Option<&'a LockedPackage> {
    lockfile
        .packages
        .iter()
        .find(|package| package.alias == alias)
}

fn render_dependency_line(dependency: &DependencyListEntry) -> String {
    format!(
        "{:<20} {}",
        display_alias(dependency),
        dependency_summary(dependency)
    )
}

fn display_alias(dependency: &DependencyListEntry) -> String {
    if dependency.kind.is_dev() {
        format!("{} [dev]", dependency.alias)
    } else {
        dependency.alias.clone()
    }
}

fn dependency_summary(dependency: &DependencyListEntry) -> String {
    let mut parts = Vec::new();
    if !dependency.enabled {
        parts.push("disabled".to_string());
    }
    parts.push(match &dependency.source {
        DependencyListSource::Path { path } => format!("path {path}"),
        DependencyListSource::Git { url } => format!("git {url}"),
    });
    if let Some(requested_ref) = &dependency.requested_ref {
        parts.push(format!("{} {}", requested_ref.kind, requested_ref.value));
    }
    parts.push(format!(
        "components {}",
        render_components(dependency.selected_components.as_deref())
    ));
    parts.push(render_locked_summary(dependency.locked.as_ref()));
    parts.join("; ")
}

fn render_components(components: Option<&[DependencyComponent]>) -> String {
    match components {
        Some(components) => components
            .iter()
            .map(|component| component.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        None => "all".into(),
    }
}

fn render_locked_summary(locked: Option<&DependencyListLocked>) -> String {
    match locked {
        Some(locked) => {
            if let Some(rev) = &locked.rev {
                format!("locked rev {}", short_value(rev))
            } else {
                format!("locked digest {}", short_value(&locked.digest))
            }
        }
        None => "unlocked".into(),
    }
}

fn short_value(value: &str) -> String {
    value.chars().take(12).collect()
}

fn is_true(value: &bool) -> bool {
    *value
}
