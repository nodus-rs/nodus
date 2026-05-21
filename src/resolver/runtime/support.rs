use std::collections::HashSet;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use dialoguer::Select;
use rayon::prelude::*;

use super::{
    ManagedCollision, ManagedCollisionChoice, ManagedCollisionResolver, ManagedCollisionSource,
    PlannedFileWrite, Resolution, ResolvedManagedPathOrigin, SyncExecutionPlan, SyncMode,
    SyncSummary, TtyManagedCollisionResolver, UnmanagedCollision,
};
use crate::adapters::ManagedFile;
use crate::execution::{ExecutionMode, PreviewChange};
use crate::lockfile::{Lockfile, OwnedSet};
use crate::manifest::LoadedManifest;
use crate::paths::{display_path, strip_path_prefix};
use crate::report::Reporter;
use crate::selection::interactive_select_theme;
use crate::store::write_atomic;

#[allow(clippy::too_many_arguments)]
pub(super) fn build_sync_execution_plan(
    original_root: &LoadedManifest,
    working_root: &LoadedManifest,
    lockfile_path: &Path,
    lockfile: &Lockfile,
    runtime_root: &Path,
    owned_paths: &OwnedSet,
    desired_paths: &HashSet<PathBuf>,
    planned_files: &[ManagedFile],
    external_files: Vec<ManagedFile>,
    warnings: Vec<String>,
    summary: SyncSummary,
    sync_mode: SyncMode,
) -> Result<SyncExecutionPlan> {
    let manifest_write = planned_manifest_write(original_root, working_root)?;
    let mut removals = planned_stale_paths(owned_paths, desired_paths, planned_files)?;
    removals.extend(planned_paths_to_replace(
        planned_files,
        owned_paths,
        desired_paths,
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
        manifest_write,
        removals,
        managed_writes: planned_files.to_vec(),
        external_writes: external_files,
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
        if !plan.external_writes.is_empty() {
            reporter.status("Preview", "managed user config")?;
            for file in &plan.external_writes {
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
        for path in &plan.removals {
            reporter.status("Removing", path.display())?;
            remove_path_and_empty_parents(path, &plan.runtime_root)?;
        }
        reporter.status("Writing", "managed runtime outputs")?;
        write_managed_files(&plan.managed_writes)?;
        if !plan.external_writes.is_empty() {
            reporter.status("Writing", "managed user config")?;
            write_managed_files(&plan.external_writes)?;
        }
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
    owned_paths: &OwnedSet,
    desired_paths: &HashSet<PathBuf>,
    planned_files: &[ManagedFile],
) -> Result<Vec<PathBuf>> {
    let mut removals: HashSet<PathBuf> = owned_paths
        .exact
        .difference(desired_paths)
        .filter(|path| fs::symlink_metadata(path).is_ok())
        .cloned()
        .collect();

    // Planned-file paths give us the actual leaves Nodus is about to write
    // inside any owned subtree. Anything else inside the subtree is stale.
    let planned_paths: HashSet<&Path> = planned_files
        .iter()
        .map(|file| file.path.as_path())
        .collect();

    // For each owned subtree root, walk on-disk and queue every file that no
    // longer corresponds to a planned path or a desired path. Subtree roots
    // that themselves no longer have any desired content are queued as well
    // so the directory disappears at the end of the sweep.
    for subtree in &owned_paths.subtrees {
        if !subtree.exists() {
            continue;
        }
        let root_still_wanted = desired_paths
            .iter()
            .any(|desired| desired == subtree.as_path() || desired.starts_with(subtree));
        let entries = walk_subtree_files(subtree)?;
        for entry in entries {
            if !desired_paths.contains(&entry) && !planned_paths.contains(entry.as_path()) {
                removals.insert(entry);
            }
        }
        if !root_still_wanted {
            // No desired content under the subtree — drop the root and the
            // executor's recursive remove will take everything underneath.
            removals.insert(subtree.clone());
        }
    }

    // For each filename-prefix rule, list the directory and queue matching
    // files not in desired_paths. Non-recursive by construction.
    for rule in &owned_paths.prefixes {
        if !rule.dir.exists() {
            continue;
        }
        let read_dir = match fs::read_dir(&rule.dir) {
            Ok(iter) => iter,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read managed directory {}", rule.dir.display())
                });
            }
        };
        for entry in read_dir {
            let entry = entry.with_context(|| {
                format!("failed to inspect managed directory {}", rule.dir.display())
            })?;
            let file_type = entry.file_type().with_context(|| {
                format!("failed to read metadata for {}", entry.path().display())
            })?;
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with(&rule.prefix) {
                continue;
            }
            let path = entry.path();
            if !desired_paths.contains(&path) && !planned_paths.contains(path.as_path()) {
                removals.insert(path);
            }
        }
    }

    let mut removals: Vec<PathBuf> = removals.into_iter().collect();
    removals.sort();
    Ok(removals)
}

fn walk_subtree_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(current) = pending.pop() {
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect managed path {}", current.display())
                });
            }
        };
        if metadata.file_type().is_symlink() {
            // Treat symlinks as opaque entries — record them so the removal
            // pass clears the link without recursing through it.
            found.push(current);
            continue;
        }
        if metadata.is_file() {
            found.push(current);
            continue;
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(&current).with_context(|| {
                format!("failed to read managed directory {}", current.display())
            })? {
                let entry = entry.with_context(|| {
                    format!("failed to inspect managed directory {}", current.display())
                })?;
                pending.push(entry.path());
            }
        }
    }
    Ok(found)
}

fn planned_paths_to_replace(
    planned_files: &[ManagedFile],
    owned_paths: &OwnedSet,
    desired_paths: &HashSet<PathBuf>,
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
            if path_is_owned(parent, owned_paths) {
                if parent.is_file() && removed.insert(parent.to_path_buf()) {
                    break;
                }
                if parent.is_dir()
                    && !desired_paths
                        .iter()
                        .any(|desired| desired != parent && desired.starts_with(parent))
                    && !directory_exactly_matches_planned_files(project_root, parent, planned_files)
                    && removed.insert(parent.to_path_buf())
                {
                    break;
                }
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
    owned_paths: &OwnedSet,
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
            // When `parent` exists on disk as a regular file but Nodus expects
            // a directory there (because something inside it is planned), it's
            // a structural collision. We need to reject it even if a v10
            // subtree claim transitively "owns" the path — subtree ownership
            // describes a directory tree, so a leaf file at an interior path
            // means the user planted something Nodus didn't put there.
            if parent.exists()
                && parent.is_file()
                && !path_is_owned_exact_or_prefix(parent, owned_paths)
            {
                return Some(UnmanagedCollision {
                    path: parent.to_path_buf(),
                });
            }
            current = parent.parent();
        }
    }

    None
}

/// Like [`OwnedSet::contains`] but ignores subtree membership. Subtree claims
/// describe a directory tree; when collision detection finds a regular file
/// where Nodus expects a parent directory, the subtree-membership check would
/// spuriously claim ownership of a user-planted file. Exact-path and
/// filename-prefix ownership are unambiguous and still considered.
fn path_is_owned_exact_or_prefix(path: &Path, owned_paths: &OwnedSet) -> bool {
    if owned_paths.exact.contains(path) {
        return true;
    }
    for rule in &owned_paths.prefixes {
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

fn allows_managed_merge(project_root: &Path, path: &Path) -> bool {
    managed_merge_paths(project_root).contains(path)
}

pub(super) fn managed_merge_paths(project_root: &Path) -> HashSet<PathBuf> {
    [
        project_root.join(".agents/.gitignore"),
        project_root.join(".claude/.gitignore"),
        project_root.join(".claude/settings.json"),
        project_root.join(".claude/settings.local.json"),
        project_root.join(".codex/.gitignore"),
        project_root.join(".codex/hooks.json"),
        project_root.join(".mcp.json"),
        project_root.join(".cursor/.gitignore"),
        project_root.join("opencode.json"),
        project_root.join(".opencode/.gitignore"),
        project_root.join(".codex/config.toml"),
    ]
    .into_iter()
    .collect()
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

pub(super) fn find_runtime_output_collision(
    planned_files: &[ManagedFile],
    collision: &UnmanagedCollision,
) -> Option<ManagedCollision> {
    planned_files
        .iter()
        .find(|file| {
            collision.path == file.path
                || collision.path.starts_with(&file.path)
                || file.path.starts_with(&collision.path)
        })
        .map(|_| ManagedCollision {
            alias: String::new(),
            ownership_root: collision.path.clone(),
            collision_path: collision.path.clone(),
            source: ManagedCollisionSource::RuntimeOutput,
        })
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
        ManagedCollisionSource::RuntimeOutput => format!(
            "refusing to overwrite unmanaged file {}. Managed runtime output {} collides with an existing path. Rerun plain `nodus sync` on a TTY to choose whether to adopt that output or cancel; {} cannot prompt interactively",
            display_path(&collision.collision_path),
            display_path(&collision.collision_path),
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
    render_managed_collision_notice(project_root, collision, output)?;
    if !cfg!(test) && io::stdin().is_terminal() && io::stderr().is_terminal() {
        writeln!(
            output,
            "Use arrow keys to choose how Nodus should continue, then press Enter."
        )?;
        output.flush()?;
        return prompt_for_managed_collision_interactive(collision);
    }

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

fn render_managed_collision_notice(
    project_root: &Path,
    collision: &ManagedCollision,
    output: &mut impl Write,
) -> Result<()> {
    match collision.source {
        ManagedCollisionSource::LegacyDependencyMapping
        | ManagedCollisionSource::PackageManagedExport => {
            writeln!(
                output,
                "{} {} from dependency `{}` collides with existing unmanaged path {}.",
                match collision.source {
                    ManagedCollisionSource::LegacyDependencyMapping => "Managed target",
                    ManagedCollisionSource::PackageManagedExport => "Package-owned managed export",
                    ManagedCollisionSource::RuntimeOutput => unreachable!(),
                },
                display_path(&project_root.join(&collision.ownership_root)),
                collision.alias,
                display_path(&collision.collision_path)
            )?;
        }
        ManagedCollisionSource::RuntimeOutput => {
            writeln!(
                output,
                "Managed runtime output {} collides with existing unmanaged path {}.",
                display_path(&collision.collision_path),
                display_path(&collision.collision_path)
            )?;
        }
    }
    Ok(())
}

fn prompt_for_managed_collision_interactive(
    collision: &ManagedCollision,
) -> Result<ManagedCollisionChoice> {
    let items = managed_collision_prompt_items(collision.source);
    let selection = Select::with_theme(&interactive_select_theme())
        .with_prompt("Choose how Nodus should continue")
        .items(&items)
        .default(0)
        .interact_on_opt(&dialoguer::console::Term::stderr())?;

    Ok(match selection {
        Some(index) => managed_collision_choice_for_index(collision.source, index)?,
        None => ManagedCollisionChoice::Cancel,
    })
}

fn managed_collision_prompt_items(source: ManagedCollisionSource) -> Vec<&'static str> {
    match source {
        ManagedCollisionSource::LegacyDependencyMapping => vec![
            "Adopt and overwrite the managed target",
            "Remove the legacy managed mapping from nodus.toml",
            "Cancel sync",
        ],
        ManagedCollisionSource::PackageManagedExport => vec![
            "Adopt and overwrite the package-managed export",
            "Cancel sync",
        ],
        ManagedCollisionSource::RuntimeOutput => {
            vec!["Adopt and overwrite this runtime output", "Cancel sync"]
        }
    }
}

fn managed_collision_choice_for_index(
    source: ManagedCollisionSource,
    index: usize,
) -> Result<ManagedCollisionChoice> {
    match (source, index) {
        (_, 0) => Ok(ManagedCollisionChoice::Adopt),
        (ManagedCollisionSource::LegacyDependencyMapping, 1) => {
            Ok(ManagedCollisionChoice::RemoveMapping)
        }
        (ManagedCollisionSource::LegacyDependencyMapping, 2)
        | (
            ManagedCollisionSource::PackageManagedExport | ManagedCollisionSource::RuntimeOutput,
            1,
        ) => Ok(ManagedCollisionChoice::Cancel),
        (_, other) => bail!("invalid collision selection index `{other}`"),
    }
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
        (
            ManagedCollisionSource::PackageManagedExport | ManagedCollisionSource::RuntimeOutput,
            "2" | "cancel",
        ) => Ok(ManagedCollisionChoice::Cancel),
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

pub(super) fn write_managed_files(planned_files: &[ManagedFile]) -> Result<()> {
    planned_files
        .par_iter()
        .map(|file| {
            write_atomic(&file.path, &file.contents)
                .with_context(|| format!("failed to write managed file {}", file.path.display()))?;
            apply_managed_file_mode(file)?;
            Ok(())
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect()
}

/// Mark claude-plugin runtime scripts executable when their contents start with
/// a shebang. User-supplied `hooks.json` configs invoke these scripts via
/// `${CLAUDE_PLUGIN_ROOT}/.../foo.sh` directly — without +x the shell raises
/// Permission denied. Scoped to the plugin install tree so unrelated managed
/// files keep their 0o600 default.
fn apply_managed_file_mode(file: &ManagedFile) -> Result<()> {
    if !is_claude_plugin_runtime_path(&file.path) {
        return Ok(());
    }
    if !file.contents.starts_with(b"#!") {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(&file.path)
            .with_context(|| {
                format!(
                    "failed to read metadata for managed file {}",
                    file.path.display()
                )
            })?
            .permissions();
        let mode = permissions.mode();
        let with_exec = mode | 0o111;
        if with_exec != mode {
            permissions.set_mode(with_exec);
            fs::set_permissions(&file.path, permissions).with_context(|| {
                format!(
                    "failed to mark managed file executable {}",
                    file.path.display()
                )
            })?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
    }
    Ok(())
}

fn is_claude_plugin_runtime_path(path: &Path) -> bool {
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        if component.as_os_str() != ".nodus" {
            continue;
        }
        if components.next().map(|c| c.as_os_str()) != Some("packages".as_ref()) {
            continue;
        }
        // Skip the alias segment.
        if components.next().is_none() {
            return false;
        }
        return matches!(
            components.next().map(|c| c.as_os_str().to_owned()),
            Some(segment) if segment == "claude-plugin"
        );
    }
    false
}

pub(super) fn remove_path_and_empty_parents(path: &Path, project_root: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_dir() {
                fs::remove_dir_all(path).with_context(|| {
                    format!(
                        "failed to remove conflicting managed directory {}",
                        path.display()
                    )
                })?;
            } else {
                fs::remove_file(path).with_context(|| {
                    format!(
                        "failed to remove conflicting managed file {}",
                        path.display()
                    )
                })?;
            }
            prune_empty_parent_dirs(path, project_root)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to inspect conflicting managed path {}",
                path.display()
            )
        }),
    }
}

pub(super) fn validate_state_consistency(
    owned_paths: &OwnedSet,
    desired_paths: &HashSet<PathBuf>,
    planned_files: &[ManagedFile],
) -> Result<()> {
    // Stale-entry detection operates on the concrete exact set only; subtree
    // and prefix rules describe regions whose membership is on-disk-state
    // dependent, so they cannot be compared directly against desired_paths.
    if let Some(path) = owned_paths.exact.difference(desired_paths).next() {
        bail!("stale managed state entry for {}", path.display());
    }

    for path in desired_paths.intersection(&owned_paths.exact) {
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

fn path_is_owned(path: &Path, owned_paths: &OwnedSet) -> bool {
    owned_paths.contains(path)
}

pub(super) fn load_owned_paths(
    project_root: &Path,
    lockfile: Option<&Lockfile>,
) -> Result<OwnedSet> {
    if let Some(lockfile) = lockfile {
        return if lockfile.uses_current_schema() {
            lockfile.owned_set(project_root)
        } else {
            lockfile.owned_set_for_sync(project_root)
        };
    }

    Ok(OwnedSet::default())
}

pub(super) fn managed_path_is_owned(path: &Path, owned_paths: &OwnedSet) -> bool {
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

pub(super) fn recover_runtime_owned_paths_from_disk(
    project_root: &Path,
    desired_paths: &HashSet<PathBuf>,
    planned_files: &[ManagedFile],
) -> HashSet<PathBuf> {
    desired_paths
        .iter()
        .filter(|path| is_runtime_managed_path(project_root, path))
        .filter(|path| !path_has_symlinked_ancestor_within(project_root, path))
        .filter(|path| path_exactly_matches_planned_files(project_root, path, planned_files))
        .cloned()
        .collect()
}

fn path_exactly_matches_planned_files(
    project_root: &Path,
    path: &Path,
    planned_files: &[ManagedFile],
) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() {
        return false;
    }
    if metadata.is_file() {
        return planned_files
            .iter()
            .find(|file| file.path == path)
            .is_some_and(|file| file_exactly_matches_planned_contents(project_root, file));
    }
    if metadata.is_dir() {
        return directory_exactly_matches_planned_files(project_root, path, planned_files);
    }
    false
}

fn file_exactly_matches_planned_contents(project_root: &Path, file: &ManagedFile) -> bool {
    if path_has_symlinked_ancestor_within(project_root, &file.path) {
        return false;
    }
    fs::symlink_metadata(&file.path)
        .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
        && fs::read(&file.path)
            .map(|contents| contents == file.contents)
            .unwrap_or(false)
}

fn directory_exactly_matches_planned_files(
    project_root: &Path,
    path: &Path,
    planned_files: &[ManagedFile],
) -> bool {
    let planned_in_dir = planned_files
        .iter()
        .filter(|file| file.path.starts_with(path))
        .collect::<Vec<_>>();
    if planned_in_dir.is_empty() {
        return false;
    }

    if !planned_in_dir
        .iter()
        .copied()
        .all(|file| file_exactly_matches_planned_contents(project_root, file))
    {
        return false;
    }

    let expected_files = planned_in_dir
        .iter()
        .map(|file| file.path.clone())
        .collect::<HashSet<_>>();
    let mut expected_dirs = HashSet::new();
    for file in &planned_in_dir {
        let mut current = file.path.parent();
        while let Some(parent) = current {
            if parent == path {
                break;
            }
            expected_dirs.insert(parent.to_path_buf());
            current = parent.parent();
        }
    }
    let expected_entries = expected_files
        .into_iter()
        .chain(expected_dirs)
        .collect::<HashSet<_>>();
    let Ok(existing_entries) = collect_entries_under_dir(project_root, path) else {
        return false;
    };
    existing_entries == expected_entries
}

fn collect_entries_under_dir(project_root: &Path, path: &Path) -> Result<HashSet<PathBuf>> {
    if path_has_symlinked_ancestor_within(project_root, path) {
        return Ok(HashSet::new());
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to read metadata for {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(HashSet::new());
    }

    let mut entries = HashSet::new();
    let mut pending = vec![path.to_path_buf()];

    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current)
            .with_context(|| format!("failed to read managed directory {}", current.display()))?
        {
            let entry = entry.with_context(|| {
                format!("failed to inspect managed directory {}", current.display())
            })?;
            let entry_path = entry.path();
            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to read metadata for {}", entry_path.display()))?;
            if file_type.is_symlink() {
                return Ok(HashSet::new());
            } else if file_type.is_dir() {
                entries.insert(entry_path.clone());
                pending.push(entry_path);
            } else if file_type.is_file() {
                entries.insert(entry_path);
            } else {
                return Ok(HashSet::new());
            }
        }
    }

    Ok(entries)
}

fn path_has_symlinked_ancestor_within(project_root: &Path, path: &Path) -> bool {
    path.ancestors()
        .skip(1)
        .take_while(|ancestor| ancestor.starts_with(project_root) && *ancestor != project_root)
        .any(|ancestor| {
            fs::symlink_metadata(ancestor)
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(false)
        })
}

fn is_runtime_managed_path(project_root: &Path, path: &Path) -> bool {
    let global_home = crate::adapters::global_nodus_home(project_root);
    if path == global_home || path.starts_with(&global_home) {
        return true;
    }

    let Some(relative) = strip_path_prefix(path, project_root) else {
        return false;
    };
    if relative == Path::new(".mcp.json") || relative == Path::new("opencode.json") {
        return true;
    }
    let mut components = relative.components();
    let Some(first) = components.next() else {
        return false;
    };
    match first.as_os_str().to_string_lossy().as_ref() {
        ".agents" => {
            let second = components
                .next()
                .map(|component| component.as_os_str().to_string_lossy());
            let third = components
                .next()
                .map(|component| component.as_os_str().to_string_lossy());
            second.is_none()
                || matches!(
                    (second.as_deref(), third.as_deref()),
                    (Some("plugins"), Some("marketplace.json"))
                )
        }
        ".claude" | ".codex" | ".cursor" | ".opencode" => true,
        ".claude-plugin" => matches!(
            components.next().map(|component| component.as_os_str().to_string_lossy()),
            Some(second) if second == "marketplace.json"
        ),
        ".github" => matches!(
            components.next().map(|component| component.as_os_str().to_string_lossy()),
            Some(second) if second == "skills" || second == "agents"
        ),
        ".nodus" => {
            let second = components
                .next()
                .map(|component| component.as_os_str().to_string_lossy());
            let third = components
                .next()
                .map(|component| component.as_os_str().to_string_lossy());
            let fourth = components
                .next()
                .map(|component| component.as_os_str().to_string_lossy());
            matches!(
                (second.as_deref(), third.as_deref()),
                (Some("packages"), Some(_))
            ) || matches!(
                (second.as_deref(), third.as_deref(), fourth.as_deref()),
                (Some(".claude-plugin"), Some("marketplace.json"), _)
                    | (Some(".agents"), Some("plugins"), Some("marketplace.json"))
            )
        }
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
        crate::adapters::global_nodus_home(project_root),
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

#[cfg(test)]
mod managed_mode_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn detects_claude_plugin_runtime_paths() {
        assert!(is_claude_plugin_runtime_path(Path::new(
            "/tmp/proj/.nodus/packages/alias/claude-plugin/.claude/hooks/hook.sh"
        )));
        assert!(is_claude_plugin_runtime_path(Path::new(
            ".nodus/packages/alias/claude-plugin/hooks/hook.sh"
        )));
    }

    #[test]
    fn rejects_non_claude_plugin_runtime_paths() {
        assert!(!is_claude_plugin_runtime_path(Path::new(
            "/tmp/proj/.nodus/packages/alias/opencode-plugin/hooks/hook.sh"
        )));
        assert!(!is_claude_plugin_runtime_path(Path::new(
            "/tmp/proj/.claude/hooks/hook.sh"
        )));
        assert!(!is_claude_plugin_runtime_path(Path::new(
            "/tmp/proj/.nodus/packages/alias"
        )));
    }

    #[test]
    fn planned_stale_paths_keeps_prefix_owned_planned_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let hooks_dir = temp.path().join(".claude/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let planned_path = hooks_dir.join("nodus-plugin-hook-shared-11111111.sh");
        let stale_path = hooks_dir.join("nodus-plugin-hook-shared-22222222.sh");
        fs::write(&planned_path, "#!/bin/sh\n").unwrap();
        fs::write(&stale_path, "#!/bin/sh\n").unwrap();

        let owned_paths = OwnedSet {
            prefixes: vec![crate::lockfile::OwnedPrefixPath {
                dir: hooks_dir,
                prefix: "nodus-plugin-hook-shared-".into(),
            }],
            ..OwnedSet::default()
        };
        let desired_paths = HashSet::new();
        let planned_files = vec![ManagedFile {
            path: planned_path.clone(),
            contents: b"#!/bin/sh\n".to_vec(),
        }];

        let removals = planned_stale_paths(&owned_paths, &desired_paths, &planned_files).unwrap();

        assert!(!removals.contains(&planned_path));
        assert!(removals.contains(&stale_path));
    }

    #[cfg(unix)]
    #[test]
    fn apply_managed_file_mode_sets_exec_bit_on_plugin_scripts() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::TempDir::new().unwrap();
        let plugin_script = temp
            .path()
            .join(".nodus/packages/alias/claude-plugin/.claude/hooks/hook.sh");
        fs::create_dir_all(plugin_script.parent().unwrap()).unwrap();
        let contents = b"#!/usr/bin/env bash\necho hi\n".to_vec();
        fs::write(&plugin_script, &contents).unwrap();
        fs::set_permissions(&plugin_script, fs::Permissions::from_mode(0o600)).unwrap();

        apply_managed_file_mode(&ManagedFile {
            path: plugin_script.clone(),
            contents,
        })
        .unwrap();

        let mode = fs::metadata(&plugin_script).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o100,
            0o100,
            "owner exec bit should be set (mode={mode:o})"
        );
    }

    #[cfg(unix)]
    #[test]
    fn apply_managed_file_mode_skips_non_shebang_contents() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::TempDir::new().unwrap();
        let plugin_file = temp
            .path()
            .join(".nodus/packages/alias/claude-plugin/.claude/hooks/hooks.json");
        fs::create_dir_all(plugin_file.parent().unwrap()).unwrap();
        let contents = br#"{"hooks":{}}"#.to_vec();
        fs::write(&plugin_file, &contents).unwrap();
        fs::set_permissions(&plugin_file, fs::Permissions::from_mode(0o600)).unwrap();

        apply_managed_file_mode(&ManagedFile {
            path: plugin_file.clone(),
            contents,
        })
        .unwrap();

        let mode = fs::metadata(&plugin_file).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o111,
            0,
            "no exec bits should be set for non-scripts"
        );
    }

    #[cfg(unix)]
    #[test]
    fn apply_managed_file_mode_leaves_non_plugin_paths_alone() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join(".claude/hooks/nodus-hook-foo.sh");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let contents = b"#!/bin/sh\n".to_vec();
        fs::write(&path, &contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        apply_managed_file_mode(&ManagedFile {
            path: path.clone(),
            contents,
        })
        .unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o111,
            0,
            "managed wrappers outside plugin tree must stay non-exec"
        );
    }
}
