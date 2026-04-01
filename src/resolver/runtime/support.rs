use std::collections::HashSet;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rayon::prelude::*;

use super::{
    ManagedCollision, ManagedCollisionChoice, ManagedCollisionResolver, ManagedCollisionSource,
    PlannedFileWrite, Resolution, ResolvedManagedPathOrigin, SyncExecutionPlan, SyncMode,
    SyncSummary, TtyManagedCollisionResolver, UnmanagedCollision,
};
use crate::adapters::ManagedFile;
use crate::execution::{ExecutionMode, PreviewChange};
use crate::lockfile::Lockfile;
use crate::manifest::LoadedManifest;
use crate::paths::{display_path, strip_path_prefix};
use crate::report::Reporter;
use crate::store::write_atomic;

#[allow(clippy::too_many_arguments)]
pub(super) fn build_sync_execution_plan(
    original_root: &LoadedManifest,
    working_root: &LoadedManifest,
    lockfile_path: &Path,
    lockfile: &Lockfile,
    runtime_root: &Path,
    owned_paths: &HashSet<PathBuf>,
    desired_paths: &HashSet<PathBuf>,
    planned_files: &[ManagedFile],
    warnings: Vec<String>,
    summary: SyncSummary,
    sync_mode: SyncMode,
) -> Result<SyncExecutionPlan> {
    let manifest_write = planned_manifest_write(original_root, working_root)?;
    let mut removals = planned_stale_paths(owned_paths, desired_paths);
    removals.extend(planned_paths_to_replace(
        planned_files,
        owned_paths,
        &working_root.root,
    )?);
    removals.sort();
    removals.dedup();
    let lockfile_write = if sync_mode.checks_lockfile() {
        None
    } else {
        Some(planned_lockfile_write(lockfile_path, lockfile)?)
    };

    Ok(SyncExecutionPlan {
        runtime_root: runtime_root.to_path_buf(),
        owned_paths: owned_paths.clone(),
        desired_paths: desired_paths.clone(),
        manifest_write,
        removals,
        managed_writes: planned_files.to_vec(),
        lockfile_write,
        warnings,
        summary,
    })
}

pub(super) fn execute_sync_plan(
    plan: &SyncExecutionPlan,
    execution_mode: ExecutionMode,
    reporter: &Reporter,
) -> Result<()> {
    if execution_mode.is_dry_run() {
        if let Some(write) = &plan.manifest_write {
            reporter.preview(&planned_write_preview_change(write))?;
        }
        for path in &plan.removals {
            reporter.preview(&PreviewChange::Remove(path.clone()))?;
        }
        if !plan.managed_writes.is_empty() {
            reporter.status("Preview", "managed runtime outputs")?;
            for file in &plan.managed_writes {
                let change = if file.path.exists() {
                    PreviewChange::Write(file.path.clone())
                } else {
                    PreviewChange::Create(file.path.clone())
                };
                reporter.preview(&change)?;
            }
        }
        if let Some(write) = &plan.lockfile_write {
            reporter.preview(&planned_write_preview_change(write))?;
        }
    } else {
        if let Some(write) = &plan.manifest_write {
            reporter.status("Writing", write.path.display())?;
            write_atomic(&write.path, &write.contents)?;
        }
        prune_stale_files(&plan.owned_paths, &plan.desired_paths, &plan.runtime_root)?;
        prepare_managed_paths_for_write(
            &plan.managed_writes,
            &plan.owned_paths,
            &plan.runtime_root,
        )?;
        reporter.status("Writing", "managed runtime outputs")?;
        write_managed_files(&plan.managed_writes)?;
        if let Some(write) = &plan.lockfile_write {
            reporter.status("Writing", write.path.display())?;
            write_atomic(&write.path, &write.contents)?;
        }
    }

    for warning in &plan.warnings {
        reporter.warning(warning)?;
    }

    Ok(())
}

fn planned_manifest_write(
    original_root: &LoadedManifest,
    working_root: &LoadedManifest,
) -> Result<Option<PlannedFileWrite>> {
    let Some(path) = &working_root.manifest_path else {
        return Ok(None);
    };
    let contents = working_root
        .read_package_file(path)
        .with_context(|| format!("failed to read manifest {}", path.display()))?;
    let current = if path.exists() {
        Some(
            std::fs::read(path)
                .with_context(|| format!("failed to read manifest {}", path.display()))?,
        )
    } else {
        None
    };
    if original_root.manifest_path.as_deref() == Some(path)
        && current
            .as_ref()
            .is_some_and(|existing| *existing == contents)
    {
        Ok(None)
    } else {
        Ok(Some(PlannedFileWrite {
            path: path.clone(),
            contents,
            create: !path.exists(),
        }))
    }
}

fn planned_lockfile_write(path: &Path, lockfile: &Lockfile) -> Result<PlannedFileWrite> {
    let contents = toml::to_string_pretty(lockfile)
        .context("failed to serialize lockfile")?
        .into_bytes();
    Ok(PlannedFileWrite {
        path: path.to_path_buf(),
        create: !path.exists(),
        contents,
    })
}

fn planned_stale_paths(
    owned_paths: &HashSet<PathBuf>,
    desired_paths: &HashSet<PathBuf>,
) -> Vec<PathBuf> {
    let mut removals = owned_paths
        .difference(desired_paths)
        .filter(|path| fs::symlink_metadata(path).is_ok())
        .cloned()
        .collect::<Vec<_>>();
    removals.sort();
    removals
}

fn planned_paths_to_replace(
    planned_files: &[ManagedFile],
    owned_paths: &HashSet<PathBuf>,
    project_root: &Path,
) -> Result<Vec<PathBuf>> {
    let mut removed = HashSet::new();

    for file in planned_files {
        if file.path.is_dir()
            && path_is_owned(&file.path, owned_paths)
            && removed.insert(file.path.clone())
        {
            continue;
        }

        let mut current = file.path.parent();
        while let Some(parent) = current {
            if parent == project_root {
                break;
            }
            if parent.is_file()
                && path_is_owned(parent, owned_paths)
                && removed.insert(parent.to_path_buf())
            {
                break;
            }
            current = parent.parent();
        }
    }

    let mut removals = removed.into_iter().collect::<Vec<_>>();
    removals.sort();
    Ok(removals)
}

fn planned_write_preview_change(write: &PlannedFileWrite) -> PreviewChange {
    if write.create {
        PreviewChange::Create(write.path.clone())
    } else {
        PreviewChange::Write(write.path.clone())
    }
}

pub(super) fn enforce_capabilities(
    resolution: &Resolution,
    allow_high_sensitivity: bool,
    reporter: &Reporter,
) -> Result<()> {
    let mut high_sensitivity = Vec::new();

    for package in &resolution.packages {
        for capability in &package.manifest.manifest.capabilities {
            reporter.note(format!(
                "capability {} {} ({})",
                package.alias, capability.id, capability.sensitivity
            ))?;
            if let Some(justification) = &capability.justification {
                reporter.note(format!("justification: {justification}"))?;
            }
            if capability.sensitivity.eq_ignore_ascii_case("high") {
                high_sensitivity.push(format!("{}:{}", package.alias, capability.id));
            }
        }
    }

    if !high_sensitivity.is_empty() && !allow_high_sensitivity {
        high_sensitivity.sort();
        bail!(
            "high-sensitivity capabilities require --allow-high-sensitivity: {}",
            high_sensitivity.join(", ")
        );
    }

    Ok(())
}

pub(super) fn find_unmanaged_collision(
    planned_files: &[ManagedFile],
    owned_paths: &HashSet<PathBuf>,
    project_root: &Path,
) -> Option<UnmanagedCollision> {
    for file in planned_files {
        if file.path.exists()
            && !path_is_owned(&file.path, owned_paths)
            && !allows_managed_merge(project_root, &file.path)
        {
            return Some(UnmanagedCollision {
                path: file.path.clone(),
            });
        }

        let mut current = file.path.parent();
        while let Some(parent) = current {
            if parent == project_root {
                break;
            }
            if parent.exists() && parent.is_file() && !path_is_owned(parent, owned_paths) {
                return Some(UnmanagedCollision {
                    path: parent.to_path_buf(),
                });
            }
            current = parent.parent();
        }
    }

    None
}

fn allows_managed_merge(project_root: &Path, path: &Path) -> bool {
    path == project_root.join(".mcp.json")
}

pub(super) fn find_managed_collision(
    project_root: &Path,
    resolution: &Resolution,
    collision: &UnmanagedCollision,
) -> Option<ManagedCollision> {
    for package in &resolution.packages {
        for managed_path in package.managed_paths() {
            let ownership_root = project_root.join(&managed_path.ownership_root);
            if collision.path == ownership_root
                || collision.path.starts_with(&ownership_root)
                || ownership_root.starts_with(&collision.path)
            {
                return Some(ManagedCollision {
                    alias: package.alias.clone(),
                    ownership_root: managed_path.ownership_root.clone(),
                    collision_path: collision.path.clone(),
                    source: managed_collision_source(managed_path.origin),
                });
            }

            if managed_path.files.iter().any(|file| {
                let target = project_root.join(&file.target_relative);
                collision.path == target || target.starts_with(&collision.path)
            }) {
                return Some(ManagedCollision {
                    alias: package.alias.clone(),
                    ownership_root: managed_path.ownership_root.clone(),
                    collision_path: collision.path.clone(),
                    source: managed_collision_source(managed_path.origin),
                });
            }
        }
    }

    None
}

pub(super) fn unmanaged_collision_guidance(
    project_root: &Path,
    collision: &ManagedCollision,
    sync_mode: SyncMode,
) -> String {
    match collision.source {
        ManagedCollisionSource::LegacyDependencyMapping => format!(
            "refusing to overwrite unmanaged file {}. Managed target {} from dependency `{}` collides with an existing path. Rerun plain `nodus sync` on a TTY to choose whether to adopt that target, remove the managed mapping from `nodus.toml`, or cancel; {} cannot prompt interactively",
            display_path(&collision.collision_path),
            display_path(&project_root.join(&collision.ownership_root)),
            collision.alias,
            sync_mode.flag(),
        ),
        ManagedCollisionSource::PackageManagedExport => format!(
            "refusing to overwrite unmanaged file {}. Package-owned managed export {} from dependency `{}` collides with an existing path. Rerun plain `nodus sync` on a TTY to choose whether to adopt that target or cancel; {} cannot prompt interactively",
            display_path(&collision.collision_path),
            display_path(&project_root.join(&collision.ownership_root)),
            collision.alias,
            sync_mode.flag(),
        ),
    }
}

impl ManagedCollisionResolver for TtyManagedCollisionResolver {
    fn resolve(
        &mut self,
        project_root: &Path,
        collision: &ManagedCollision,
    ) -> Result<ManagedCollisionChoice> {
        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        let stderr = io::stderr();
        let mut stderr = stderr.lock();
        prompt_for_managed_collision(project_root, collision, &mut stdin, &mut stderr)
    }
}

fn prompt_for_managed_collision(
    project_root: &Path,
    collision: &ManagedCollision,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<ManagedCollisionChoice> {
    writeln!(
        output,
        "{} {} from dependency `{}` collides with existing unmanaged path {}.",
        match collision.source {
            ManagedCollisionSource::LegacyDependencyMapping => "Managed target",
            ManagedCollisionSource::PackageManagedExport => "Package-owned managed export",
        },
        display_path(&project_root.join(&collision.ownership_root)),
        collision.alias,
        display_path(&collision.collision_path)
    )?;
    writeln!(output, "Choose how to continue:")?;
    writeln!(
        output,
        "  1. adopt  (let Nodus take ownership and overwrite managed files under that target)"
    )?;
    if collision.source == ManagedCollisionSource::LegacyDependencyMapping {
        writeln!(
            output,
            "  2. remove (delete the corresponding managed mapping from nodus.toml and continue)"
        )?;
        writeln!(output, "  3. cancel")?;
    } else {
        writeln!(output, "  2. cancel")?;
    }
    write!(output, "> ")?;
    output.flush()?;

    let mut line = String::new();
    input.read_line(&mut line)?;
    parse_managed_collision_choice(&line, collision.source)
}

fn parse_managed_collision_choice(
    answer: &str,
    source: ManagedCollisionSource,
) -> Result<ManagedCollisionChoice> {
    match (source, answer.trim().to_ascii_lowercase().as_str()) {
        (_, "1" | "adopt") => Ok(ManagedCollisionChoice::Adopt),
        (ManagedCollisionSource::LegacyDependencyMapping, "2" | "remove") => {
            Ok(ManagedCollisionChoice::RemoveMapping)
        }
        (ManagedCollisionSource::LegacyDependencyMapping, "3" | "cancel") => {
            Ok(ManagedCollisionChoice::Cancel)
        }
        (ManagedCollisionSource::PackageManagedExport, "2" | "cancel") => {
            Ok(ManagedCollisionChoice::Cancel)
        }
        (_, other) => bail!("invalid collision resolution `{other}`"),
    }
}

fn managed_collision_source(origin: ResolvedManagedPathOrigin) -> ManagedCollisionSource {
    match origin {
        ResolvedManagedPathOrigin::LegacyDependencyMapping => {
            ManagedCollisionSource::LegacyDependencyMapping
        }
        ResolvedManagedPathOrigin::PackageManagedExport { .. } => {
            ManagedCollisionSource::PackageManagedExport
        }
    }
}

fn prune_stale_files(
    owned_paths: &HashSet<PathBuf>,
    desired_paths: &HashSet<PathBuf>,
    project_root: &Path,
) -> Result<()> {
    for path in owned_paths.difference(desired_paths) {
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if metadata.file_type().is_dir() {
                fs::remove_dir_all(path).with_context(|| {
                    format!(
                        "failed to remove stale managed directory {}",
                        path.display()
                    )
                })?;
            } else {
                fs::remove_file(path).with_context(|| {
                    format!("failed to remove stale managed file {}", path.display())
                })?;
            }
            prune_empty_parent_dirs(path, project_root)?;
        }
    }

    Ok(())
}

pub(super) fn write_managed_files(planned_files: &[ManagedFile]) -> Result<()> {
    planned_files
        .par_iter()
        .map(|file| {
            write_atomic(&file.path, &file.contents)
                .with_context(|| format!("failed to write managed file {}", file.path.display()))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect()
}

fn prepare_managed_paths_for_write(
    planned_files: &[ManagedFile],
    owned_paths: &HashSet<PathBuf>,
    project_root: &Path,
) -> Result<()> {
    let mut removed = HashSet::new();

    for file in planned_files {
        if file.path.is_dir()
            && path_is_owned(&file.path, owned_paths)
            && removed.insert(file.path.clone())
        {
            fs::remove_dir_all(&file.path).with_context(|| {
                format!(
                    "failed to replace managed directory {} with a file",
                    file.path.display()
                )
            })?;
            prune_empty_parent_dirs(&file.path, project_root)?;
        }

        let mut current = file.path.parent();
        while let Some(parent) = current {
            if parent == project_root {
                break;
            }
            if parent.is_file()
                && path_is_owned(parent, owned_paths)
                && removed.insert(parent.to_path_buf())
            {
                fs::remove_file(parent).with_context(|| {
                    format!(
                        "failed to replace managed file {} with a directory",
                        parent.display()
                    )
                })?;
                prune_empty_parent_dirs(parent, project_root)?;
            }
            current = parent.parent();
        }
    }

    Ok(())
}

pub(super) fn validate_state_consistency(
    owned_paths: &HashSet<PathBuf>,
    desired_paths: &HashSet<PathBuf>,
    planned_files: &[ManagedFile],
) -> Result<()> {
    if let Some(path) = owned_paths.difference(desired_paths).next() {
        bail!("stale managed state entry for {}", path.display());
    }

    for path in desired_paths.intersection(owned_paths) {
        if !path.exists() {
            bail!("managed file is missing from disk: {}", path.display());
        }
    }

    for file in planned_files {
        if path_is_owned(&file.path, owned_paths) && !file.path.exists() {
            bail!("managed file is missing from disk: {}", file.path.display());
        }
    }

    Ok(())
}

fn path_is_owned(path: &Path, owned_paths: &HashSet<PathBuf>) -> bool {
    owned_paths
        .iter()
        .any(|owned| path == owned || path.starts_with(owned))
}

pub(super) fn load_owned_paths(
    project_root: &Path,
    lockfile: Option<&Lockfile>,
) -> Result<HashSet<PathBuf>> {
    if let Some(lockfile) = lockfile {
        return if lockfile.uses_current_schema() {
            lockfile.managed_paths(project_root)
        } else {
            lockfile.managed_paths_for_sync(project_root)
        };
    }

    Ok(HashSet::new())
}

pub(super) fn managed_path_is_owned(path: &Path, owned_paths: &HashSet<PathBuf>) -> bool {
    path_is_owned(path, owned_paths)
}

pub(super) fn recover_runtime_owned_paths(
    project_root: &Path,
    desired_paths: &HashSet<PathBuf>,
) -> HashSet<PathBuf> {
    desired_paths
        .iter()
        .filter(|path| is_runtime_managed_path(project_root, path))
        .cloned()
        .collect()
}

pub(super) fn recover_runtime_owned_dirs_from_disk(
    project_root: &Path,
    desired_paths: &HashSet<PathBuf>,
    planned_files: &[ManagedFile],
) -> HashSet<PathBuf> {
    desired_paths
        .iter()
        .filter(|path| is_runtime_managed_path(project_root, path))
        .filter(|path| path.is_dir())
        .filter(|path| {
            planned_files.iter().any(|file| {
                file.path.starts_with(path)
                    && file.path.is_file()
                    && fs::read(&file.path)
                        .map(|contents| contents == file.contents)
                        .unwrap_or(false)
            })
        })
        .cloned()
        .collect()
}

fn is_runtime_managed_path(project_root: &Path, path: &Path) -> bool {
    let Some(relative) = strip_path_prefix(path, project_root) else {
        return false;
    };
    if relative == Path::new(".mcp.json") {
        return true;
    }
    let mut components = relative.components();
    let Some(first) = components.next() else {
        return false;
    };
    match first.as_os_str().to_string_lossy().as_ref() {
        ".agents" | ".claude" | ".codex" | ".cursor" | ".opencode" => true,
        ".github" => matches!(
            components.next().map(|component| component.as_os_str().to_string_lossy()),
            Some(second) if second == "skills" || second == "agents"
        ),
        _ => false,
    }
}

pub(super) fn prune_empty_parent_dirs(path: &Path, project_root: &Path) -> Result<()> {
    let stop_roots = [
        project_root.to_path_buf(),
        project_root.join(".agents"),
        project_root.join(".claude"),
        project_root.join(".codex"),
        project_root.join(".cursor"),
        project_root.join(".github"),
        project_root.join(".opencode"),
    ];
    let mut current = path.parent();

    while let Some(dir) = current {
        if stop_roots.iter().any(|root| dir == root) {
            break;
        }
        match fs::remove_dir(dir) {
            Ok(()) => {
                current = dir.parent();
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                current = dir.parent();
            }
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to prune empty directory {}", dir.display()));
            }
        }
    }

    Ok(())
}
