use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use crate::adapters::{Adapter, Adapters, ArtifactKind, managed_artifact_path, managed_skill_root};
use crate::execution::{ExecutionMode, PreviewChange};
use crate::git::{
    git_urls_match, is_git_repository, normalize_git_url, repository_origin_url,
    resolve_dependency_alias,
};
use crate::local_config::{LocalConfig, RelayLink, config_path, local_dir};
use crate::lockfile::{LOCKFILE_NAME, Lockfile};
use crate::manifest::{DependencySourceKind, MANIFEST_FILE, SkillEntry, load_root_from_dir};
use crate::report::Reporter;
use crate::resolver::{
    PackageSource, ResolvedPackage, resolve_project_from_existing_lockfile_in_dir,
};
use crate::store::snapshot_resolution;

#[derive(Debug, Clone)]
pub struct RelaySummary {
    pub alias: String,
    pub linked_repo: PathBuf,
    pub updated_file_count: usize,
}

#[derive(Debug, Clone)]
struct RelayWorkspace {
    root: crate::manifest::LoadedManifest,
    project_root: PathBuf,
    selected_adapters: Adapters,
    resolution: crate::resolver::Resolution,
    snapshot_roots: HashMap<String, PathBuf>,
    local_config: LocalConfig,
}

#[derive(Debug, Clone)]
struct DependencyContext {
    alias: String,
    url: String,
    package: ResolvedPackage,
    snapshot_root: PathBuf,
}

#[derive(Debug, Clone)]
struct RelayFileMapping {
    managed_path: PathBuf,
    snapshot_path: PathBuf,
    linked_source_path: PathBuf,
    transform: RelayTransform,
}

#[derive(Debug, Clone)]
enum RelayTransform {
    None,
    OpenCodeSkillName { managed_skill_id: String },
}

#[derive(Debug, Clone, Default)]
struct RelayPlan {
    updates: BTreeMap<PathBuf, Vec<u8>>,
    noops: BTreeSet<PathBuf>,
    conflicts: Vec<String>,
}

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
struct RelayWatchOptions {
    poll_interval: Duration,
    max_events: Option<usize>,
    max_polls: Option<usize>,
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

pub fn relay_dependency_in_dir(
    project_root: &Path,
    cache_root: &Path,
    package: &str,
    repo_path_override: Option<&Path>,
    via_override: Option<Adapter>,
    reporter: &Reporter,
) -> Result<RelaySummary> {
    relay_dependency_in_dir_mode(
        project_root,
        cache_root,
        package,
        repo_path_override,
        via_override,
        ExecutionMode::Apply,
        reporter,
    )
}

pub fn relay_dependency_in_dir_dry_run(
    project_root: &Path,
    cache_root: &Path,
    package: &str,
    repo_path_override: Option<&Path>,
    via_override: Option<Adapter>,
    reporter: &Reporter,
) -> Result<RelaySummary> {
    relay_dependency_in_dir_mode(
        project_root,
        cache_root,
        package,
        repo_path_override,
        via_override,
        ExecutionMode::DryRun,
        reporter,
    )
}

fn relay_dependency_in_dir_mode(
    project_root: &Path,
    cache_root: &Path,
    package: &str,
    repo_path_override: Option<&Path>,
    via_override: Option<Adapter>,
    execution_mode: ExecutionMode,
    reporter: &Reporter,
) -> Result<RelaySummary> {
    let mut workspace = load_workspace(project_root, cache_root, reporter)?;
    let dependency = dependency_context(&workspace, package)?;
    let local_config_changed = relay_link_would_change(
        &workspace.local_config,
        &dependency.alias,
        repo_path_override,
        via_override,
        &dependency.url,
    )?;
    let linked_repo = resolve_linked_repo(
        project_root,
        &mut workspace.local_config,
        &dependency,
        repo_path_override,
        via_override,
        execution_mode,
    )?;

    let plan = build_relay_plan(
        &dependency,
        &workspace.project_root,
        workspace.selected_adapters,
        &linked_repo,
    )?;
    if !plan.conflicts.is_empty() {
        bail!(
            "relay conflicts for `{}`:\n{}",
            dependency.alias,
            plan.conflicts.join("\n")
        );
    }

    if execution_mode.is_dry_run() {
        if local_config_changed {
            reporter.preview(&PreviewChange::PersistLocalConfig(config_path(
                project_root,
            )))?;
            let gitignore_path = local_dir(project_root).join(".gitignore");
            let gitignore_change = if gitignore_path.exists() {
                PreviewChange::Write(gitignore_path)
            } else {
                PreviewChange::Create(gitignore_path)
            };
            reporter.preview(&gitignore_change)?;
        }
        reporter.status("Preview", format!("relay edits for {}", dependency.alias))?;
        for path in plan.updates.keys() {
            reporter.preview(&PreviewChange::Relay(path.clone()))?;
        }
        for path in &plan.noops {
            reporter.note(format!(
                "{} already matches managed edits",
                display_relative(&linked_repo, path)
            ))?;
        }
    } else {
        reporter.status(
            "Relaying",
            format!("managed edits for {}", dependency.alias),
        )?;
        for (path, contents) in &plan.updates {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            crate::store::write_atomic(path, contents)
                .with_context(|| format!("failed to write relayed source {}", path.display()))?;
            reporter.note(format!("updated {}", display_relative(&linked_repo, path)))?;
        }
        for path in &plan.noops {
            reporter.note(format!(
                "{} already matches managed edits",
                display_relative(&linked_repo, path)
            ))?;
        }

        workspace.local_config.save_in_dir(project_root)?;
    }

    Ok(RelaySummary {
        alias: dependency.alias,
        linked_repo,
        updated_file_count: plan.updates.len(),
    })
}

pub fn watch_dependency_in_dir(
    project_root: &Path,
    cache_root: &Path,
    package: &str,
    repo_path_override: Option<&Path>,
    via_override: Option<Adapter>,
    reporter: &Reporter,
) -> Result<()> {
    watch_dependency_in_dir_with_options(
        project_root,
        cache_root,
        package,
        repo_path_override,
        via_override,
        reporter,
        RelayWatchOptions::default(),
    )
    .map(|_| ())
}

pub fn watch_dependencies_in_dir(
    project_root: &Path,
    cache_root: &Path,
    packages: &[String],
    via_override: Option<Adapter>,
    reporter: &Reporter,
) -> Result<()> {
    watch_dependencies_in_dir_with_options(
        project_root,
        cache_root,
        packages,
        via_override,
        reporter,
        RelayWatchOptions::default(),
    )
    .map(|_| ())
}

pub fn ensure_no_pending_relay_edits_in_dir(project_root: &Path, cache_root: &Path) -> Result<()> {
    let reporter = Reporter::silent();
    if !project_root.join(LOCKFILE_NAME).exists() {
        return Ok(());
    }
    let workspace = load_workspace_if_linked(project_root, cache_root, &reporter)?;
    if workspace.local_config.relay.is_empty() {
        return Ok(());
    }

    let linked_aliases = workspace
        .root
        .manifest
        .dependencies
        .keys()
        .filter(|alias| workspace.local_config.relay.contains_key(*alias))
        .cloned()
        .collect::<Vec<_>>();
    if linked_aliases.is_empty() {
        return Ok(());
    }

    let mut blocked = Vec::new();
    for alias in linked_aliases {
        let dependency = dependency_context(&workspace, &alias)?;
        let linked = resolve_existing_link(&workspace.local_config, &dependency)?;
        let plan = build_relay_plan(
            &dependency,
            &workspace.project_root,
            workspace.selected_adapters,
            &linked,
        )?;
        if !plan.conflicts.is_empty() {
            blocked.push(format!("{alias}: {}", plan.conflicts.join("; ")));
            continue;
        }
        if !plan.updates.is_empty() {
            blocked.push(format!(
                "{alias}: {} pending relayed source files",
                plan.updates.len()
            ));
        }
    }

    if blocked.is_empty() {
        Ok(())
    } else {
        bail!(
            "pending relay edits would be overwritten:\n{}\nRun `nodus relay <dependency>` or discard the managed edits first.",
            blocked.join("\n")
        )
    }
}

fn load_workspace(
    project_root: &Path,
    cache_root: &Path,
    reporter: &Reporter,
) -> Result<RelayWorkspace> {
    let root = load_root_from_dir(project_root)?;
    let local_config = LocalConfig::load_in_dir(project_root)?;
    let (resolution, lockfile) = resolve_project_from_existing_lockfile_in_dir(
        project_root,
        cache_root,
        Adapters::NONE,
        reporter,
    )?;
    let selected_adapters = adapters_from_lockfile(&lockfile);
    let snapshot_roots = snapshot_resolution(cache_root, &resolution)?
        .into_iter()
        .map(|stored| (stored.digest, stored.snapshot_root))
        .collect::<HashMap<_, _>>();

    Ok(RelayWorkspace {
        root,
        project_root: project_root.to_path_buf(),
        selected_adapters,
        resolution,
        snapshot_roots,
        local_config,
    })
}

fn load_workspace_if_linked(
    project_root: &Path,
    cache_root: &Path,
    reporter: &Reporter,
) -> Result<RelayWorkspace> {
    let root = load_root_from_dir(project_root)?;
    let local_config = LocalConfig::load_in_dir(project_root)?;
    if local_config.relay.is_empty() {
        return Ok(RelayWorkspace {
            root,
            project_root: project_root.to_path_buf(),
            selected_adapters: Adapters::NONE,
            resolution: crate::resolver::Resolution {
                project_root: project_root.to_path_buf(),
                packages: Vec::new(),
                warnings: Vec::new(),
            },
            snapshot_roots: HashMap::new(),
            local_config,
        });
    }
    let (resolution, lockfile) = resolve_project_from_existing_lockfile_in_dir(
        project_root,
        cache_root,
        Adapters::NONE,
        reporter,
    )?;
    let selected_adapters = adapters_from_lockfile(&lockfile);
    let snapshot_roots = snapshot_resolution(cache_root, &resolution)?
        .into_iter()
        .map(|stored| (stored.digest, stored.snapshot_root))
        .collect::<HashMap<_, _>>();

    Ok(RelayWorkspace {
        root,
        project_root: project_root.to_path_buf(),
        selected_adapters,
        resolution,
        snapshot_roots,
        local_config,
    })
}

fn adapters_from_lockfile(lockfile: &Lockfile) -> Adapters {
    lockfile
        .managed_files
        .iter()
        .filter_map(|path| {
            if path.starts_with(".agents/") {
                Some(Adapter::Agents)
            } else if path.starts_with(".claude/") {
                Some(Adapter::Claude)
            } else if path.starts_with(".codex/") {
                Some(Adapter::Codex)
            } else if path.starts_with(".cursor/") {
                Some(Adapter::Cursor)
            } else if path.starts_with(".opencode/") {
                Some(Adapter::OpenCode)
            } else {
                None
            }
        })
        .fold(Adapters::NONE, |selected, adapter| {
            selected.union(adapter.into())
        })
}

fn dependency_context(workspace: &RelayWorkspace, package: &str) -> Result<DependencyContext> {
    let alias = resolve_dependency_alias(&workspace.root.manifest.dependencies, package)?;
    let spec = workspace
        .root
        .manifest
        .dependencies
        .get(&alias)
        .ok_or_else(|| anyhow!("dependency `{alias}` does not exist"))?;
    if spec.source_kind()? != DependencySourceKind::Git {
        bail!("relay supports direct git dependencies only; `{alias}` is a path dependency");
    }
    let url = normalize_git_url(&spec.resolved_git_url()?);

    let package = workspace
        .resolution
        .packages
        .iter()
        .find(|resolved| {
            resolved.alias == alias
                && matches!(
                    &resolved.source,
                    PackageSource::Git { url: resolved_url, .. } if normalize_git_url(resolved_url) == url
                )
        })
        .cloned()
        .ok_or_else(|| anyhow!("dependency `{alias}` is missing from the current lockfile state"))?;
    let snapshot_root = workspace
        .snapshot_roots
        .get(&package.digest)
        .cloned()
        .ok_or_else(|| anyhow!("missing snapshot for dependency `{alias}`"))?;

    Ok(DependencyContext {
        alias,
        url,
        package,
        snapshot_root,
    })
}

fn resolve_linked_repo(
    project_root: &Path,
    local_config: &mut LocalConfig,
    dependency: &DependencyContext,
    repo_path_override: Option<&Path>,
    via_override: Option<Adapter>,
    execution_mode: ExecutionMode,
) -> Result<PathBuf> {
    match repo_path_override {
        Some(path) => {
            let linked_repo = canonicalize_existing_dir(path)?;
            validate_linked_repo(&linked_repo, &dependency.url)?;
            let existing_via = local_config
                .relay_link(&dependency.alias)
                .and_then(|link| link.via);
            local_config.set_relay_link(
                dependency.alias.clone(),
                RelayLink {
                    repo_path: linked_repo.clone(),
                    url: dependency.url.clone(),
                    via: via_override.or(existing_via),
                },
            );
            if !execution_mode.is_dry_run() {
                local_config.save_in_dir(project_root)?;
            }
            Ok(linked_repo)
        }
        None => {
            let linked_repo = resolve_existing_link(local_config, dependency)?;
            if let Some(via) = via_override {
                let existing = local_config.relay_link(&dependency.alias).ok_or_else(|| {
                    anyhow!(
                        "no relay link configured for `{}`; rerun with `--repo-path <path>`",
                        dependency.alias
                    )
                })?;
                if existing.via != Some(via) {
                    local_config.set_relay_link(
                        dependency.alias.clone(),
                        RelayLink {
                            repo_path: existing.repo_path.clone(),
                            url: existing.url.clone(),
                            via: Some(via),
                        },
                    );
                    if !execution_mode.is_dry_run() {
                        local_config.save_in_dir(project_root)?;
                    }
                }
            }
            Ok(linked_repo)
        }
    }
}

fn relay_link_would_change(
    local_config: &LocalConfig,
    alias: &str,
    repo_path_override: Option<&Path>,
    via_override: Option<Adapter>,
    url: &str,
) -> Result<bool> {
    let Some(repo_path_override) = repo_path_override else {
        return Ok(via_override.is_some_and(|via| {
            local_config
                .relay_link(alias)
                .is_some_and(|link| link.via != Some(via))
        }));
    };

    let linked_repo = canonicalize_existing_dir(repo_path_override)?;
    let existing = local_config.relay_link(alias);
    let next_via = via_override.or(existing.and_then(|link| link.via));

    Ok(existing.is_none_or(|link| {
        link.repo_path != linked_repo || link.url != url || link.via != next_via
    }))
}

fn resolve_existing_link(
    local_config: &LocalConfig,
    dependency: &DependencyContext,
) -> Result<PathBuf> {
    let link = local_config.relay_link(&dependency.alias).ok_or_else(|| {
        anyhow!(
            "no relay link configured for `{}`; rerun with `--repo-path <path>`",
            dependency.alias
        )
    })?;
    let linked_repo = canonicalize_existing_dir(&link.repo_path)?;
    validate_linked_repo(&linked_repo, &dependency.url)?;
    Ok(linked_repo)
}

fn validate_linked_repo(path: &Path, url: &str) -> Result<()> {
    if !is_git_repository(path) {
        bail!("linked repo {} is not a git repository", path.display());
    }
    let origin = repository_origin_url(path).with_context(|| {
        format!(
            "linked repo {} is missing an `origin` remote",
            path.display()
        )
    })?;
    if !git_urls_match(&origin, url) {
        bail!(
            "linked repo {} has origin `{}` instead of `{}`",
            path.display(),
            origin,
            url
        );
    }
    Ok(())
}

fn build_relay_plan(
    dependency: &DependencyContext,
    project_root: &Path,
    selected_adapters: Adapters,
    linked_repo: &Path,
) -> Result<RelayPlan> {
    let mappings = build_mappings(dependency, project_root, selected_adapters, linked_repo)?;
    let mut grouped = BTreeMap::<PathBuf, Vec<RelayFileMapping>>::new();
    for mapping in mappings {
        grouped
            .entry(mapping.linked_source_path.clone())
            .or_default()
            .push(mapping);
    }

    let mut plan = RelayPlan::default();
    for (linked_source_path, group) in grouped {
        let mut candidate_source: Option<Vec<u8>> = None;
        let linked_current = fs::read(&linked_source_path).ok();
        let mut linked_changed = false;

        for mapping in group {
            let baseline_source = fs::read(&mapping.snapshot_path).with_context(|| {
                format!(
                    "failed to read relay baseline {}",
                    mapping.snapshot_path.display()
                )
            })?;
            let baseline_managed = mapping.transform.to_managed_bytes(&baseline_source)?;
            let current_managed = match fs::read(&mapping.managed_path) {
                Ok(contents) => contents,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // Missing generated outputs do not encode a source edit we can relay.
                    // Let sync regenerate them instead of blocking on a synthetic conflict.
                    continue;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to read managed file {}",
                            mapping.managed_path.display()
                        )
                    });
                }
            };
            if current_managed == baseline_managed {
                continue;
            }

            let candidate = mapping
                .transform
                .to_source_bytes(&current_managed, &baseline_source)?;
            if linked_current.as_deref() != Some(baseline_source.as_slice()) {
                linked_changed = true;
            }
            if let Some(existing) = &candidate_source {
                if existing != &candidate {
                    plan.conflicts.push(format!(
                        "managed variants for {} disagree on relayed contents",
                        linked_source_path.display()
                    ));
                    continue;
                }
            } else {
                candidate_source = Some(candidate);
            }
        }

        let Some(candidate_source) = candidate_source else {
            continue;
        };
        if linked_current.as_deref() == Some(candidate_source.as_slice()) {
            plan.noops.insert(linked_source_path);
            continue;
        }
        if linked_changed {
            plan.conflicts.push(format!(
                "{} changed in both managed outputs and linked source",
                linked_source_path.display()
            ));
            continue;
        }
        plan.updates.insert(linked_source_path, candidate_source);
    }

    Ok(plan)
}

fn watch_dependency_in_dir_with_options(
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

fn watch_dependencies_in_dir_with_options(
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
        state = capture_watch_state(project_root, cache_root, packages, reporter)?;
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

fn build_mappings(
    dependency: &DependencyContext,
    project_root: &Path,
    selected_adapters: Adapters,
    linked_repo: &Path,
) -> Result<Vec<RelayFileMapping>> {
    let mut mappings = Vec::new();
    let package = &dependency.package;
    let snapshot_root = &dependency.snapshot_root;

    for skill in &package.manifest.discovered.skills {
        if !package.selects_component(crate::manifest::DependencyComponent::Skills) {
            continue;
        }

        for adapter in [
            Adapter::Agents,
            Adapter::Claude,
            Adapter::Cursor,
            Adapter::OpenCode,
        ] {
            if !selected_adapters.contains(adapter) {
                continue;
            }
            let source_root = snapshot_root.join(&skill.path);
            let managed_root = managed_skill_root(project_root, adapter, package, &skill.id);
            let target_root = linked_repo.join(&skill.path);
            mappings.extend(skill_mappings(
                adapter,
                package,
                skill,
                snapshot_root,
                &source_root,
                &target_root,
                &managed_root,
            )?);
        }
    }

    for agent in &package.manifest.discovered.agents {
        if !package.selects_component(crate::manifest::DependencyComponent::Agents) {
            continue;
        }
        for adapter in [Adapter::Claude, Adapter::OpenCode] {
            if !selected_adapters.contains(adapter) {
                continue;
            }
            if let Some(managed_path) = managed_artifact_path(
                project_root,
                adapter,
                ArtifactKind::Agent,
                package,
                &agent.id,
            ) {
                mappings.push(file_mapping(
                    managed_path,
                    snapshot_root.join(&agent.path),
                    linked_repo.join(&agent.path),
                    RelayTransform::None,
                ));
            }
        }
    }

    for rule in &package.manifest.discovered.rules {
        if !package.selects_component(crate::manifest::DependencyComponent::Rules) {
            continue;
        }
        for (adapter, kind) in [
            (Adapter::Claude, ArtifactKind::Rule),
            (Adapter::Codex, ArtifactKind::Rule),
            (Adapter::Cursor, ArtifactKind::Rule),
            (Adapter::OpenCode, ArtifactKind::Rule),
        ] {
            if !selected_adapters.contains(adapter) {
                continue;
            }
            if let Some(managed_path) =
                managed_artifact_path(project_root, adapter, kind, package, &rule.id)
            {
                mappings.push(file_mapping(
                    managed_path,
                    snapshot_root.join(&rule.path),
                    linked_repo.join(&rule.path),
                    RelayTransform::None,
                ));
            }
        }
    }

    for command in &package.manifest.discovered.commands {
        if !package.selects_component(crate::manifest::DependencyComponent::Commands) {
            continue;
        }
        for (adapter, kind) in [
            (Adapter::Agents, ArtifactKind::Command),
            (Adapter::Claude, ArtifactKind::Command),
            (Adapter::Cursor, ArtifactKind::Command),
            (Adapter::OpenCode, ArtifactKind::Command),
        ] {
            if !selected_adapters.contains(adapter) {
                continue;
            }
            if let Some(managed_path) =
                managed_artifact_path(project_root, adapter, kind, package, &command.id)
            {
                mappings.push(file_mapping(
                    managed_path,
                    snapshot_root.join(&command.path),
                    linked_repo.join(&command.path),
                    RelayTransform::None,
                ));
            }
        }
    }

    for mapping in package.direct_managed_paths() {
        for file in &mapping.files {
            mappings.push(file_mapping(
                project_root.join(&file.target_relative),
                snapshot_root.join(&file.source_relative),
                linked_repo.join(&file.source_relative),
                RelayTransform::None,
            ));
        }
    }

    Ok(mappings)
}

fn skill_mappings(
    adapter: Adapter,
    package: &ResolvedPackage,
    skill: &SkillEntry,
    snapshot_root: &Path,
    source_root: &Path,
    linked_root: &Path,
    managed_root: &Path,
) -> Result<Vec<RelayFileMapping>> {
    let mut mappings = Vec::new();
    for entry in walkdir::WalkDir::new(source_root) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(source_root)
            .with_context(|| format!("failed to make {} relative", entry.path().display()))?;
        let transform = if adapter == Adapter::OpenCode && relative == Path::new("SKILL.md") {
            RelayTransform::OpenCodeSkillName {
                managed_skill_id: crate::adapters::namespaced_skill_id(package, &skill.id),
            }
        } else {
            RelayTransform::None
        };
        mappings.push(file_mapping(
            managed_root.join(relative),
            snapshot_root.join(&skill.path).join(relative),
            linked_root.join(relative),
            transform,
        ));
    }
    Ok(mappings)
}

fn file_mapping(
    managed_path: PathBuf,
    snapshot_path: PathBuf,
    linked_source_path: PathBuf,
    transform: RelayTransform,
) -> RelayFileMapping {
    RelayFileMapping {
        managed_path,
        snapshot_path,
        linked_source_path,
        transform,
    }
}

impl RelayTransform {
    fn to_managed_bytes(&self, source: &[u8]) -> Result<Vec<u8>> {
        match self {
            Self::None => Ok(source.to_vec()),
            Self::OpenCodeSkillName { managed_skill_id } => {
                crate::adapters::opencode::rewrite_skill_name(source, managed_skill_id)
            }
        }
    }

    fn to_source_bytes(&self, managed: &[u8], baseline_source: &[u8]) -> Result<Vec<u8>> {
        match self {
            Self::None => Ok(managed.to_vec()),
            Self::OpenCodeSkillName { managed_skill_id } => {
                restore_opencode_skill_name(managed, baseline_source, managed_skill_id)
            }
        }
    }
}

fn restore_opencode_skill_name(
    managed: &[u8],
    baseline_source: &[u8],
    managed_skill_id: &str,
) -> Result<Vec<u8>> {
    let managed =
        String::from_utf8(managed.to_vec()).context("OpenCode managed skills must be UTF-8")?;
    let baseline_source = String::from_utf8(baseline_source.to_vec())
        .context("OpenCode source skills must be UTF-8")?;
    let restored_name = extract_frontmatter_name(&baseline_source)?;
    let mut lines = split_lines_preserving_endings(&managed);
    let Some(index) = lines
        .iter()
        .position(|line| trim_line_ending(line).trim_start() == format!("name: {managed_skill_id}"))
        .or_else(|| {
            lines
                .iter()
                .position(|line| trim_line_ending(line).trim_start().starts_with("name:"))
        })
    else {
        bail!("OpenCode managed skill is missing a frontmatter `name`");
    };
    lines[index] = rewrite_frontmatter_name_line(&lines[index], &restored_name);
    Ok(lines.concat().into_bytes())
}

fn extract_frontmatter_name(contents: &str) -> Result<String> {
    let lines = contents.lines().collect::<Vec<_>>();
    if lines.first().copied() != Some("---") {
        bail!("OpenCode skill is missing YAML frontmatter");
    }
    let Some(frontmatter_end) = lines.iter().skip(1).position(|line| *line == "---") else {
        bail!("OpenCode skill is missing a closing frontmatter fence");
    };
    let frontmatter_end = frontmatter_end + 1;
    for line in lines.iter().take(frontmatter_end) {
        if let Some(value) = line.trim_start().strip_prefix("name:") {
            return Ok(value.trim().to_string());
        }
    }
    bail!("OpenCode skill is missing a frontmatter `name`")
}

fn split_lines_preserving_endings(contents: &str) -> Vec<String> {
    if contents.is_empty() {
        Vec::new()
    } else {
        contents.split_inclusive('\n').map(str::to_string).collect()
    }
}

fn trim_line_ending(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

fn rewrite_frontmatter_name_line(line: &str, name: &str) -> String {
    let leading = line
        .chars()
        .take_while(|character| character.is_ascii_whitespace())
        .collect::<String>();
    let newline = if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    };

    format!("{leading}name: {name}{newline}")
}

fn canonicalize_existing_dir(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to access {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("{} is not a directory", canonical.display());
    }
    Ok(canonical)
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{self, Write};
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::adapters::managed_artifact_path;
    use crate::git::{AddDependencyOptions, add_dependency_in_dir_with_adapters};
    use crate::report::ColorMode;

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::File::create(path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }

    fn append_file(path: &Path, suffix: &str) {
        let mut contents = fs::read_to_string(path).unwrap();
        contents.push_str(suffix);
        fs::write(path, contents).unwrap();
    }

    fn run_git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_git_repo(path: &Path) {
        run_git(path, &["init"]);
        run_git(path, &["config", "user.email", "test@example.com"]);
        run_git(path, &["config", "user.name", "Test User"]);
        run_git(path, &["config", "core.autocrlf", "false"]);
        write_file(&path.join(".gitattributes"), "* text eol=lf\n");
        run_git(path, &["add", "."]);
        run_git(path, &["commit", "-m", "initial"]);
    }

    fn toml_path_value(path: &Path) -> String {
        crate::paths::display_path(path)
    }

    fn create_remote_dependency_named(name: &str) -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path().join(name);
        fs::create_dir_all(&repo_path).unwrap();
        write_file(
            &repo_path.join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Example.\n---\n# Review\n",
        );
        write_file(&repo_path.join("agents/security.md"), "# Security\n");
        write_file(&repo_path.join("rules/policy.md"), "Be careful.\n");
        write_file(&repo_path.join("commands/build.md"), "# Build\n");
        write_file(&repo_path.join("prompts/review.md"), "Review prompt.\n");
        write_file(&repo_path.join("templates/checklist.md"), "Checklist.\n");
        write_file(&repo_path.join("templates/nested/tips.md"), "Tips.\n");
        init_git_repo(&repo_path);
        run_git(&repo_path, &["tag", "v0.1.0"]);
        (temp, repo_path)
    }

    fn create_remote_dependency() -> (TempDir, PathBuf) {
        create_remote_dependency_named("playbook-ios")
    }

    fn clone_linked_repo(remote: &Path) -> TempDir {
        let linked = TempDir::new().unwrap();
        let target = linked.path().join("linked");
        let output = Command::new("git")
            .args([
                "clone",
                remote.to_string_lossy().as_ref(),
                target.to_string_lossy().as_ref(),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        linked
    }

    fn install_dependency(project: &Path, cache: &Path, remote: &Path, adapters: &[Adapter]) {
        let reporter = Reporter::silent();
        add_dependency_in_dir_with_adapters(
            project,
            cache,
            &remote.to_string_lossy(),
            AddDependencyOptions {
                git_ref: Some(crate::manifest::RequestedGitRef::Tag("v0.1.0")),
                adapters,
                components: &[],
                sync_on_launch: false,
            },
            &reporter,
        )
        .unwrap();
    }

    fn sync_project(project: &Path, cache: &Path, adapters: &[Adapter]) {
        crate::resolver::sync_in_dir_with_adapters(
            project,
            cache,
            false,
            false,
            adapters,
            false,
            &Reporter::silent(),
        )
        .unwrap();
    }

    fn resolved_package(project: &Path, cache: &Path, adapters: &[Adapter]) -> ResolvedPackage {
        let reporter = Reporter::silent();
        let (resolution, _) = resolve_project_from_existing_lockfile_in_dir(
            project,
            cache,
            Adapters::from_slice(adapters),
            &reporter,
        )
        .unwrap();
        resolution
            .packages
            .into_iter()
            .find(|package| package.alias == "playbook_ios")
            .unwrap()
    }

    #[test]
    fn relay_writes_back_edits_for_all_adapters_and_preserves_opencode_name() {
        let (_remote_root, remote_repo) = create_remote_dependency();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        install_dependency(project.path(), cache.path(), &remote_repo, &Adapter::ALL);

        let package = resolved_package(project.path(), cache.path(), &Adapter::ALL);
        let skill_suffix = "\nRelay skill update.\n";
        for adapter in [
            Adapter::Agents,
            Adapter::Claude,
            Adapter::Cursor,
            Adapter::OpenCode,
        ] {
            append_file(
                &managed_skill_root(project.path(), adapter, &package, "review").join("SKILL.md"),
                skill_suffix,
            );
        }
        let agent_suffix = "\nRelay agent update.\n";
        for adapter in [Adapter::Claude, Adapter::OpenCode] {
            append_file(
                &managed_artifact_path(
                    project.path(),
                    adapter,
                    ArtifactKind::Agent,
                    &package,
                    "security",
                )
                .unwrap(),
                agent_suffix,
            );
        }
        let rule_suffix = "\nRelay rule update.\n";
        for adapter in [
            Adapter::Claude,
            Adapter::Codex,
            Adapter::Cursor,
            Adapter::OpenCode,
        ] {
            append_file(
                &managed_artifact_path(
                    project.path(),
                    adapter,
                    ArtifactKind::Rule,
                    &package,
                    "policy",
                )
                .unwrap(),
                rule_suffix,
            );
        }
        let command_suffix = "\nRelay command update.\n";
        for adapter in [
            Adapter::Agents,
            Adapter::Claude,
            Adapter::Cursor,
            Adapter::OpenCode,
        ] {
            append_file(
                &managed_artifact_path(
                    project.path(),
                    adapter,
                    ArtifactKind::Command,
                    &package,
                    "build",
                )
                .unwrap(),
                command_suffix,
            );
        }

        let summary = relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            None,
            &Reporter::silent(),
        )
        .unwrap();

        assert_eq!(summary.alias, "playbook_ios");
        assert_eq!(summary.updated_file_count, 4);
        let relayed_skill = fs::read_to_string(linked_repo.join("skills/review/SKILL.md")).unwrap();
        assert!(relayed_skill.contains("name: Review"));
        assert!(!relayed_skill.contains("name: review_"));
        assert!(relayed_skill.ends_with(skill_suffix));
        assert!(
            fs::read_to_string(linked_repo.join("agents/security.md"))
                .unwrap()
                .ends_with(agent_suffix)
        );
        assert!(
            fs::read_to_string(linked_repo.join("rules/policy.md"))
                .unwrap()
                .ends_with(rule_suffix)
        );
        assert!(
            fs::read_to_string(linked_repo.join("commands/build.md"))
                .unwrap()
                .ends_with(command_suffix)
        );

        let local_config = LocalConfig::load_in_dir(project.path()).unwrap();
        let link = local_config.relay_link("playbook_ios").unwrap();
        assert_eq!(link.repo_path, linked_repo.canonicalize().unwrap());
        assert_eq!(link.via, None);
    }

    #[test]
    fn relay_persists_via_hint() {
        let (_remote_root, remote_repo) = create_remote_dependency();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        install_dependency(
            project.path(),
            cache.path(),
            &remote_repo,
            &[Adapter::Claude, Adapter::Codex],
        );

        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            Some(Adapter::Claude),
            &Reporter::silent(),
        )
        .unwrap();

        let local_config = LocalConfig::load_in_dir(project.path()).unwrap();
        let link = local_config.relay_link("playbook_ios").unwrap();
        assert_eq!(link.repo_path, linked_repo.canonicalize().unwrap());
        assert_eq!(link.via, Some(Adapter::Claude));
    }

    #[test]
    fn relay_writes_back_direct_managed_file_and_directory_edits() {
        let (_remote_root, remote_repo) = create_remote_dependency();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        write_file(
            &project.path().join("nodus.toml"),
            &format!(
                r#"
[adapters]
enabled = ["codex"]

[dependencies.playbook_ios]
url = "{}"
tag = "v0.1.0"

[[dependencies.playbook_ios.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"

[[dependencies.playbook_ios.managed]]
source = "templates"
target = "docs/templates"
"#,
                toml_path_value(&remote_repo)
            ),
        );

        sync_project(project.path(), cache.path(), &[Adapter::Codex]);

        append_file(
            &project.path().join(".github/prompts/review.md"),
            "\nRelay prompt update.\n",
        );
        append_file(
            &project.path().join("docs/templates/checklist.md"),
            "\nRelay checklist update.\n",
        );
        append_file(
            &project.path().join("docs/templates/nested/tips.md"),
            "\nRelay tips update.\n",
        );

        let summary = relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            None,
            &Reporter::silent(),
        )
        .unwrap();

        assert_eq!(summary.updated_file_count, 3);
        assert!(
            fs::read_to_string(linked_repo.join("prompts/review.md"))
                .unwrap()
                .ends_with("\nRelay prompt update.\n")
        );
        assert!(
            fs::read_to_string(linked_repo.join("templates/checklist.md"))
                .unwrap()
                .ends_with("\nRelay checklist update.\n")
        );
        assert!(
            fs::read_to_string(linked_repo.join("templates/nested/tips.md"))
                .unwrap()
                .ends_with("\nRelay tips update.\n")
        );
    }

    #[test]
    fn relay_rejects_direct_managed_double_edits() {
        let (_remote_root, remote_repo) = create_remote_dependency();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        write_file(
            &project.path().join("nodus.toml"),
            &format!(
                r#"
[adapters]
enabled = ["codex"]

[dependencies.playbook_ios]
url = "{}"
tag = "v0.1.0"

[[dependencies.playbook_ios.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
                toml_path_value(&remote_repo)
            ),
        );

        sync_project(project.path(), cache.path(), &[Adapter::Codex]);
        append_file(
            &project.path().join(".github/prompts/review.md"),
            "\nManaged prompt change.\n",
        );
        append_file(
            &linked_repo.join("prompts/review.md"),
            "\nLinked prompt change.\n",
        );

        let error = relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            None,
            &Reporter::silent(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("changed in both managed outputs and linked source"));
    }

    #[test]
    fn relay_rejects_when_managed_variants_disagree() {
        let (_remote_root, remote_repo) = create_remote_dependency();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        install_dependency(
            project.path(),
            cache.path(),
            &remote_repo,
            &[Adapter::Claude, Adapter::Codex],
        );

        let package = resolved_package(
            project.path(),
            cache.path(),
            &[Adapter::Claude, Adapter::Codex],
        );
        append_file(
            &managed_artifact_path(
                project.path(),
                Adapter::Claude,
                ArtifactKind::Rule,
                &package,
                "policy",
            )
            .unwrap(),
            "\nClaude change.\n",
        );
        append_file(
            &managed_artifact_path(
                project.path(),
                Adapter::Codex,
                ArtifactKind::Rule,
                &package,
                "policy",
            )
            .unwrap(),
            "\nCodex change.\n",
        );

        let error = relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            None,
            &Reporter::silent(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("disagree on relayed contents"));
    }

    #[test]
    fn relay_requires_a_persisted_or_explicit_repo_path() {
        let (_remote_root, remote_repo) = create_remote_dependency();
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        install_dependency(
            project.path(),
            cache.path(),
            &remote_repo,
            &[Adapter::Claude],
        );

        let error = relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            None,
            None,
            &Reporter::silent(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("--repo-path <path>"));
    }

    #[test]
    fn relay_rejects_path_dependencies() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        write_file(
            &project.path().join("nodus.toml"),
            r#"
[adapters]
enabled = ["codex"]

[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
        );
        write_file(
            &project.path().join("vendor/shared/skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Example.\n---\n# Review\n",
        );
        write_file(
            &project.path().join("vendor/shared/prompts/review.md"),
            "Review prompt.\n",
        );

        sync_project(project.path(), cache.path(), &[Adapter::Codex]);

        let error = relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "shared",
            Some(&project.path().join("vendor/shared")),
            None,
            &Reporter::silent(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("path dependency"));
    }

    #[test]
    fn pending_relay_edits_block_sync_update_and_remove() {
        let (_remote_root, remote_repo) = create_remote_dependency();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        install_dependency(
            project.path(),
            cache.path(),
            &remote_repo,
            &[Adapter::Claude],
        );

        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            None,
            &Reporter::silent(),
        )
        .unwrap();

        let package = resolved_package(project.path(), cache.path(), &[Adapter::Claude]);
        append_file(
            &managed_skill_root(project.path(), Adapter::Claude, &package, "review")
                .join("SKILL.md"),
            "\nPending relay change.\n",
        );

        let sync_error = crate::resolver::sync_in_dir_with_adapters(
            project.path(),
            cache.path(),
            false,
            false,
            &[],
            false,
            &Reporter::silent(),
        )
        .unwrap_err()
        .to_string();
        assert!(sync_error.contains("pending relay edits"));

        let update_error = crate::update::update_direct_dependencies_in_dir(
            project.path(),
            cache.path(),
            false,
            &Reporter::silent(),
        )
        .unwrap_err()
        .to_string();
        assert!(update_error.contains("pending relay edits"));

        let remove_error = crate::git::remove_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            &Reporter::silent(),
        )
        .unwrap_err()
        .to_string();
        assert!(remove_error.contains("pending relay edits"));
    }

    #[test]
    fn missing_managed_variants_do_not_block_sync_with_relay() {
        let (_remote_root, remote_repo) = create_remote_dependency();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        install_dependency(
            project.path(),
            cache.path(),
            &remote_repo,
            &[Adapter::Claude, Adapter::Codex],
        );

        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            None,
            &Reporter::silent(),
        )
        .unwrap();

        fs::remove_dir_all(project.path().join(".codex")).unwrap();

        crate::resolver::sync_in_dir_with_adapters(
            project.path(),
            cache.path(),
            false,
            false,
            &[],
            false,
            &Reporter::silent(),
        )
        .unwrap();

        let package = resolved_package(
            project.path(),
            cache.path(),
            &[Adapter::Claude, Adapter::Codex],
        );
        assert!(
            managed_artifact_path(
                project.path(),
                Adapter::Codex,
                ArtifactKind::Rule,
                &package,
                "policy",
            )
            .unwrap()
            .exists()
        );
    }

    #[test]
    fn adapter_expansion_does_not_block_sync_with_existing_relay() {
        let (_remote_root, remote_repo) = create_remote_dependency();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        install_dependency(
            project.path(),
            cache.path(),
            &remote_repo,
            &[Adapter::Claude],
        );
        let loaded = crate::manifest::load_root_from_dir(project.path()).unwrap();
        let dependency = loaded.manifest.dependencies.get("playbook_ios").unwrap();
        let source_spec = if let Some(github) = &dependency.github {
            format!("github = {:?}", github)
        } else if let Some(url) = &dependency.url {
            format!("url = {:?}", url)
        } else {
            panic!("expected git dependency source");
        };
        let tag = dependency.tag.as_deref().unwrap_or("v0.1.0");

        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            None,
            &Reporter::silent(),
        )
        .unwrap();

        write_file(
            &project.path().join("nodus.toml"),
            &format!(
                r#"
[adapters]
enabled = ["claude", "codex"]

[dependencies.playbook_ios]
{source_spec}
tag = {:?}
"#,
                tag
            ),
        );

        crate::resolver::sync_in_dir_with_adapters(
            project.path(),
            cache.path(),
            false,
            false,
            &[],
            false,
            &Reporter::silent(),
        )
        .unwrap();

        let package = resolved_package(
            project.path(),
            cache.path(),
            &[Adapter::Claude, Adapter::Codex],
        );
        assert!(
            managed_artifact_path(
                project.path(),
                Adapter::Codex,
                ArtifactKind::Rule,
                &package,
                "policy",
            )
            .unwrap()
            .exists()
        );
    }

    #[test]
    fn stale_lockfile_does_not_block_sync_relay_preflight_without_pending_edits() {
        let (_remote_root, remote_repo) = create_remote_dependency();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        install_dependency(
            project.path(),
            cache.path(),
            &remote_repo,
            &[Adapter::Codex],
        );

        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            None,
            &Reporter::silent(),
        )
        .unwrap();

        write_file(
            &project.path().join("skills/root-review/SKILL.md"),
            "---\nname: Root Review\ndescription: Example.\n---\n# Root Review\n",
        );

        crate::resolver::sync_in_dir_with_adapters(
            project.path(),
            cache.path(),
            false,
            false,
            &[],
            false,
            &Reporter::silent(),
        )
        .unwrap();
    }

    #[test]
    fn missing_lockfile_does_not_block_sync_with_relay_links() {
        let (_remote_root, remote_repo) = create_remote_dependency();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        install_dependency(
            project.path(),
            cache.path(),
            &remote_repo,
            &[Adapter::Codex],
        );

        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            None,
            &Reporter::silent(),
        )
        .unwrap();

        fs::remove_file(project.path().join(LOCKFILE_NAME)).unwrap();

        crate::resolver::sync_in_dir_with_adapters(
            project.path(),
            cache.path(),
            false,
            false,
            &[],
            false,
            &Reporter::silent(),
        )
        .unwrap();

        assert!(project.path().join(LOCKFILE_NAME).exists());
    }

    #[test]
    fn relay_watch_syncs_follow_up_managed_edits() {
        let (_remote_root, remote_repo) = create_remote_dependency();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        install_dependency(
            project.path(),
            cache.path(),
            &remote_repo,
            &[Adapter::Claude],
        );

        let package = resolved_package(project.path(), cache.path(), &[Adapter::Claude]);
        let managed_skill = managed_skill_root(project.path(), Adapter::Claude, &package, "review")
            .join("SKILL.md");

        let project_root = project.path().to_path_buf();
        let cache_root = cache.path().to_path_buf();
        let linked_repo_for_watch = linked_repo.clone();
        let output = SharedBuffer::default();
        let output_for_watch = output.clone();
        let watch_handle = thread::spawn(move || {
            watch_dependency_in_dir_with_options(
                &project_root,
                &cache_root,
                "playbook_ios",
                Some(&linked_repo_for_watch),
                None,
                &Reporter::sink(ColorMode::Never, output_for_watch),
                RelayWatchOptions {
                    poll_interval: Duration::from_millis(20),
                    max_events: Some(2),
                    max_polls: Some(200),
                },
            )
            .unwrap()
        });

        let mut ready = false;
        for _ in 0..500 {
            if output
                .contents()
                .contains("watching managed outputs for changes")
            {
                ready = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(ready, "watcher never reported readiness");
        append_file(&managed_skill, "\nWatched relay update.\n");

        let summaries = watch_handle.join().unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[1].updated_file_count, 1);
        assert!(
            fs::read_to_string(linked_repo.join("skills/review/SKILL.md"))
                .unwrap()
                .ends_with("\nWatched relay update.\n")
        );
    }

    #[test]
    fn relay_watch_syncs_follow_up_managed_edits_for_multiple_dependencies() {
        let (_remote_root_one, remote_repo_one) = create_remote_dependency_named("playbook-ios");
        let (_remote_root_two, remote_repo_two) = create_remote_dependency_named("docs-kit");
        append_file(
            &remote_repo_two.join("skills/review/SKILL.md"),
            "\nDocs baseline.\n",
        );
        run_git(&remote_repo_two, &["add", "."]);
        run_git(&remote_repo_two, &["commit", "-m", "docs baseline"]);
        run_git(&remote_repo_two, &["tag", "v0.2.0"]);

        let linked_one = clone_linked_repo(&remote_repo_one);
        let linked_two = clone_linked_repo(&remote_repo_two);
        let linked_repo_one = linked_one.path().join("linked");
        let linked_repo_two = linked_two.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        install_dependency(
            project.path(),
            cache.path(),
            &remote_repo_one,
            &[Adapter::Claude],
        );
        let reporter = Reporter::silent();
        add_dependency_in_dir_with_adapters(
            project.path(),
            cache.path(),
            &remote_repo_two.to_string_lossy(),
            AddDependencyOptions {
                git_ref: Some(crate::manifest::RequestedGitRef::Tag("v0.2.0")),
                adapters: &[Adapter::Claude],
                components: &[],
                sync_on_launch: false,
            },
            &reporter,
        )
        .unwrap();

        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo_one),
            None,
            &Reporter::silent(),
        )
        .unwrap();
        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "docs_kit",
            Some(&linked_repo_two),
            None,
            &Reporter::silent(),
        )
        .unwrap();

        let package_one = resolved_package(project.path(), cache.path(), &[Adapter::Claude]);
        let managed_skill_one =
            managed_skill_root(project.path(), Adapter::Claude, &package_one, "review")
                .join("SKILL.md");
        let output = SharedBuffer::default();
        let output_for_watch = output.clone();
        let project_root = project.path().to_path_buf();
        let cache_root = cache.path().to_path_buf();
        let watch_packages = vec!["playbook_ios".to_string(), "docs_kit".to_string()];
        let watch_handle = thread::spawn(move || {
            watch_dependencies_in_dir_with_options(
                &project_root,
                &cache_root,
                &watch_packages,
                None,
                &Reporter::sink(ColorMode::Never, output_for_watch),
                RelayWatchOptions {
                    poll_interval: Duration::from_millis(20),
                    max_events: Some(3),
                    max_polls: Some(200),
                },
            )
            .unwrap()
        });

        let mut ready = false;
        for _ in 0..500 {
            if output
                .contents()
                .contains("watching managed outputs for changes")
            {
                ready = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(ready, "watcher never reported readiness");
        append_file(&managed_skill_one, "\nWatched relay update.\n");

        let summaries = watch_handle.join().unwrap();
        assert_eq!(summaries.len(), 3);
        assert_eq!(summaries[2].alias, "playbook_ios");
        assert_eq!(summaries[2].updated_file_count, 1);
        assert!(
            fs::read_to_string(linked_repo_one.join("skills/review/SKILL.md"))
                .unwrap()
                .ends_with("\nWatched relay update.\n")
        );
        assert!(
            fs::read_to_string(linked_repo_two.join("skills/review/SKILL.md"))
                .unwrap()
                .ends_with("\nDocs baseline.\n")
        );
    }

    #[test]
    fn restore_opencode_skill_name_preserves_crlf() {
        let managed = b"---\r\nname: review_abcd12\r\ndescription: Example.\r\n---\r\n# Review\r\n";
        let baseline = b"---\r\nname: Review\r\ndescription: Example.\r\n---\r\n# Review\r\n";

        let restored = restore_opencode_skill_name(managed, baseline, "review_abcd12").unwrap();
        let restored = String::from_utf8(restored).unwrap();

        assert!(restored.contains("name: Review\r\n"));
        assert!(restored.contains("description: Example.\r\n"));
        assert!(restored.ends_with("\r\n"));
    }
}
