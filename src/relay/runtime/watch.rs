use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::{
    RelaySummary, build_mappings, dependency_context, display_relative, load_workspace,
    relay_dependency_in_dir, resolve_existing_link,
};
use crate::adapters::Adapter;
use crate::lockfile::LOCKFILE_NAME;
use crate::manifest::MANIFEST_FILE;
use crate::report::Reporter;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelayWatchState {
    config: BTreeMap<PathBuf, PathFingerprint>,
    managed: BTreeMap<String, BTreeMap<PathBuf, PathFingerprint>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathFingerprint {
    Missing,
    Directory,
    File([u8; 32]),
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RelayWatchOptions {
    pub(super) poll_interval: Duration,
    pub(super) max_events: Option<usize>,
    pub(super) max_polls: Option<usize>,
}

impl Default for RelayWatchOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(1),
            max_events: None,
            max_polls: None,
        }
    }
}

pub(super) fn watch_dependency_in_dir_with_options(
    project_root: &Path,
    cache_root: &Path,
    package: &str,
    repo_path_override: Option<&Path>,
    via_override: Option<Adapter>,
    reporter: &Reporter,
    options: RelayWatchOptions,
) -> Result<Vec<RelaySummary>> {
    let packages = vec![package.to_string()];
    watch_dependencies_in_dir_impl_with_options(
        project_root,
        cache_root,
        &packages,
        repo_path_override,
        via_override,
        reporter,
        options,
    )
}

pub(super) fn watch_dependencies_in_dir_with_options(
    project_root: &Path,
    cache_root: &Path,
    packages: &[String],
    via_override: Option<Adapter>,
    reporter: &Reporter,
    options: RelayWatchOptions,
) -> Result<Vec<RelaySummary>> {
    watch_dependencies_in_dir_impl_with_options(
        project_root,
        cache_root,
        packages,
        None,
        via_override,
        reporter,
        options,
    )
}

fn watch_dependencies_in_dir_impl_with_options(
    project_root: &Path,
    cache_root: &Path,
    packages: &[String],
    repo_path_override: Option<&Path>,
    via_override: Option<Adapter>,
    reporter: &Reporter,
    options: RelayWatchOptions,
) -> Result<Vec<RelaySummary>> {
    if packages.is_empty() {
        bail!("relay watch requires at least one dependency");
    }
    if packages.len() > 1 && repo_path_override.is_some() {
        bail!("`nodus relay --repo-path` requires exactly one dependency");
    }

    let mut summaries = Vec::with_capacity(packages.len());
    for package in packages {
        let summary = relay_dependency_in_dir(
            project_root,
            cache_root,
            package,
            repo_path_override,
            via_override,
            reporter,
        )?;
        reporter.finish(format!(
            "relayed {} into {}; updated {} source files",
            summary.alias,
            display_relative(project_root, &summary.linked_repo),
            summary.updated_file_count,
        ))?;
        summaries.push(summary);
    }

    let mut state = capture_watch_state(project_root, cache_root, packages, reporter)?;
    reporter.note("watching managed outputs for changes; press Ctrl-C to stop")?;
    let mut polls = 0usize;

    loop {
        if options
            .max_events
            .is_some_and(|max_events| summaries.len() >= max_events)
        {
            return Ok(summaries);
        }
        if options
            .max_polls
            .is_some_and(|max_polls| polls >= max_polls)
        {
            return Ok(summaries);
        }

        thread::sleep(options.poll_interval);
        polls += 1;

        let next_state = capture_watch_state(project_root, cache_root, packages, reporter)?;
        let config_changed = next_state.config != state.config;
        let changed_packages = changed_watch_packages(&state, &next_state);
        if !config_changed && changed_packages.is_empty() {
            continue;
        }

        state = next_state;
        if changed_packages.is_empty() {
            reporter.note("reloaded relay watch inputs")?;
            continue;
        }

        for package in changed_packages {
            reporter.status("Watching", format!("detected managed edits for {package}"))?;
            let summary =
                relay_dependency_in_dir(project_root, cache_root, &package, None, None, reporter)?;
            reporter.finish(format!(
                "relayed {} into {}; updated {} source files",
                summary.alias,
                display_relative(project_root, &summary.linked_repo),
                summary.updated_file_count,
            ))?;
            summaries.push(summary);
        }
    }
}

fn changed_watch_packages(previous: &RelayWatchState, next: &RelayWatchState) -> Vec<String> {
    let mut aliases = previous
        .managed
        .keys()
        .chain(next.managed.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    aliases.retain(|alias| previous.managed.get(alias) != next.managed.get(alias));
    aliases.into_iter().collect()
}

fn capture_watch_state(
    project_root: &Path,
    cache_root: &Path,
    packages: &[String],
    reporter: &Reporter,
) -> Result<RelayWatchState> {
    let workspace = load_workspace(project_root, cache_root, reporter)?;
    let mut managed = BTreeMap::new();
    for package in packages {
        let dependency = dependency_context(&workspace, package)?;
        let linked_repo = resolve_existing_link(&workspace.local_config, &dependency)?;
        let mappings = build_mappings(
            &dependency,
            &workspace.project_root,
            workspace.selected_adapters,
            &linked_repo,
        )?;

        let mut package_managed = BTreeMap::new();
        for path in mappings.into_iter().map(|mapping| mapping.managed_path) {
            package_managed
                .entry(path.clone())
                .or_insert(path_fingerprint(&path)?);
        }
        managed.insert(dependency.alias.clone(), package_managed);
    }

    let adapter_markers = [
        ".agents",
        ".claude",
        ".codex",
        ".github/skills",
        ".github/agents",
        ".cursor",
        ".opencode",
        "AGENTS.md",
    ];
    let mut config = BTreeMap::new();
    for path in [
        project_root.join(MANIFEST_FILE),
        project_root.join(LOCKFILE_NAME),
        crate::local_config::config_path(project_root),
    ] {
        config.insert(path.clone(), path_fingerprint(&path)?);
    }
    for marker in adapter_markers {
        let path = project_root.join(marker);
        config.insert(path.clone(), path_fingerprint(&path)?);
    }

    Ok(RelayWatchState { config, managed })
}

fn path_fingerprint(path: &Path) -> Result<PathFingerprint> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PathFingerprint::Missing);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };

    if metadata.is_dir() {
        return Ok(PathFingerprint::Directory);
    }
    if !metadata.is_file() {
        return Ok(PathFingerprint::Missing);
    }

    let contents = fs::read(path)
        .with_context(|| format!("failed to read watched file {}", path.display()))?;
    let digest = Sha256::digest(contents);
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&digest);
    Ok(PathFingerprint::File(hash))
}
