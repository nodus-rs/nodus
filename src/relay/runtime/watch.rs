use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

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
    pub(super) debounce: Duration,
    pub(super) fallback_interval: Duration,
    pub(super) max_events: Option<usize>,
    pub(super) timeout: Option<Duration>,
}

impl Default for RelayWatchOptions {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(100),
            fallback_interval: Duration::from_secs(30),
            max_events: None,
            timeout: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RelayWatchInvocation<'a> {
    pub(super) repo_path_override: Option<&'a Path>,
    pub(super) via_override: Option<Adapter>,
    pub(super) create_missing: bool,
    pub(super) options: RelayWatchOptions,
}

pub(super) async fn watch_dependency_in_dir_with_options(
    project_root: &Path,
    cache_root: &Path,
    package: &str,
    invocation: RelayWatchInvocation<'_>,
    reporter: &Reporter,
) -> Result<Vec<RelaySummary>> {
    let packages = vec![package.to_string()];
    watch_dependencies_in_dir_impl_with_options(
        project_root,
        cache_root,
        &packages,
        invocation,
        reporter,
    )
    .await
}

pub(super) async fn watch_dependencies_in_dir_with_options(
    project_root: &Path,
    cache_root: &Path,
    packages: &[String],
    invocation: RelayWatchInvocation<'_>,
    reporter: &Reporter,
) -> Result<Vec<RelaySummary>> {
    watch_dependencies_in_dir_impl_with_options(
        project_root,
        cache_root,
        packages,
        invocation,
        reporter,
    )
    .await
}

async fn watch_dependencies_in_dir_impl_with_options(
    project_root: &Path,
    cache_root: &Path,
    packages: &[String],
    invocation: RelayWatchInvocation<'_>,
    reporter: &Reporter,
) -> Result<Vec<RelaySummary>> {
    if packages.is_empty() {
        bail!("relay watch requires at least one dependency");
    }
    if packages.len() > 1 && invocation.repo_path_override.is_some() {
        bail!("`nodus relay --repo-path` requires exactly one dependency");
    }

    // Initial relay pass.
    let mut summaries = Vec::with_capacity(packages.len());
    for package in packages {
        let summary = relay_dependency_in_dir(
            project_root,
            cache_root,
            package,
            invocation.repo_path_override,
            invocation.via_override,
            invocation.create_missing,
            reporter,
        )?;
        reporter.finish(format!(
            "relayed {} into {}; created {} and updated {} source files",
            summary.alias,
            display_relative(project_root, &summary.linked_repo),
            summary.created_file_count,
            summary.updated_file_count,
        ))?;
        summaries.push(summary);
    }

    let mut state = capture_watch_state(project_root, cache_root, packages, reporter)?;

    // Set up notify watcher.
    let (tx, mut rx) = mpsc::channel(256);
    let watcher_result = setup_watcher(&state, project_root, tx);
    let _watcher = match watcher_result {
        Ok(watcher) => {
            reporter.note("watching managed outputs for changes; press Ctrl-C to stop")?;
            Some(watcher)
        }
        Err(err) => {
            reporter.warning(format!(
                "failed to initialize file watcher ({err:#}); falling back to periodic polling"
            ))?;
            reporter.note("watching managed outputs for changes; press Ctrl-C to stop")?;
            None
        }
    };

    let deadline = invocation
        .options
        .timeout
        .map(|t| tokio::time::Instant::now() + t);

    loop {
        if invocation
            .options
            .max_events
            .is_some_and(|max| summaries.len() >= max)
        {
            return Ok(summaries);
        }

        // Wait for a notify event, fallback timeout, deadline, or ctrl-c.
        tokio::select! {
            _ = rx.recv() => {
                // Debounce: wait briefly then drain any queued events.
                tokio::time::sleep(invocation.options.debounce).await;
                while rx.try_recv().is_ok() {}
            }
            _ = tokio::time::sleep(invocation.options.fallback_interval) => {}
            _ = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            } => {
                return Ok(summaries);
            }
            _ = tokio::signal::ctrl_c() => {
                return Ok(summaries);
            }
        }

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
            let summary = relay_dependency_in_dir(
                project_root,
                cache_root,
                &package,
                None,
                None,
                invocation.create_missing,
                reporter,
            )?;
            reporter.finish(format!(
                "relayed {} into {}; created {} and updated {} source files",
                summary.alias,
                display_relative(project_root, &summary.linked_repo),
                summary.created_file_count,
                summary.updated_file_count,
            ))?;
            summaries.push(summary);
        }
    }
}

fn setup_watcher(
    state: &RelayWatchState,
    project_root: &Path,
    tx: mpsc::Sender<()>,
) -> Result<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            let _ = tx.blocking_send(());
        }
    })?;

    // Watch config files (non-recursive).
    for path in state.config.keys() {
        if path.exists() {
            let watch_path = if path.is_file() {
                path.parent().unwrap_or(project_root)
            } else {
                path.as_path()
            };
            let _ = watcher.watch(watch_path, RecursiveMode::NonRecursive);
        }
    }

    // Watch managed output directories (recursive).
    let mut watched_dirs = BTreeSet::new();
    for package_managed in state.managed.values() {
        for path in package_managed.keys() {
            if let Some(parent) = path.parent() {
                let mut candidate = parent;
                while let Some(grandparent) = candidate.parent() {
                    if grandparent == project_root || !grandparent.starts_with(project_root) {
                        break;
                    }
                    candidate = grandparent;
                }
                if watched_dirs.insert(candidate.to_path_buf()) && candidate.exists() {
                    watcher.watch(candidate, RecursiveMode::Recursive)?;
                }
            }
        }
    }

    Ok(watcher)
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
    let managed_names = crate::adapters::ManagedArtifactNames::from_resolved_packages(
        workspace.resolution.packages.iter(),
    );
    let mut managed = BTreeMap::new();
    for package in packages {
        let dependency = dependency_context(&workspace, package)?;
        let linked_repo = resolve_existing_link(&workspace.local_config, &dependency)?;
        let mappings = build_mappings(
            &managed_names,
            &workspace.resolution.packages,
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
    let hash: [u8; 32] = *blake3::hash(&contents).as_bytes();
    Ok(PathFingerprint::File(hash))
}
