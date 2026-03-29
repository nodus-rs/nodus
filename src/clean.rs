use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::execution::{ExecutionMode, PreviewChange};
use crate::git::{shared_checkout_path, shared_repository_path};
use crate::lockfile::{LOCKFILE_NAME, Lockfile};
use crate::report::Reporter;
use crate::store::{STORE_ROOT, snapshot_path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanSummary {
    pub repository_count: usize,
    pub checkout_count: usize,
    pub snapshot_count: usize,
}

#[derive(Debug, Default)]
struct CleanPlan {
    checkout_paths: Vec<PathBuf>,
    checkout_parent_paths: Vec<PathBuf>,
    repository_paths: Vec<PathBuf>,
    snapshot_paths: Vec<PathBuf>,
}

impl CleanPlan {
    fn summary(&self) -> CleanSummary {
        CleanSummary {
            repository_count: self.repository_paths.len(),
            checkout_count: self.checkout_paths.len(),
            snapshot_count: self.snapshot_paths.len(),
        }
    }

    fn preview(&self, reporter: &Reporter) -> Result<()> {
        for path in &self.checkout_paths {
            reporter.preview(&PreviewChange::Remove(path.clone()))?;
        }
        for path in &self.checkout_parent_paths {
            reporter.preview(&PreviewChange::Remove(path.clone()))?;
        }
        for path in &self.repository_paths {
            reporter.preview(&PreviewChange::Remove(path.clone()))?;
        }
        for path in &self.snapshot_paths {
            reporter.preview(&PreviewChange::Remove(path.clone()))?;
        }

        Ok(())
    }

    fn execute(&self) -> Result<()> {
        for path in &self.checkout_paths {
            remove_path_if_exists(path)?;
        }
        for path in &self.checkout_parent_paths {
            remove_empty_dir_if_exists(path)?;
        }
        for path in &self.repository_paths {
            remove_path_if_exists(path)?;
        }
        for path in &self.snapshot_paths {
            remove_path_if_exists(path)?;
        }

        Ok(())
    }
}

pub fn clean_project_cache(
    project_root: &Path,
    cache_root: &Path,
    execution_mode: ExecutionMode,
    reporter: &Reporter,
) -> Result<CleanSummary> {
    let plan = project_clean_plan(project_root, cache_root)?;
    let summary = plan.summary();

    match execution_mode {
        ExecutionMode::DryRun => plan.preview(reporter)?,
        ExecutionMode::Apply => plan.execute()?,
    }

    Ok(summary)
}

pub fn clean_all_cache(
    cache_root: &Path,
    execution_mode: ExecutionMode,
    reporter: &Reporter,
) -> Result<CleanSummary> {
    let plan = all_cache_clean_plan(cache_root);
    let summary = plan.summary();

    match execution_mode {
        ExecutionMode::DryRun => plan.preview(reporter)?,
        ExecutionMode::Apply => plan.execute()?,
    }

    Ok(summary)
}

fn project_clean_plan(project_root: &Path, cache_root: &Path) -> Result<CleanPlan> {
    let lockfile_path = project_root.join(LOCKFILE_NAME);
    if !lockfile_path.exists() {
        bail!(
            "missing {} in {}; run `nodus sync` first or use `nodus clean --all` to clear the shared cache directories",
            LOCKFILE_NAME,
            project_root.display()
        );
    }

    let lockfile = Lockfile::read_for_sync(&lockfile_path)?;
    let mut repository_paths = BTreeSet::new();
    let mut checkout_paths = BTreeSet::new();
    let mut snapshot_paths = BTreeSet::new();

    for package in &lockfile.packages {
        snapshot_paths.insert(snapshot_path(cache_root, &package.digest)?);

        if package.source.kind == "git" {
            let url = package.source.url.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "package `{}` in {} is missing git url metadata",
                    package.alias,
                    LOCKFILE_NAME
                )
            })?;
            let rev = package.source.rev.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "package `{}` in {} is missing git revision metadata",
                    package.alias,
                    LOCKFILE_NAME
                )
            })?;
            repository_paths.insert(shared_repository_path(cache_root, url)?);
            checkout_paths.insert(shared_checkout_path(cache_root, url, rev)?);
        }
    }

    Ok(CleanPlan {
        checkout_parent_paths: checkout_parent_paths(cache_root, &checkout_paths)?,
        checkout_paths: existing_paths(checkout_paths),
        repository_paths: existing_paths(repository_paths),
        snapshot_paths: existing_paths(snapshot_paths),
    })
}

fn all_cache_clean_plan(cache_root: &Path) -> CleanPlan {
    let repository_paths =
        existing_paths(std::iter::once(cache_root.join("repositories")).collect());
    let checkout_paths = existing_paths(std::iter::once(cache_root.join("checkouts")).collect());
    let snapshot_paths = existing_paths(std::iter::once(cache_root.join(STORE_ROOT)).collect());

    CleanPlan {
        checkout_paths,
        checkout_parent_paths: Vec::new(),
        repository_paths,
        snapshot_paths,
    }
}

fn existing_paths(paths: BTreeSet<PathBuf>) -> Vec<PathBuf> {
    paths.into_iter().filter(|path| path.exists()).collect()
}

fn checkout_parent_paths(
    cache_root: &Path,
    checkout_paths: &BTreeSet<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let checkouts_root = cache_root.join("checkouts");
    let targeted = checkout_paths.iter().cloned().collect::<HashSet<_>>();
    let mut parents = BTreeSet::new();

    for checkout_path in checkout_paths {
        let Some(parent) = checkout_path.parent() else {
            continue;
        };
        if !parent.starts_with(&checkouts_root) || !parent.exists() {
            continue;
        }

        let mut removable = true;
        for entry in fs::read_dir(parent)
            .with_context(|| format!("failed to read checkout parent {}", parent.display()))?
        {
            let entry =
                entry.with_context(|| format!("failed to read entry in {}", parent.display()))?;
            if !targeted.contains(&entry.path()) {
                removable = false;
                break;
            }
        }

        if removable {
            parents.insert(parent.to_path_buf());
        }
    }

    Ok(existing_paths(parents))
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to access path {}", path.display()));
        }
    };

    if metadata.is_dir() {
        fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove directory {}", path.display()))?;
    } else {
        fs::remove_file(path)
            .with_context(|| format!("failed to remove file {}", path.display()))?;
    }

    Ok(())
}

fn remove_empty_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove directory {}", path.display()))
        }
    }
}
