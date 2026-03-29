use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::git::{ensure_git_dependency, normalize_alias_from_url};
use crate::manifest::{
    DependencyComponent, DependencySourceKind, DependencySpec, LoadedManifest, PackageRole,
    load_dependency_from_dir, load_root_from_dir,
};
use crate::report::Reporter;

#[derive(Debug, Clone)]
pub(crate) enum ResolvedInspectionSource {
    Path {
        declared_path: Option<PathBuf>,
        resolved_root: PathBuf,
        tag: Option<String>,
    },
    Git {
        url: String,
        subpath: Option<PathBuf>,
        tag: Option<String>,
        branch: Option<String>,
        rev: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedInspectionTarget {
    pub(crate) alias: String,
    pub(crate) manifest: LoadedManifest,
    pub(crate) source: ResolvedInspectionSource,
    pub(crate) enabled: bool,
    pub(crate) selected_components: Option<Vec<DependencyComponent>>,
    pub(crate) selected_workspace_members: Option<Vec<String>>,
    pub(crate) version_requirement: Option<String>,
    pub(crate) role: PackageRole,
}

pub(crate) fn resolve_direct_dependency(
    cwd: &Path,
    package: &str,
) -> Result<Option<(String, DependencySpec, LoadedManifest)>> {
    let root_manifest = load_root_from_dir(cwd)?;
    if let Some(entry) = root_manifest.manifest.get_dependency(package) {
        return Ok(Some((
            package.to_string(),
            entry.spec.clone(),
            root_manifest,
        )));
    }

    let normalized = match normalize_alias_from_url(package) {
        Ok(alias) => alias,
        Err(_) => return Ok(None),
    };
    let Some(entry) = root_manifest.manifest.get_dependency(&normalized) else {
        return Ok(None);
    };
    Ok(Some((normalized, entry.spec.clone(), root_manifest)))
}

pub(crate) fn resolve_local_package_path(cwd: &Path, package: &str) -> Result<Option<PathBuf>> {
    let candidate = Path::new(package);
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    };
    if !candidate.exists() {
        return Ok(None);
    }

    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("failed to access {}", candidate.display()))?;
    if !canonical.is_dir() {
        bail!("package path {} must be a directory", canonical.display());
    }
    Ok(Some(canonical))
}

pub(crate) fn load_manifest_for_inspection(root: &Path) -> Result<(LoadedManifest, PackageRole)> {
    match load_root_from_dir(root) {
        Ok(manifest) => Ok((manifest, PackageRole::Root)),
        Err(_) => {
            load_dependency_from_dir(root).map(|manifest| (manifest, PackageRole::Dependency))
        }
    }
}

pub(crate) fn resolve_inspection_target(
    alias: &str,
    dependency: &DependencySpec,
    root_manifest: &LoadedManifest,
    cache_root: &Path,
    reporter: &Reporter,
) -> Result<ResolvedInspectionTarget> {
    match dependency.source_kind()? {
        DependencySourceKind::Path => {
            let declared_path = dependency
                .path
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("dependency `{alias}` must declare `path`"))?;
            let package_root = root_manifest.resolve_path(declared_path)?;
            let manifest = load_dependency_from_dir(&package_root)?;
            Ok(ResolvedInspectionTarget {
                alias: alias.to_string(),
                manifest,
                source: ResolvedInspectionSource::Path {
                    declared_path: Some(declared_path.clone()),
                    resolved_root: package_root,
                    tag: dependency.tag.clone(),
                },
                enabled: dependency.is_enabled(),
                selected_components: dependency.effective_selected_components(),
                selected_workspace_members: dependency.explicit_members_sorted(),
                version_requirement: dependency.version.as_ref().map(ToString::to_string),
                role: PackageRole::Dependency,
            })
        }
        DependencySourceKind::Git => {
            let url = dependency.resolved_git_url()?;
            let checkout = ensure_git_dependency(
                cache_root,
                &url,
                dependency.requested_git_ref_or_none()?,
                true,
                reporter,
            )?;
            let canonical_checkout = checkout.path.canonicalize().with_context(|| {
                format!(
                    "failed to canonicalize dependency `{alias}` checkout {}",
                    checkout.path.display()
                )
            })?;
            let package_root = if let Some(subpath) = dependency.subpath.as_deref() {
                let path = canonical_checkout.join(subpath);
                let canonical = path.canonicalize().with_context(|| {
                    format!(
                        "failed to resolve dependency `{alias}` subpath {}",
                        path.display()
                    )
                })?;
                if !canonical.starts_with(&canonical_checkout) {
                    bail!(
                        "dependency `{alias}` subpath `{}` escapes the git checkout {}",
                        subpath.display(),
                        canonical_checkout.display()
                    );
                }
                canonical
            } else {
                checkout.path.clone()
            };
            let manifest = load_dependency_from_dir(&package_root).with_context(|| {
                format!("dependency `{alias}` does not match the Nodus package layout")
            })?;
            Ok(ResolvedInspectionTarget {
                alias: alias.to_string(),
                manifest,
                source: ResolvedInspectionSource::Git {
                    url: checkout.url,
                    subpath: dependency.subpath.clone(),
                    tag: checkout.tag,
                    branch: checkout.branch,
                    rev: checkout.rev,
                },
                enabled: dependency.is_enabled(),
                selected_components: dependency.effective_selected_components(),
                selected_workspace_members: dependency.explicit_members_sorted(),
                version_requirement: dependency.version.as_ref().map(ToString::to_string),
                role: PackageRole::Dependency,
            })
        }
    }
}
