use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use sha2::{Digest, Sha256};

use super::{
    PackageSource, Resolution, ResolveMode, ResolvedManagedFile, ResolvedManagedPath,
    ResolvedPackage,
};
use crate::git::{
    current_rev, ensure_git_dependency, ensure_git_dependency_at_rev, shared_checkout_path,
    shared_repository_path, validate_shared_checkout,
};
use crate::lockfile::{LOCKFILE_NAME, LockedSource, Lockfile};
use crate::manifest::{
    DependencyComponent, DependencySourceKind, DependencySpec, LoadedManifest, PackageRole,
    RequestedGitRef, load_dependency_from_dir, load_root_from_dir,
};
use crate::paths::display_path;
use crate::report::Reporter;

#[derive(Debug, Default)]
struct ResolverState {
    stack: Vec<PathBuf>,
    resolved_by_path: HashMap<PathBuf, ResolvedPackage>,
}

#[derive(Clone, Copy)]
struct ResolveContext<'a> {
    cache_root: &'a Path,
    mode: ResolveMode,
    frozen_lockfile: Option<&'a Lockfile>,
    root_override: Option<&'a LoadedManifest>,
    reporter: &'a Reporter,
}

struct ResolvePackageInput {
    alias: String,
    package_root: PathBuf,
    source: PackageSource,
    role: PackageRole,
    selected_components: Option<Vec<DependencyComponent>>,
    direct_managed_paths: Vec<ResolvedManagedPath>,
    extra_package_files: Vec<PathBuf>,
}

pub(super) fn validate_git_package(package: &ResolvedPackage, cache_root: &Path) -> Result<()> {
    let PackageSource::Git { url, rev, .. } = &package.source else {
        return Ok(());
    };

    let checkout_path = shared_checkout_path(cache_root, url, rev)?;
    if package.root != checkout_path {
        bail!(
            "git dependency `{}` resolved to {} instead of shared checkout {}",
            package.alias,
            package.root.display(),
            checkout_path.display()
        );
    }
    let current = current_rev(&package.root)?;
    if current.trim() != rev {
        bail!(
            "git dependency `{}` is checked out at {} instead of {}",
            package.alias,
            current.trim(),
            rev
        );
    }

    let mirror_path = shared_repository_path(cache_root, url)?;
    validate_shared_checkout(&package.root, &mirror_path, url)
}

pub(super) fn resolve_project(
    root: &Path,
    cache_root: &Path,
    mode: ResolveMode,
    reporter: &Reporter,
    frozen_lockfile: Option<&Lockfile>,
    root_override: Option<&LoadedManifest>,
) -> Result<Resolution> {
    let project_root = root
        .canonicalize()
        .with_context(|| format!("failed to access {}", root.display()))?;
    let context = ResolveContext {
        cache_root,
        mode,
        frozen_lockfile,
        root_override,
        reporter,
    };
    let mut state = ResolverState::default();
    resolve_package(
        &context,
        ResolvePackageInput {
            alias: "root".to_string(),
            package_root: project_root.clone(),
            source: PackageSource::Root,
            role: PackageRole::Root,
            selected_components: None,
            direct_managed_paths: Vec::new(),
            extra_package_files: Vec::new(),
        },
        &mut state,
    )?;

    let mut packages: Vec<_> = state.resolved_by_path.into_values().collect();
    packages.sort_by(|left, right| {
        left.alias
            .cmp(&right.alias)
            .then(left.root.cmp(&right.root))
    });

    let warnings = packages
        .iter()
        .flat_map(|package| package.manifest.warnings.iter().cloned())
        .collect();

    Ok(Resolution {
        project_root,
        packages,
        warnings,
    })
}

fn resolve_package(
    context: &ResolveContext<'_>,
    input: ResolvePackageInput,
    state: &mut ResolverState,
) -> Result<ResolvedPackage> {
    let ResolvePackageInput {
        alias,
        package_root,
        source,
        role,
        selected_components,
        direct_managed_paths,
        extra_package_files,
    } = input;
    if let Some(existing) = state.resolved_by_path.get_mut(&package_root) {
        existing.selected_components =
            union_selected_components(existing.selected_components.clone(), selected_components);
        if !direct_managed_paths.is_empty() {
            existing.direct_managed_paths = merge_direct_managed_paths(
                &package_root,
                &existing.direct_managed_paths,
                &direct_managed_paths,
            )?;
            merge_extra_package_files(&mut existing.extra_package_files, &extra_package_files);
            existing.digest =
                compute_package_digest(&existing.manifest, &existing.extra_package_files)?;
        }
        return Ok(existing.clone());
    }

    if state.stack.iter().any(|path| path == &package_root) {
        let cycle = state
            .stack
            .iter()
            .chain(std::iter::once(&package_root))
            .map(|path| display_path(path))
            .collect::<Vec<_>>()
            .join(" -> ");
        bail!("dependency cycle detected: {cycle}");
    }

    state.stack.push(package_root.clone());

    let manifest = match role {
        PackageRole::Root => {
            if let Some(root_override) = context.root_override {
                root_override.clone()
            } else {
                load_root_from_dir(&package_root)?
            }
        }
        PackageRole::Dependency => load_dependency_from_dir(&package_root)?,
    };

    let dependencies = manifest
        .manifest
        .active_dependency_entries_for_role(role)
        .into_iter()
        .map(|entry| resolve_dependency(&manifest, role, entry.alias, entry.spec, context, state))
        .collect::<Result<Vec<_>>>()?;

    let digest = compute_package_digest(&manifest, &extra_package_files)?;
    let resolved = ResolvedPackage {
        alias,
        root: package_root.clone(),
        manifest,
        source,
        digest,
        selected_components,
        direct_managed_paths,
        extra_package_files,
    };
    state
        .resolved_by_path
        .insert(package_root.clone(), resolved.clone());
    state.stack.pop();

    drop(dependencies);

    Ok(resolved)
}

fn resolve_dependency(
    parent: &LoadedManifest,
    parent_role: PackageRole,
    alias: &str,
    dependency: &DependencySpec,
    context: &ResolveContext<'_>,
    state: &mut ResolverState,
) -> Result<ResolvedPackage> {
    match dependency.source_kind()? {
        DependencySourceKind::Path => {
            let declared_path = dependency
                .path
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("dependency `{alias}` must declare `path`"))?;
            let dependency_root = parent
                .resolve_path(declared_path)
                .with_context(|| format!("failed to resolve dependency `{alias}`"))?;
            let source = PackageSource::Path {
                path: declared_path.clone(),
                tag: dependency.tag.clone(),
            };
            let (direct_managed_paths, extra_package_files) =
                resolve_direct_managed_paths(parent_role, alias, dependency, &dependency_root)?;
            resolve_package(
                context,
                ResolvePackageInput {
                    alias: alias.to_string(),
                    package_root: dependency_root,
                    source,
                    role: PackageRole::Dependency,
                    selected_components: dependency.effective_selected_components(),
                    direct_managed_paths,
                    extra_package_files,
                },
                state,
            )
        }
        DependencySourceKind::Git => {
            let url = dependency.resolved_git_url()?;
            let requested_ref = dependency.requested_git_ref()?;
            let checkout = if let Some(lockfile) = context.frozen_lockfile {
                let locked = locked_git_source(lockfile, alias, &url, requested_ref)?;
                let rev = locked.rev.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "dependency `{alias}` in {} does not record a git revision",
                        LOCKFILE_NAME
                    )
                })?;
                ensure_git_dependency_at_rev(
                    context.cache_root,
                    &url,
                    locked.tag.as_deref(),
                    locked.branch.as_deref(),
                    rev,
                    context.mode == ResolveMode::Sync,
                    context.reporter,
                )?
            } else {
                ensure_git_dependency(
                    context.cache_root,
                    &url,
                    Some(requested_ref),
                    context.mode == ResolveMode::Sync,
                    context.reporter,
                )?
            };
            let source = PackageSource::Git {
                url: checkout.url,
                tag: checkout.tag,
                branch: checkout.branch,
                rev: checkout.rev,
            };
            let (direct_managed_paths, extra_package_files) =
                resolve_direct_managed_paths(parent_role, alias, dependency, &checkout.path)?;
            resolve_package(
                context,
                ResolvePackageInput {
                    alias: alias.to_string(),
                    package_root: checkout.path,
                    source,
                    role: PackageRole::Dependency,
                    selected_components: dependency.effective_selected_components(),
                    direct_managed_paths,
                    extra_package_files,
                },
                state,
            )
        }
    }
}

fn locked_git_source<'a>(
    lockfile: &'a Lockfile,
    alias: &str,
    url: &str,
    requested_ref: RequestedGitRef<'_>,
) -> Result<&'a LockedSource> {
    let matches_requested_ref = |source: &LockedSource| match requested_ref {
        RequestedGitRef::Tag(tag) => source.tag.as_deref() == Some(tag) && source.branch.is_none(),
        RequestedGitRef::Branch(branch) => {
            source.branch.as_deref() == Some(branch) && source.tag.is_none()
        }
        RequestedGitRef::Revision(revision) => {
            source.rev.as_deref() == Some(revision)
                && source.tag.is_none()
                && source.branch.is_none()
        }
        RequestedGitRef::VersionReq(requirement) => {
            source
                .tag
                .as_deref()
                .and_then(crate::git::parse_semver_tag)
                .is_some_and(|version| requirement.matches(&version))
                && source.branch.is_none()
        }
    };

    let mut matching_sources = lockfile
        .packages
        .iter()
        .filter(|package| {
            package.source.kind == "git"
                && package.source.url.as_deref() == Some(url)
                && matches_requested_ref(&package.source)
        })
        .collect::<Vec<_>>();

    if matching_sources.is_empty() {
        bail!(
            "dependency `{alias}` is missing from {}; run `nodus sync` without `--frozen` to regenerate it",
            LOCKFILE_NAME
        );
    }

    if matching_sources.len() > 1 {
        let alias_matches = matching_sources
            .iter()
            .copied()
            .filter(|package| package.alias == alias)
            .collect::<Vec<_>>();
        matching_sources = if alias_matches.is_empty() {
            matching_sources
        } else {
            alias_matches
        };
    }

    if matching_sources.len() != 1 {
        bail!(
            "dependency `{alias}` has ambiguous git entries in {}; run `nodus sync` without `--frozen` to regenerate it",
            LOCKFILE_NAME
        );
    }

    Ok(&matching_sources[0].source)
}

fn union_selected_components(
    left: Option<Vec<DependencyComponent>>,
    right: Option<Vec<DependencyComponent>>,
) -> Option<Vec<DependencyComponent>> {
    match (left, right) {
        (None, _) | (_, None) => None,
        (Some(mut left), Some(right)) => {
            left.extend(right);
            left.sort();
            left.dedup();
            Some(left)
        }
    }
}

fn resolve_direct_managed_paths(
    parent_role: PackageRole,
    alias: &str,
    dependency: &DependencySpec,
    dependency_root: &Path,
) -> Result<(Vec<ResolvedManagedPath>, Vec<PathBuf>)> {
    if dependency.managed_mappings().is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    if parent_role != PackageRole::Root {
        bail!(
            "dependency `{alias}` field `managed` is supported only for direct dependencies in the root manifest"
        );
    }

    let mut ownership_roots = Vec::<PathBuf>::new();
    let mut concrete_targets = std::collections::HashSet::<PathBuf>::new();
    let mut mappings = Vec::new();
    let mut extra_package_files = Vec::new();

    for spec in dependency.managed_mappings() {
        let source_root = spec.normalized_source()?;
        let target_root = spec.normalized_target()?;
        validate_managed_ownership_root(alias, &ownership_roots, &target_root)?;

        let source_path =
            resolve_dependency_managed_source_path(alias, dependency_root, &source_root)?;
        let metadata = fs::metadata(&source_path)
            .with_context(|| format!("failed to read managed source {}", source_path.display()))?;
        let files = if metadata.is_file() {
            if !concrete_targets.insert(target_root.clone()) {
                bail!(
                    "dependency `{alias}` field `managed` maps multiple sources into {}",
                    target_root.display()
                );
            }
            extra_package_files.push(source_path);
            vec![ResolvedManagedFile {
                source_relative: source_root.clone(),
                target_relative: target_root.clone(),
            }]
        } else if metadata.is_dir() {
            let mut files = Vec::new();
            for entry in walkdir::WalkDir::new(&source_path) {
                let entry = entry?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let relative = entry.path().strip_prefix(&source_path).with_context(|| {
                    format!("failed to make {} relative", entry.path().display())
                })?;
                let source_relative = source_root.join(relative);
                let target_relative = target_root.join(relative);
                if !concrete_targets.insert(target_relative.clone()) {
                    bail!(
                        "dependency `{alias}` field `managed` maps multiple sources into {}",
                        target_relative.display()
                    );
                }
                extra_package_files.push(entry.path().canonicalize().with_context(|| {
                    format!("failed to canonicalize {}", entry.path().display())
                })?);
                files.push(ResolvedManagedFile {
                    source_relative,
                    target_relative,
                });
            }
            files.sort();
            files
        } else {
            bail!(
                "dependency `{alias}` managed source {} must be a file or directory",
                source_root.display()
            );
        };

        ownership_roots.push(target_root.clone());
        mappings.push(ResolvedManagedPath {
            source_root,
            target_root: target_root.clone(),
            ownership_root: target_root,
            files,
        });
    }

    extra_package_files.sort();
    extra_package_files.dedup();
    Ok((mappings, extra_package_files))
}

fn validate_managed_ownership_root(
    alias: &str,
    existing_roots: &[PathBuf],
    candidate: &Path,
) -> Result<()> {
    if let Some(existing) = existing_roots.iter().find(|existing| {
        existing.as_path().starts_with(candidate) || candidate.starts_with(existing)
    }) {
        bail!(
            "dependency `{alias}` field `managed` has overlapping target roots `{}` and `{}`",
            existing.display(),
            candidate.display()
        );
    }
    Ok(())
}

fn resolve_dependency_managed_source_path(
    alias: &str,
    dependency_root: &Path,
    source_root: &Path,
) -> Result<PathBuf> {
    let canonical_dependency_root = dependency_root
        .canonicalize()
        .with_context(|| format!("failed to access {}", dependency_root.display()))?;
    let source_path = dependency_root.join(source_root);
    let canonical = source_path
        .canonicalize()
        .with_context(|| format!("missing managed source {}", source_path.display()))?;
    if !canonical.starts_with(&canonical_dependency_root) {
        bail!(
            "dependency `{alias}` managed source {} escapes the dependency root {}",
            source_root.display(),
            canonical_dependency_root.display()
        );
    }
    Ok(canonical)
}

fn merge_direct_managed_paths(
    package_root: &Path,
    existing: &[ResolvedManagedPath],
    incoming: &[ResolvedManagedPath],
) -> Result<Vec<ResolvedManagedPath>> {
    let mut merged = existing.to_vec();

    for path in incoming {
        if merged.contains(path) {
            continue;
        }

        if let Some(conflict) = merged.iter().find(|existing| {
            existing.ownership_root.starts_with(&path.ownership_root)
                || path.ownership_root.starts_with(&existing.ownership_root)
        }) {
            bail!(
                "direct-managed targets for {} overlap at `{}` and `{}`",
                package_root.display(),
                conflict.ownership_root.display(),
                path.ownership_root.display()
            );
        }

        let existing_targets = merged
            .iter()
            .flat_map(|mapping| mapping.files.iter().map(|file| &file.target_relative))
            .collect::<std::collections::HashSet<_>>();
        if let Some(conflict) = path
            .files
            .iter()
            .find(|file| existing_targets.contains(&file.target_relative))
        {
            bail!(
                "direct-managed targets for {} overlap at `{}`",
                package_root.display(),
                conflict.target_relative.display()
            );
        }

        merged.push(path.clone());
    }

    Ok(merged)
}

fn merge_extra_package_files(target: &mut Vec<PathBuf>, extra_files: &[PathBuf]) {
    target.extend(extra_files.iter().cloned());
    target.sort();
    target.dedup();
}

fn compute_package_digest(
    manifest: &LoadedManifest,
    extra_package_files: &[PathBuf],
) -> Result<String> {
    let mut files = manifest.package_files()?;
    files.extend(extra_package_files.iter().cloned());
    files.sort();
    files.dedup();

    let file_payloads = files
        .par_iter()
        .map(|file| {
            let relative = file
                .strip_prefix(&manifest.root)
                .with_context(|| format!("failed to make {} relative", file.display()))?
                .to_path_buf();
            let contents = manifest
                .read_package_file(file)
                .with_context(|| format!("failed to read {} for hashing", file.display()))?;
            Ok((relative, contents))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<Result<Vec<_>>>()?;

    let mut hasher = Sha256::new();
    for (relative, contents) in file_payloads {
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(contents);
        hasher.update([0xff]);
    }

    Ok(format!("sha256:{:x}", hasher.finalize()))
}
