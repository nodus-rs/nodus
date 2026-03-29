use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use sha2::{Digest, Sha256};

use super::{
    ManagedMappingMigration, PackageSource, Resolution, ResolveMode, ResolvedManagedFile,
    ResolvedManagedPath, ResolvedManagedPathOrigin, ResolvedPackage,
};
use crate::git::{
    current_rev, ensure_git_dependency, ensure_git_dependency_at_rev, shared_checkout_path,
    shared_repository_path, validate_shared_checkout,
};
use crate::lockfile::{LOCKFILE_NAME, LockedSource, Lockfile};
use crate::manifest::{
    DependencyComponent, DependencySourceKind, DependencySpec, LoadedManifest, ManagedExportSpec,
    ManagedPlacement, PackageRole, RequestedGitRef, load_dependency_from_dir, load_root_from_dir,
};
use crate::paths::display_path;
use crate::report::Reporter;

#[derive(Debug, Default)]
struct ResolverState {
    stack: Vec<PathBuf>,
    resolved_by_path: HashMap<PathBuf, ResolvedPackage>,
    managed_migrations: Vec<ManagedMappingMigration>,
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
    selected_workspace_members: Option<Vec<String>>,
    incoming_managed_paths: Vec<ResolvedManagedPath>,
    extra_package_files: Vec<PathBuf>,
    manifest_override: Option<LoadedManifest>,
}

pub(super) fn validate_git_package(package: &ResolvedPackage, cache_root: &Path) -> Result<()> {
    let PackageSource::Git {
        url, subpath, rev, ..
    } = &package.source
    else {
        return Ok(());
    };

    let checkout_path = shared_checkout_path(cache_root, url, rev)?;
    let canonical_checkout = checkout_path.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize shared checkout {}",
            checkout_path.display()
        )
    })?;
    let expected_root = match subpath {
        Some(subpath) => canonical_checkout
            .join(subpath)
            .canonicalize()
            .with_context(|| {
                format!(
                    "failed to resolve git dependency subpath {}",
                    subpath.display()
                )
            })?,
        None => checkout_path.clone(),
    };
    if package.root != expected_root {
        bail!(
            "git dependency `{}` resolved to {} instead of expected package root {}",
            package.alias,
            package.root.display(),
            expected_root.display()
        );
    }
    let current = current_rev(&checkout_path)?;
    if current.trim() != rev {
        bail!(
            "git dependency `{}` is checked out at {} instead of {}",
            package.alias,
            current.trim(),
            rev
        );
    }

    let mirror_path = shared_repository_path(cache_root, url)?;
    validate_shared_checkout(&checkout_path, &mirror_path, url)
}

pub(super) fn resolve_project(
    root: &Path,
    cache_root: &Path,
    mode: ResolveMode,
    reporter: &Reporter,
    frozen_lockfile: Option<&Lockfile>,
    root_override: Option<&LoadedManifest>,
) -> Result<Resolution> {
    let project_root = if let Some(root_override) = root_override {
        root_override.root.clone()
    } else {
        root.canonicalize()
            .with_context(|| format!("failed to access {}", root.display()))?
    };
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
            package_root: project_root,
            source: PackageSource::Root,
            role: PackageRole::Root,
            selected_components: None,
            selected_workspace_members: None,
            incoming_managed_paths: Vec::new(),
            extra_package_files: Vec::new(),
            manifest_override: None,
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
        packages,
        warnings,
        managed_migrations: state.managed_migrations,
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
        selected_workspace_members,
        incoming_managed_paths,
        extra_package_files,
        manifest_override,
    } = input;
    if let Some(existing) = state.resolved_by_path.get_mut(&package_root) {
        existing.selected_components =
            union_selected_components(existing.selected_components.clone(), selected_components);
        existing.selected_workspace_members = union_selected_workspace_members(
            existing.selected_workspace_members.clone(),
            selected_workspace_members,
        );
        if !incoming_managed_paths.is_empty() {
            existing.managed_paths = merge_managed_paths(
                &package_root,
                &existing.managed_paths,
                &incoming_managed_paths,
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
            if let Some(manifest_override) = manifest_override {
                manifest_override
            } else if let Some(root_override) = context.root_override {
                root_override.clone()
            } else {
                load_root_from_dir(&package_root)?
            }
        }
        PackageRole::Dependency => {
            if let Some(manifest_override) = manifest_override {
                manifest_override
            } else {
                load_dependency_from_dir(&package_root)?
            }
        }
    };

    let (package_managed_paths, package_extra_files) = if role == PackageRole::Dependency {
        resolve_package_managed_exports(&alias, &manifest, &package_root)?
    } else {
        (Vec::new(), Vec::new())
    };
    let mut extra_package_files = extra_package_files;
    merge_extra_package_files(&mut extra_package_files, &package_extra_files);
    let managed_paths = merge_managed_paths(
        &package_root,
        &incoming_managed_paths,
        &package_managed_paths,
    )?;

    let dependencies = manifest
        .manifest
        .active_dependency_entries_for_role(role)
        .into_iter()
        .map(|entry| (entry.alias.to_string(), entry.spec.clone()))
        .chain(workspace_member_dependencies(
            &manifest,
            role,
            selected_workspace_members.clone(),
        )?)
        .map(|(alias, spec)| resolve_dependency(&manifest, role, &alias, &spec, context, state))
        .collect::<Result<Vec<_>>>()?;

    let digest = compute_package_digest(&manifest, &extra_package_files)?;
    let resolved = ResolvedPackage {
        alias,
        root: package_root.clone(),
        manifest,
        source,
        digest,
        selected_components,
        selected_workspace_members,
        managed_paths,
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
            let dependency_manifest = load_dependency_from_dir(&dependency_root)?;
            if dependency_manifest.manifest.workspace.is_none() && dependency.members.is_some() {
                bail!(
                    "dependency `{alias}` field `members` is supported only for workspace dependencies"
                );
            }
            let (incoming_managed_paths, extra_package_files, managed_migration) =
                resolve_incoming_managed_paths(
                    parent_role,
                    alias,
                    dependency,
                    &dependency_manifest,
                    &dependency_root,
                )?;
            if let Some(managed_migration) = managed_migration {
                state.managed_migrations.push(managed_migration);
            }
            resolve_package(
                context,
                ResolvePackageInput {
                    alias: alias.to_string(),
                    package_root: dependency_root,
                    source,
                    role: PackageRole::Dependency,
                    selected_components: dependency.effective_selected_components(),
                    selected_workspace_members: dependency.explicit_members_sorted(),
                    incoming_managed_paths,
                    extra_package_files,
                    manifest_override: Some(dependency_manifest),
                },
                state,
            )
        }
        DependencySourceKind::Git => {
            let url = dependency.resolved_git_url()?;
            let requested_ref = dependency.requested_git_ref_or_none()?;
            let checkout = if let Some(lockfile) = context.frozen_lockfile {
                let locked = locked_git_source(
                    lockfile,
                    alias,
                    &url,
                    dependency.subpath.as_deref(),
                    requested_ref,
                )?;
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
                    requested_ref,
                    context.mode == ResolveMode::Sync,
                    context.reporter,
                )?
            };
            let dependency_root =
                resolve_git_dependency_root(alias, &checkout.path, dependency.subpath.as_deref())?;
            let source = PackageSource::Git {
                url: checkout.url,
                subpath: dependency.subpath.clone(),
                tag: checkout.tag,
                branch: checkout.branch,
                rev: checkout.rev,
            };
            let dependency_manifest = load_dependency_from_dir(&dependency_root)?;
            if dependency_manifest.manifest.workspace.is_none() && dependency.members.is_some() {
                bail!(
                    "dependency `{alias}` field `members` is supported only for workspace dependencies"
                );
            }
            let (incoming_managed_paths, extra_package_files, managed_migration) =
                resolve_incoming_managed_paths(
                    parent_role,
                    alias,
                    dependency,
                    &dependency_manifest,
                    &dependency_root,
                )?;
            if let Some(managed_migration) = managed_migration {
                state.managed_migrations.push(managed_migration);
            }
            resolve_package(
                context,
                ResolvePackageInput {
                    alias: alias.to_string(),
                    package_root: if dependency.subpath.is_some() {
                        dependency_manifest.root.clone()
                    } else {
                        dependency_root.clone()
                    },
                    source,
                    role: PackageRole::Dependency,
                    selected_components: dependency.effective_selected_components(),
                    selected_workspace_members: dependency.explicit_members_sorted(),
                    incoming_managed_paths,
                    extra_package_files,
                    manifest_override: Some(dependency_manifest),
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
    subpath: Option<&Path>,
    requested_ref: Option<RequestedGitRef<'_>>,
) -> Result<&'a LockedSource> {
    let matches_requested_ref = |source: &LockedSource| match requested_ref {
        Some(RequestedGitRef::Tag(tag)) => {
            source.tag.as_deref() == Some(tag) && source.branch.is_none()
        }
        Some(RequestedGitRef::Branch(branch)) => {
            source.branch.as_deref() == Some(branch) && source.tag.is_none()
        }
        Some(RequestedGitRef::Revision(revision)) => {
            source.rev.as_deref() == Some(revision)
                && source.tag.is_none()
                && source.branch.is_none()
        }
        Some(RequestedGitRef::VersionReq(requirement)) => {
            source
                .tag
                .as_deref()
                .and_then(crate::git::parse_semver_tag)
                .is_some_and(|version| requirement.matches(&version))
                && source.branch.is_none()
        }
        None => true,
    };

    let mut matching_sources = lockfile
        .packages
        .iter()
        .filter(|package| {
            package.source.kind == "git"
                && package.source.url.as_deref() == Some(url)
                && package.source.path.as_deref() == subpath.map(display_path).as_deref()
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

fn resolve_git_dependency_root(
    alias: &str,
    checkout_root: &Path,
    subpath: Option<&Path>,
) -> Result<PathBuf> {
    let canonical_checkout = checkout_root.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize dependency `{alias}` checkout {}",
            checkout_root.display()
        )
    })?;
    let Some(subpath) = subpath else {
        return Ok(checkout_root.to_path_buf());
    };

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
            display_path(subpath),
            canonical_checkout.display()
        );
    }
    if !canonical.is_dir() {
        bail!(
            "dependency `{alias}` subpath `{}` must point to a directory, found {}",
            display_path(subpath),
            canonical.display()
        );
    }
    Ok(canonical)
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

fn union_selected_workspace_members(
    left: Option<Vec<String>>,
    right: Option<Vec<String>>,
) -> Option<Vec<String>> {
    match (left, right) {
        (None, None) => None,
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (Some(mut left), Some(right)) => {
            left.extend(right);
            left.sort();
            left.dedup();
            Some(left)
        }
    }
}

fn workspace_member_dependencies(
    manifest: &LoadedManifest,
    role: PackageRole,
    selected_members: Option<Vec<String>>,
) -> Result<Vec<(String, DependencySpec)>> {
    let workspace_members = manifest.resolved_workspace_members()?;
    if workspace_members.is_empty() {
        return Ok(Vec::new());
    }

    let selected = match role {
        PackageRole::Root => workspace_members
            .iter()
            .map(|member| member.id.clone())
            .collect::<Vec<_>>(),
        PackageRole::Dependency => {
            let requested = selected_members.unwrap_or_default();
            let available = workspace_members
                .iter()
                .map(|member| member.id.as_str())
                .collect::<HashSet<_>>();
            for member in &requested {
                if !available.contains(member.as_str()) {
                    bail!(
                        "workspace dependency selects unknown member `{member}` in {}",
                        manifest.root.display()
                    );
                }
            }
            requested
        }
    };

    let selected = selected.into_iter().collect::<HashSet<_>>();
    Ok(workspace_members
        .into_iter()
        .filter(|member| selected.contains(&member.id))
        .map(|member| {
            (
                member.id,
                DependencySpec {
                    github: None,
                    url: None,
                    path: Some(member.path),
                    subpath: None,
                    tag: None,
                    branch: None,
                    revision: None,
                    version: None,
                    components: None,
                    members: None,
                    managed: None,
                    enabled: true,
                },
            )
        })
        .collect())
}

fn resolve_incoming_managed_paths(
    parent_role: PackageRole,
    alias: &str,
    dependency: &DependencySpec,
    dependency_manifest: &LoadedManifest,
    dependency_root: &Path,
) -> Result<(
    Vec<ResolvedManagedPath>,
    Vec<PathBuf>,
    Option<ManagedMappingMigration>,
)> {
    let (legacy_paths, legacy_files) =
        resolve_legacy_dependency_managed_paths(parent_role, alias, dependency, dependency_root)?;
    let (package_paths, package_files) =
        resolve_package_managed_exports(alias, dependency_manifest, dependency_root)?;

    if legacy_paths.is_empty() {
        return Ok((Vec::new(), Vec::new(), None));
    }
    if package_paths.is_empty() {
        return Ok((legacy_paths, legacy_files, None));
    }

    let legacy_entries = managed_file_entries(&legacy_paths);
    let package_entries = managed_file_entries(&package_paths);
    if !legacy_entries.is_subset(&package_entries) {
        bail!(
            "dependency `{alias}` declares both legacy `[[dependencies.{alias}.managed]]` entries in the root manifest and package-owned `[[managed_exports]]`; remove the legacy root mappings because they do not match the package exports"
        );
    }

    let mut extra_package_files = package_files;
    merge_extra_package_files(&mut extra_package_files, &legacy_files);
    Ok((
        package_paths,
        extra_package_files,
        Some(ManagedMappingMigration {
            alias: alias.to_string(),
            legacy_target_roots: dependency
                .managed_mappings()
                .iter()
                .map(|mapping| mapping.normalized_target())
                .collect::<Result<Vec<_>>>()?,
            adds_additional_package_exports: package_entries.len() > legacy_entries.len(),
        }),
    ))
}

fn resolve_legacy_dependency_managed_paths(
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

    resolve_managed_paths(
        alias,
        dependency_root,
        dependency
            .managed_mappings()
            .iter()
            .map(|spec| {
                Ok(ResolvedManagedPathSpec {
                    source_root: spec.normalized_source()?,
                    target_root: spec.normalized_target()?,
                    origin: ResolvedManagedPathOrigin::LegacyDependencyMapping,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    )
}

fn resolve_package_managed_exports(
    alias: &str,
    dependency_manifest: &LoadedManifest,
    dependency_root: &Path,
) -> Result<(Vec<ResolvedManagedPath>, Vec<PathBuf>)> {
    if dependency_manifest.manifest.managed_exports.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let package_name = dependency_manifest.effective_name();
    resolve_managed_paths(
        alias,
        dependency_root,
        dependency_manifest
            .manifest
            .managed_exports
            .iter()
            .map(|spec| resolve_managed_export_spec(spec, &package_name))
            .collect::<Result<Vec<_>>>()?,
    )
}

#[derive(Debug, Clone)]
struct ResolvedManagedPathSpec {
    source_root: PathBuf,
    target_root: PathBuf,
    origin: ResolvedManagedPathOrigin,
}

fn resolve_managed_export_spec(
    spec: &ManagedExportSpec,
    package_name: &str,
) -> Result<ResolvedManagedPathSpec> {
    let target_root = match spec.placement {
        ManagedPlacement::Package => PathBuf::from(".nodus")
            .join("packages")
            .join(package_name)
            .join(spec.normalized_target()?),
        ManagedPlacement::Project => spec.normalized_target()?,
    };

    Ok(ResolvedManagedPathSpec {
        source_root: spec.normalized_source()?,
        target_root,
        origin: ResolvedManagedPathOrigin::PackageManagedExport {
            placement: spec.placement,
        },
    })
}

fn resolve_managed_paths(
    alias: &str,
    dependency_root: &Path,
    specs: Vec<ResolvedManagedPathSpec>,
) -> Result<(Vec<ResolvedManagedPath>, Vec<PathBuf>)> {
    if specs.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut ownership_roots = Vec::<PathBuf>::new();
    let mut concrete_targets = HashSet::<PathBuf>::new();
    let mut mappings = Vec::new();
    let mut extra_package_files = Vec::new();

    for spec in specs {
        let source_root = spec.source_root;
        let target_root = spec.target_root;
        validate_managed_ownership_root(alias, &ownership_roots, &target_root)?;

        let source_path =
            resolve_dependency_managed_source_path(alias, dependency_root, &source_root)?;
        let metadata = fs::metadata(&source_path)
            .with_context(|| format!("failed to read managed source {}", source_path.display()))?;
        let files = if metadata.is_file() {
            if !concrete_targets.insert(target_root.clone()) {
                bail!(
                    "dependency `{alias}` managed mapping resolves multiple sources into {}",
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
                        "dependency `{alias}` managed mapping resolves multiple sources into {}",
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
            origin: spec.origin,
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

fn merge_managed_paths(
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
                "managed targets for {} overlap at `{}` and `{}`",
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
                "managed targets for {} overlap at `{}`",
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

fn managed_file_entries(managed_paths: &[ResolvedManagedPath]) -> HashSet<(PathBuf, PathBuf)> {
    managed_paths
        .iter()
        .flat_map(|path| {
            path.files
                .iter()
                .map(|file| (file.source_relative.clone(), file.target_relative.clone()))
        })
        .collect()
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
