mod mappings;
mod watch;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use crate::hashing::blake3_hex;
use anyhow::{Context, Result, anyhow, bail};

use self::watch::{
    RelayWatchInvocation, RelayWatchOptions, watch_dependencies_in_dir_with_options,
    watch_dependency_in_dir_with_options,
};
use crate::adapters::{Adapter, Adapters, ManagedArtifactNames};
use crate::execution::{ExecutionMode, PreviewChange};
use crate::git::{
    git_urls_match, is_git_repository, normalize_git_url, repository_origin_url,
    resolve_dependency_alias,
};
use crate::local_config::{LocalConfig, RelayLink, RelayedFileState, config_path, local_dir};
use crate::lockfile::{LOCKFILE_NAME, Lockfile};
use crate::manifest::{DependencySourceKind, load_root_from_dir};
use crate::paths::{canonicalize_path, display_path, strip_path_prefix};
use crate::report::Reporter;
use crate::resolver::{
    PackageSource, ResolvedPackage, resolve_project_from_existing_lockfile_in_dir,
};
use crate::store::snapshot_packages;

#[derive(Debug, Clone)]
pub struct RelaySummary {
    pub alias: String,
    pub linked_repo: PathBuf,
    pub created_file_count: usize,
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
    snapshot_path: Option<PathBuf>,
    linked_source_path: PathBuf,
    artifact_id: String,
    transform: RelayTransform,
}

#[derive(Debug, Clone)]
enum RelayTransform {
    None,
    OpenCodeSkillName {
        managed_skill_id: String,
    },
    CopilotSkillName {
        managed_skill_id: String,
    },
    CodexAgentToml {
        rewritten_name: Option<String>,
    },
    CodexAgentMarkdown {
        runtime_name: String,
        description: String,
    },
    CodexCommandSkill {
        managed_skill_id: String,
        source_command_id: String,
    },
    MarkdownAgentToml {
        adapter_name: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayOperationKind {
    Create,
    Update,
}

#[derive(Debug, Clone)]
struct RelayWrite {
    kind: RelayOperationKind,
    contents: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
struct RelayPlan {
    writes: BTreeMap<PathBuf, RelayWrite>,
    noops: BTreeSet<PathBuf>,
    state_files: BTreeMap<String, RelayedFileState>,
    conflicts: Vec<String>,
}

#[derive(Debug, Clone)]
struct RelayJobPlan {
    dependency: DependencyContext,
    linked_repo: PathBuf,
    relay_link: RelayLink,
    plan: RelayPlan,
}

#[derive(Debug, Clone)]
struct PlannedLinkedSourcePath {
    relative_path: String,
    linked_source_path: PathBuf,
    write: Option<RelayWrite>,
    noop: bool,
    state_file: Option<RelayedFileState>,
    conflicts: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct RelayExecution<'a> {
    repo_path_override: Option<&'a Path>,
    via_override: Option<Adapter>,
    create_missing: bool,
    execution_mode: ExecutionMode,
}

pub fn relay_dependency_in_dir(
    project_root: &Path,
    cache_root: &Path,
    package: &str,
    repo_path_override: Option<&Path>,
    via_override: Option<Adapter>,
    create_missing: bool,
    reporter: &Reporter,
) -> Result<RelaySummary> {
    let packages = vec![package.to_string()];
    let mut summaries = relay_dependencies_in_dir_mode(
        project_root,
        cache_root,
        &packages,
        RelayExecution {
            repo_path_override,
            via_override,
            create_missing,
            execution_mode: ExecutionMode::Apply,
        },
        reporter,
    )?;
    Ok(summaries.remove(0))
}

pub fn relay_dependencies_in_dir(
    project_root: &Path,
    cache_root: &Path,
    packages: &[String],
    repo_path_override: Option<&Path>,
    via_override: Option<Adapter>,
    create_missing: bool,
    reporter: &Reporter,
) -> Result<Vec<RelaySummary>> {
    relay_dependencies_in_dir_mode(
        project_root,
        cache_root,
        packages,
        RelayExecution {
            repo_path_override,
            via_override,
            create_missing,
            execution_mode: ExecutionMode::Apply,
        },
        reporter,
    )
}

pub fn relay_dependencies_in_dir_dry_run(
    project_root: &Path,
    cache_root: &Path,
    packages: &[String],
    repo_path_override: Option<&Path>,
    via_override: Option<Adapter>,
    create_missing: bool,
    reporter: &Reporter,
) -> Result<Vec<RelaySummary>> {
    relay_dependencies_in_dir_mode(
        project_root,
        cache_root,
        packages,
        RelayExecution {
            repo_path_override,
            via_override,
            create_missing,
            execution_mode: ExecutionMode::DryRun,
        },
        reporter,
    )
}

fn relay_dependencies_in_dir_mode(
    project_root: &Path,
    cache_root: &Path,
    packages: &[String],
    execution: RelayExecution<'_>,
    reporter: &Reporter,
) -> Result<Vec<RelaySummary>> {
    if packages.is_empty() {
        bail!("relay requires at least one dependency");
    }
    if packages.len() > 1 && execution.repo_path_override.is_some() {
        bail!("`nodus relay --repo-path` requires exactly one dependency");
    }

    let mut workspace = load_workspace(project_root, cache_root, reporter)?;
    let original_local_config = workspace.local_config.clone();
    let mut jobs = Vec::with_capacity(packages.len());
    for package in packages {
        let dependency = dependency_context(&workspace, package)?;
        let linked_repo = resolve_linked_repo(
            &mut workspace.local_config,
            &dependency,
            execution.repo_path_override,
            execution.via_override,
        )?;

        let plan = build_relay_plan(
            &workspace,
            &dependency,
            &workspace.project_root,
            workspace.selected_adapters,
            workspace.local_config.relay_link(&dependency.alias),
            &linked_repo,
            execution.create_missing,
        )?;
        if !plan.conflicts.is_empty() {
            bail!(
                "relay conflicts for `{}`:\n{}",
                dependency.alias,
                plan.conflicts.join("\n")
            );
        }
        update_relay_link_state(&mut workspace.local_config, &dependency, &plan)?;
        let relay_link = workspace
            .local_config
            .relay_link(&dependency.alias)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "no relay link configured for `{}`; rerun with `--repo-path <path>`",
                    dependency.alias
                )
            })?;
        jobs.push(RelayJobPlan {
            dependency,
            linked_repo,
            relay_link,
            plan,
        });
    }

    ensure_disjoint_job_writes(&jobs)?;
    let local_config_changed = workspace.local_config != original_local_config;

    if execution.execution_mode.is_dry_run() {
        preview_relay_jobs(project_root, reporter, local_config_changed, &jobs)?;
    } else {
        apply_relay_jobs_and_persist_state(project_root, &jobs, original_local_config, reporter)?;
    }

    Ok(jobs
        .into_iter()
        .map(|job| RelaySummary {
            alias: job.dependency.alias,
            linked_repo: job.linked_repo,
            created_file_count: job
                .plan
                .writes
                .values()
                .filter(|write| write.kind == RelayOperationKind::Create)
                .count(),
            updated_file_count: job
                .plan
                .writes
                .values()
                .filter(|write| write.kind == RelayOperationKind::Update)
                .count(),
        })
        .collect())
}

pub async fn watch_dependency_in_dir(
    project_root: &Path,
    cache_root: &Path,
    package: &str,
    repo_path_override: Option<&Path>,
    via_override: Option<Adapter>,
    create_missing: bool,
    reporter: &Reporter,
) -> Result<()> {
    watch_dependency_in_dir_with_options(
        project_root,
        cache_root,
        package,
        RelayWatchInvocation {
            repo_path_override,
            via_override,
            create_missing,
            options: RelayWatchOptions::default(),
        },
        reporter,
    )
    .await
    .map(|_| ())
}

pub async fn watch_dependencies_in_dir(
    project_root: &Path,
    cache_root: &Path,
    packages: &[String],
    via_override: Option<Adapter>,
    create_missing: bool,
    reporter: &Reporter,
) -> Result<()> {
    watch_dependencies_in_dir_with_options(
        project_root,
        cache_root,
        packages,
        RelayWatchInvocation {
            repo_path_override: None,
            via_override,
            create_missing,
            options: RelayWatchOptions::default(),
        },
        reporter,
    )
    .await
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
        .all_dependency_entries()
        .into_iter()
        .map(|entry| entry.alias.to_string())
        .filter(|alias| workspace.local_config.relay.contains_key(alias))
        .collect::<Vec<_>>();
    if linked_aliases.is_empty() {
        return Ok(());
    }

    let mut blocked = Vec::new();
    for alias in linked_aliases {
        let dependency = dependency_context(&workspace, &alias)?;
        let linked = resolve_existing_link(&workspace.local_config, &dependency)?;
        let plan = build_relay_plan(
            &workspace,
            &dependency,
            &workspace.project_root,
            workspace.selected_adapters,
            workspace.local_config.relay_link(&alias),
            &linked,
            false,
        )?;
        if !plan.conflicts.is_empty() {
            blocked.push(format!("{alias}: {}", plan.conflicts.join("; ")));
            continue;
        }
        if !plan.writes.is_empty() {
            blocked.push(format!(
                "{alias}: {} pending relayed source files",
                plan.writes.len()
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
    let snapshot_roots = snapshot_packages(cache_root, &resolution.packages)?
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
                packages: Vec::new(),
                warnings: Vec::new(),
                managed_migrations: Vec::new(),
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
    let snapshot_roots = snapshot_packages(cache_root, &resolution.packages)?
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
            } else if path.starts_with(".github/skills/") || path.starts_with(".github/agents/") {
                Some(Adapter::Copilot)
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
    let alias = resolve_dependency_alias(&workspace.root.manifest, package)?;
    let spec = workspace
        .root
        .manifest
        .get_dependency(&alias)
        .map(|entry| entry.spec)
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
    local_config: &mut LocalConfig,
    dependency: &DependencyContext,
    repo_path_override: Option<&Path>,
    via_override: Option<Adapter>,
) -> Result<PathBuf> {
    match repo_path_override {
        Some(path) => {
            let linked_repo = canonicalize_existing_dir(path)?;
            validate_linked_repo(&linked_repo, &dependency.url)?;
            let existing = local_config.relay_link(&dependency.alias).cloned();
            let reuses_state = existing
                .as_ref()
                .is_some_and(|link| link.repo_path == linked_repo && link.url == dependency.url);
            local_config.set_relay_link(
                dependency.alias.clone(),
                RelayLink {
                    repo_path: linked_repo.clone(),
                    url: dependency.url.clone(),
                    via: via_override.or(existing.as_ref().and_then(|link| link.via)),
                    package_digest: reuses_state
                        .then(|| {
                            existing
                                .as_ref()
                                .and_then(|link| link.package_digest.clone())
                        })
                        .flatten(),
                    files: if reuses_state {
                        existing.map(|link| link.files).unwrap_or_default()
                    } else {
                        BTreeMap::new()
                    },
                },
            );
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
                            package_digest: existing.package_digest.clone(),
                            files: existing.files.clone(),
                        },
                    );
                }
            }
            Ok(linked_repo)
        }
    }
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
    workspace: &RelayWorkspace,
    dependency: &DependencyContext,
    project_root: &Path,
    selected_adapters: Adapters,
    relay_link: Option<&RelayLink>,
    linked_repo: &Path,
    create_missing: bool,
) -> Result<RelayPlan> {
    let managed_names =
        ManagedArtifactNames::from_resolved_packages(workspace.resolution.packages.iter());
    let mut mappings = mappings::build_mappings(
        &managed_names,
        &workspace.resolution.packages,
        dependency,
        project_root,
        selected_adapters,
        linked_repo,
    )?;
    if create_missing {
        mappings.extend(mappings::build_missing_mappings(
            &managed_names,
            &workspace.resolution.packages,
            dependency,
            project_root,
            selected_adapters,
            relay_link.and_then(|link| link.via),
            linked_repo,
        )?);
    }
    let mut grouped = BTreeMap::<PathBuf, Vec<RelayFileMapping>>::new();
    for mapping in mappings {
        grouped
            .entry(mapping.linked_source_path.clone())
            .or_default()
            .push(mapping);
    }

    let mut plan = RelayPlan::default();
    for (linked_source_path, group) in grouped {
        let planned = plan_linked_source_path(
            linked_source_path,
            group,
            relay_link,
            &dependency.package.digest,
            linked_repo,
        )?;
        let PlannedLinkedSourcePath {
            relative_path,
            linked_source_path,
            write,
            noop,
            state_file,
            conflicts,
        } = planned;
        plan.conflicts.extend(conflicts);
        if let Some(state_file) = state_file {
            plan.state_files.insert(relative_path, state_file);
        }
        if noop {
            plan.noops.insert(linked_source_path.clone());
        }
        if let Some(write) = write {
            plan.writes.insert(linked_source_path, write);
        }
    }

    Ok(plan)
}

fn update_relay_link_state(
    local_config: &mut LocalConfig,
    dependency: &DependencyContext,
    plan: &RelayPlan,
) -> Result<()> {
    let link = local_config
        .relay_link_mut(&dependency.alias)
        .ok_or_else(|| {
            anyhow!(
                "no relay link configured for `{}`; rerun with `--repo-path <path>`",
                dependency.alias
            )
        })?;
    if plan.state_files.is_empty() {
        link.package_digest = None;
        link.files.clear();
    } else {
        link.package_digest = Some(dependency.package.digest.clone());
        link.files = plan.state_files.clone();
    }
    Ok(())
}

fn ensure_disjoint_job_writes(jobs: &[RelayJobPlan]) -> Result<()> {
    let mut owners = BTreeMap::<PathBuf, &str>::new();
    for job in jobs {
        for path in job.plan.writes.keys() {
            if let Some(existing) = owners.insert(path.clone(), &job.dependency.alias) {
                bail!(
                    "relay jobs for `{existing}` and `{}` both write {}",
                    job.dependency.alias,
                    display_relative(&job.linked_repo, path)
                );
            }
        }
    }
    Ok(())
}

fn preview_relay_jobs(
    project_root: &Path,
    reporter: &Reporter,
    local_config_changed: bool,
    jobs: &[RelayJobPlan],
) -> Result<()> {
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

    for job in jobs {
        reporter.status(
            "Preview",
            format!("relay edits for {}", job.dependency.alias),
        )?;
        for (path, write) in &job.plan.writes {
            match write.kind {
                RelayOperationKind::Create => {
                    reporter.preview(&PreviewChange::Create(path.clone()))?
                }
                RelayOperationKind::Update => {
                    reporter.preview(&PreviewChange::Relay(path.clone()))?
                }
            }
        }
        for path in &job.plan.noops {
            reporter.note(format!(
                "{} already matches managed edits",
                display_relative(&job.linked_repo, path)
            ))?;
        }
    }

    Ok(())
}

fn apply_relay_job(job: &RelayJobPlan) -> Result<()> {
    for (path, write) in &job.plan.writes {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        crate::store::write_atomic(path, &write.contents)
            .with_context(|| format!("failed to write relayed source {}", path.display()))?;
    }
    Ok(())
}

fn apply_relay_jobs_and_persist_state(
    project_root: &Path,
    jobs: &[RelayJobPlan],
    mut local_config: LocalConfig,
    reporter: &Reporter,
) -> Result<()> {
    for job in jobs {
        apply_relay_job(job)?;
        local_config.set_relay_link(job.dependency.alias.clone(), job.relay_link.clone());
        local_config.save_in_dir(project_root)?;
        reporter.status(
            "Relaying",
            format!("managed edits for {}", job.dependency.alias),
        )?;
        for (path, write) in &job.plan.writes {
            let action = match write.kind {
                RelayOperationKind::Create => "created",
                RelayOperationKind::Update => "updated",
            };
            reporter.note(format!(
                "{action} {}",
                display_relative(&job.linked_repo, path)
            ))?;
        }
        for path in &job.plan.noops {
            reporter.note(format!(
                "{} already matches managed edits",
                display_relative(&job.linked_repo, path)
            ))?;
        }
    }
    Ok(())
}

fn content_hash(bytes: &[u8]) -> String {
    blake3_hex(bytes)
}

fn plan_linked_source_path(
    linked_source_path: PathBuf,
    group: Vec<RelayFileMapping>,
    relay_link: Option<&RelayLink>,
    dependency_digest: &str,
    linked_repo: &Path,
) -> Result<PlannedLinkedSourcePath> {
    let mut candidate_source: Option<Vec<u8>> = None;
    let linked_current = read_optional_linked_source(&linked_source_path)?;
    let mut baseline_sources = Vec::<Vec<u8>>::new();
    let mut conflicts = Vec::new();

    for mapping in group {
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
        let mapping_baseline = match &mapping.snapshot_path {
            Some(snapshot_path) => {
                let baseline = fs::read(snapshot_path).with_context(|| {
                    format!("failed to read relay baseline {}", snapshot_path.display())
                })?;
                if !baseline_sources
                    .iter()
                    .any(|existing| existing == &baseline)
                {
                    baseline_sources.push(baseline.clone());
                }
                let baseline_managed = mapping.transform.to_managed_bytes(&baseline)?;
                if current_managed == baseline_managed {
                    continue;
                }
                Some(baseline)
            }
            None => None,
        };

        let candidate = mapping.transform.to_source_bytes(
            &current_managed,
            mapping_baseline.as_deref(),
            &mapping.artifact_id,
        )?;
        if let Some(existing) = &candidate_source {
            if existing != &candidate {
                conflicts.push(format!(
                    "managed variants for {} disagree on relayed contents",
                    linked_source_path.display()
                ));
                continue;
            }
        } else {
            candidate_source = Some(candidate);
        }
    }

    let relative_path = display_relative(linked_repo, &linked_source_path);
    let linked_hash_matches_state = relay_link
        .filter(|link| link.package_digest.as_deref() == Some(dependency_digest))
        .and_then(|link| link.files.get(&relative_path))
        .zip(linked_current.as_deref())
        .is_some_and(|(state, current)| state.source_hash == content_hash(current));
    let Some(candidate_source) = candidate_source else {
        let state_file = linked_current
            .as_deref()
            .filter(|current| {
                baseline_sources
                    .iter()
                    .any(|baseline| current == &baseline.as_slice())
                    || linked_hash_matches_state
            })
            .map(|current| RelayedFileState {
                source_hash: content_hash(current),
            });
        return Ok(PlannedLinkedSourcePath {
            relative_path,
            linked_source_path,
            write: None,
            noop: false,
            state_file,
            conflicts,
        });
    };
    let state_file = RelayedFileState {
        source_hash: content_hash(&candidate_source),
    };

    if linked_current.as_deref() == Some(candidate_source.as_slice()) {
        return Ok(PlannedLinkedSourcePath {
            relative_path,
            linked_source_path,
            write: None,
            noop: true,
            state_file: Some(state_file),
            conflicts,
        });
    }

    let linked_matches_baseline = baseline_sources
        .iter()
        .any(|baseline| linked_current.as_deref() == Some(baseline.as_slice()));
    if !baseline_sources.is_empty() {
        if linked_current.is_some() && !linked_matches_baseline && !linked_hash_matches_state {
            conflicts.push(format!(
                "{} changed in both managed outputs and linked source",
                linked_source_path.display()
            ));
            return Ok(PlannedLinkedSourcePath {
                relative_path,
                linked_source_path,
                write: None,
                noop: false,
                state_file: None,
                conflicts,
            });
        }
    } else if linked_current.is_some() && !linked_hash_matches_state {
        conflicts.push(format!(
            "{} already exists in the linked source and does not match the managed creation candidate",
            linked_source_path.display()
        ));
        return Ok(PlannedLinkedSourcePath {
            relative_path,
            linked_source_path,
            write: None,
            noop: false,
            state_file: None,
            conflicts,
        });
    }

    let kind = if linked_current.is_some() {
        RelayOperationKind::Update
    } else {
        RelayOperationKind::Create
    };
    Ok(PlannedLinkedSourcePath {
        relative_path,
        linked_source_path,
        write: Some(RelayWrite {
            kind,
            contents: candidate_source,
        }),
        noop: false,
        state_file: Some(state_file),
        conflicts,
    })
}

fn read_optional_linked_source(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("failed to read linked source {}", path.display()))
        }
    }
}

fn canonicalize_existing_dir(path: &Path) -> Result<PathBuf> {
    let canonical =
        canonicalize_path(path).with_context(|| format!("failed to access {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("{} is not a directory", canonical.display());
    }
    Ok(canonical)
}

fn display_relative(root: &Path, path: &Path) -> String {
    display_path(strip_path_prefix(path, root).unwrap_or(path))
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
    use crate::adapters::{ArtifactKind, ManagedArtifactNames};
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

    fn write_codex_agent_toml(path: &Path, name: &str, description: &str, instructions: &str) {
        write_file(
            path,
            &format!(
                "name = {name:?}\ndescription = {description:?}\ndeveloper_instructions = {instructions:?}\n"
            ),
        );
    }

    fn write_codex_command_skill(path: &Path, skill_id: &str, command_id: &str, body: &str) {
        let contents = crate::adapters::codex::emitted_command_skill_markdown(
            body.as_bytes(),
            skill_id,
            command_id,
            "Codex command source",
        )
        .unwrap();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn append_file(path: &Path, suffix: &str) {
        let mut contents = fs::read_to_string(path).unwrap();
        contents.push_str(suffix);
        fs::write(path, contents).unwrap();
    }

    fn managed_skill_root(
        project_root: &Path,
        adapter: Adapter,
        package: &ResolvedPackage,
        skill_id: &str,
    ) -> PathBuf {
        let names = Lockfile::read(&project_root.join(LOCKFILE_NAME))
            .map(|lockfile| ManagedArtifactNames::from_locked_packages(lockfile.packages.iter()))
            .unwrap_or_else(|_| ManagedArtifactNames::from_resolved_packages([package]));
        crate::adapters::managed_skill_root(&names, project_root, adapter, package, skill_id)
    }

    fn managed_artifact_path(
        project_root: &Path,
        adapter: Adapter,
        kind: ArtifactKind,
        package: &ResolvedPackage,
        artifact_id: &str,
    ) -> Option<PathBuf> {
        let names = Lockfile::read(&project_root.join(LOCKFILE_NAME))
            .map(|lockfile| ManagedArtifactNames::from_locked_packages(lockfile.packages.iter()))
            .unwrap_or_else(|_| ManagedArtifactNames::from_resolved_packages([package]));
        crate::adapters::managed_artifact_path(
            &names,
            project_root,
            adapter,
            kind,
            package,
            artifact_id,
        )
    }

    fn managed_codex_command_skill_path(
        project_root: &Path,
        package: &ResolvedPackage,
        command_id: &str,
    ) -> PathBuf {
        let names = Lockfile::read(&project_root.join(LOCKFILE_NAME))
            .map(|lockfile| ManagedArtifactNames::from_locked_packages(lockfile.packages.iter()))
            .unwrap_or_else(|_| ManagedArtifactNames::from_resolved_packages([package]));
        let skill_id =
            crate::adapters::codex::synthetic_command_skill_id(&names, package, command_id);
        crate::adapters::managed_skill_root(
            &names,
            project_root,
            Adapter::Codex,
            package,
            &skill_id,
        )
        .join("SKILL.md")
    }

    fn relay_dependency_in_dir(
        project_root: &Path,
        cache_root: &Path,
        package: &str,
        repo_path_override: Option<&Path>,
        via_override: Option<Adapter>,
        reporter: &Reporter,
    ) -> Result<RelaySummary> {
        super::relay_dependency_in_dir(
            project_root,
            cache_root,
            package,
            repo_path_override,
            via_override,
            false,
            reporter,
        )
    }

    fn relay_dependency_in_dir_create_missing(
        project_root: &Path,
        cache_root: &Path,
        package: &str,
        repo_path_override: Option<&Path>,
        via_override: Option<Adapter>,
        reporter: &Reporter,
    ) -> Result<RelaySummary> {
        super::relay_dependency_in_dir(
            project_root,
            cache_root,
            package,
            repo_path_override,
            via_override,
            true,
            reporter,
        )
    }

    fn wait_until(mut predicate: impl FnMut() -> bool, message: &str) {
        for _ in 0..500 {
            if predicate() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("{message}");
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

    fn create_remote_dependency_with_codex_toml_agent() -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path().join("playbook-codex");
        fs::create_dir_all(&repo_path).unwrap();
        write_file(
            &repo_path.join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Example.\n---\n# Review\n",
        );
        write_codex_agent_toml(
            &repo_path.join("agents/security.toml"),
            "Security reviewer",
            "Review security-sensitive code.",
            "Be careful.",
        );
        init_git_repo(&repo_path);
        run_git(&repo_path, &["tag", "v0.1.0"]);
        (temp, repo_path)
    }

    fn create_remote_dependency() -> (TempDir, PathBuf) {
        create_remote_dependency_named("playbook-ios")
    }

    fn create_learning_dependency() -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path().join("playbook-learning");
        fs::create_dir_all(&repo_path).unwrap();
        write_file(
            &repo_path.join("nodus.toml"),
            r#"
name = "learning"

[adapters]
enabled = ["claude"]

[[managed_exports]]
source = "learnings"
target = "learnings"
placement = "project"
"#,
        );
        write_file(
            &repo_path.join("learnings/general.md"),
            "# General Learnings\n",
        );
        write_file(
            &repo_path.join("rules/learning.md"),
            "Record learnings under the managed export.\n",
        );
        init_git_repo(&repo_path);
        run_git(&repo_path, &["tag", "v0.1.0"]);
        (temp, repo_path)
    }

    fn create_versioned_dependency_with_same_skill() -> (TempDir, PathBuf) {
        let (temp, repo_path) = create_remote_dependency_named("playbook-versioned");
        write_file(&repo_path.join("README.md"), "# Version two\n");
        run_git(&repo_path, &["add", "."]);
        run_git(&repo_path, &["commit", "-m", "version two"]);
        run_git(&repo_path, &["tag", "v0.2.0"]);
        (temp, repo_path)
    }

    fn create_versioned_dependency_with_disjoint_skills() -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path().join("playbook-disjoint");
        fs::create_dir_all(&repo_path).unwrap();
        write_file(
            &repo_path.join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Review skill.\n---\n# Review\n",
        );
        init_git_repo(&repo_path);
        run_git(&repo_path, &["tag", "v0.1.0"]);

        fs::remove_dir_all(repo_path.join("skills/review")).unwrap();
        write_file(
            &repo_path.join("skills/checks/SKILL.md"),
            "---\nname: Checks\ndescription: Checks skill.\n---\n# Checks\n",
        );
        run_git(&repo_path, &["add", "."]);
        run_git(&repo_path, &["commit", "-m", "version two"]);
        run_git(&repo_path, &["tag", "v0.2.0"]);
        (temp, repo_path)
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

    fn install_dependency_with_kind(
        project: &Path,
        cache: &Path,
        remote: &Path,
        adapters: &[Adapter],
        kind: crate::manifest::DependencyKind,
    ) {
        let reporter = Reporter::silent();
        add_dependency_in_dir_with_adapters(
            project,
            cache,
            &remote.to_string_lossy(),
            AddDependencyOptions {
                git_ref: Some(crate::manifest::RequestedGitRef::Tag("v0.1.0")),
                version_req: None,
                kind,
                adapters,
                components: &[],
                sync_on_launch: false,
                accept_all_dependencies: false,
            },
            &reporter,
        )
        .unwrap();
    }

    fn install_dependency(project: &Path, cache: &Path, remote: &Path, adapters: &[Adapter]) {
        install_dependency_with_kind(
            project,
            cache,
            remote,
            adapters,
            crate::manifest::DependencyKind::Dependency,
        );
    }

    fn sync_project(project: &Path, cache: &Path, adapters: &[Adapter]) {
        crate::resolver::sync_in_dir_with_adapters(
            project,
            cache,
            false,
            false,
            false,
            adapters,
            false,
            &Reporter::silent(),
        )
        .unwrap();
    }

    fn resolved_package(project: &Path, cache: &Path, adapters: &[Adapter]) -> ResolvedPackage {
        resolved_package_by_alias(project, cache, adapters, "playbook_ios")
    }

    fn resolved_package_by_alias(
        project: &Path,
        cache: &Path,
        adapters: &[Adapter],
        alias: &str,
    ) -> ResolvedPackage {
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
            .find(|package| package.alias == alias)
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
            Adapter::Copilot,
            Adapter::Cursor,
            Adapter::OpenCode,
        ] {
            append_file(
                &managed_skill_root(project.path(), adapter, &package, "review").join("SKILL.md"),
                skill_suffix,
            );
        }
        let agent_suffix = "\nRelay agent update.\n";
        for adapter in [Adapter::Claude, Adapter::Copilot, Adapter::OpenCode] {
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
        for adapter in [Adapter::Claude, Adapter::Cursor, Adapter::OpenCode] {
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
        append_file(
            &managed_codex_command_skill_path(project.path(), &package, "build"),
            command_suffix,
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
        assert_eq!(link.repo_path, canonicalize_path(&linked_repo).unwrap());
        assert_eq!(link.via, None);
    }

    #[test]
    fn relay_writes_back_copilot_skill_and_agent_edits() {
        let (_remote_root, remote_repo) = create_remote_dependency();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        install_dependency(
            project.path(),
            cache.path(),
            &remote_repo,
            &[Adapter::Copilot],
        );

        let package = resolved_package(project.path(), cache.path(), &[Adapter::Copilot]);
        append_file(
            &managed_skill_root(project.path(), Adapter::Copilot, &package, "review")
                .join("SKILL.md"),
            "\nCopilot skill update.\n",
        );
        append_file(
            &managed_artifact_path(
                project.path(),
                Adapter::Copilot,
                ArtifactKind::Agent,
                &package,
                "security",
            )
            .unwrap(),
            "\nCopilot agent update.\n",
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

        assert_eq!(summary.updated_file_count, 2);
        let relayed_skill = fs::read_to_string(linked_repo.join("skills/review/SKILL.md")).unwrap();
        assert!(relayed_skill.contains("name: Review"));
        assert!(!relayed_skill.contains("name: review_"));
        assert!(relayed_skill.ends_with("\nCopilot skill update.\n"));
        assert!(
            fs::read_to_string(linked_repo.join("agents/security.md"))
                .unwrap()
                .ends_with("\nCopilot agent update.\n")
        );
    }

    #[test]
    fn relay_writes_back_codex_agent_edits_to_toml_source() {
        let (_remote_root, remote_repo) = create_remote_dependency_with_codex_toml_agent();
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

        let package = resolved_package_by_alias(
            project.path(),
            cache.path(),
            &[Adapter::Codex],
            "playbook_codex",
        );
        let managed_path = managed_artifact_path(
            project.path(),
            Adapter::Codex,
            ArtifactKind::Agent,
            &package,
            "security",
        )
        .unwrap();
        write_codex_agent_toml(
            &managed_path,
            "Security reviewer",
            "Review security-sensitive code.",
            "Be extra careful.",
        );

        let summary = relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_codex",
            Some(&linked_repo),
            Some(Adapter::Codex),
            &Reporter::silent(),
        )
        .unwrap();

        assert_eq!(summary.updated_file_count, 1);
        let relayed = fs::read_to_string(linked_repo.join("agents/security.toml")).unwrap();
        assert!(relayed.contains("name = \"Security reviewer\""));
        assert!(relayed.contains("developer_instructions = \"Be extra careful.\""));
    }

    #[test]
    fn relay_writes_back_codex_agent_edits_to_markdown_source() {
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

        let package = resolved_package(project.path(), cache.path(), &[Adapter::Codex]);
        let managed_path = managed_artifact_path(
            project.path(),
            Adapter::Codex,
            ArtifactKind::Agent,
            &package,
            "security",
        )
        .unwrap();
        write_codex_agent_toml(
            &managed_path,
            "security",
            "Instructions for the `security` agent.",
            "# Security\nUpdated from Codex.\n",
        );

        let summary = relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            Some(Adapter::Codex),
            &Reporter::silent(),
        )
        .unwrap();

        assert_eq!(summary.updated_file_count, 1);
        assert_eq!(
            fs::read_to_string(linked_repo.join("agents/security.md")).unwrap(),
            "# Security\nUpdated from Codex.\n"
        );
    }

    #[test]
    fn relay_writes_back_codex_command_skill_edits_to_source() {
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

        let package = resolved_package(project.path(), cache.path(), &[Adapter::Codex]);
        write_codex_command_skill(
            &managed_codex_command_skill_path(project.path(), &package, "build"),
            "__cmd_build",
            "build",
            "# Build\nUpdated from Codex.\n",
        );

        let summary = relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            Some(Adapter::Codex),
            &Reporter::silent(),
        )
        .unwrap();

        assert_eq!(summary.updated_file_count, 1);
        assert_eq!(
            fs::read_to_string(linked_repo.join("commands/build.md")).unwrap(),
            "# Build\nUpdated from Codex.\n"
        );
    }

    #[test]
    fn relay_create_missing_is_opt_in() {
        let (_remote_root, remote_repo) = create_remote_dependency();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        install_dependency(
            project.path(),
            cache.path(),
            &remote_repo,
            &[Adapter::Copilot],
        );

        let package = resolved_package(project.path(), cache.path(), &[Adapter::Copilot]);
        let new_skill_root = project.path().join(".github/skills/draft");
        write_file(
            &new_skill_root.join("SKILL.md"),
            "---\nname: draft_managed\ndescription: New draft.\n---\n# Draft\n",
        );
        write_file(
            &project.path().join(".github/agents/auditor.agent.md"),
            "# Auditor\n",
        );

        let summary = relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            Some(Adapter::Copilot),
            &Reporter::silent(),
        )
        .unwrap();

        assert_eq!(summary.created_file_count, 0);
        assert_eq!(summary.updated_file_count, 0);
        assert!(!linked_repo.join("skills/draft/SKILL.md").exists());
        assert!(!linked_repo.join("agents/auditor.md").exists());
        assert!(
            package
                .manifest
                .discovered
                .skills
                .iter()
                .any(|skill| skill.id == "review")
        );
    }

    #[test]
    fn relay_create_missing_copies_new_copilot_skill_and_agent_into_source() {
        let (_remote_root, remote_repo) = create_remote_dependency();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        install_dependency(
            project.path(),
            cache.path(),
            &remote_repo,
            &[Adapter::Copilot],
        );

        write_file(
            &project.path().join(".github/skills/draft/SKILL.md"),
            "---\nname: draft_managed\ndescription: New draft.\n---\n# Draft\n",
        );
        write_file(
            &project.path().join(".github/agents/auditor.agent.md"),
            "# Auditor\n",
        );

        let summary = relay_dependency_in_dir_create_missing(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            Some(Adapter::Copilot),
            &Reporter::silent(),
        )
        .unwrap();

        assert_eq!(summary.created_file_count, 2);
        assert_eq!(summary.updated_file_count, 0);
        assert!(
            fs::read_to_string(linked_repo.join("skills/draft/SKILL.md"))
                .unwrap()
                .contains("name: draft")
        );
        assert_eq!(
            fs::read_to_string(linked_repo.join("agents/auditor.md")).unwrap(),
            "# Auditor\n"
        );
    }

    #[test]
    fn relay_create_missing_copies_new_codex_agent_into_toml_source() {
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

        write_codex_agent_toml(
            &project.path().join(".codex/agents/auditor.toml"),
            "auditor",
            "Instructions for the `auditor` agent.",
            "Audit changes carefully.",
        );

        let summary = relay_dependency_in_dir_create_missing(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            Some(Adapter::Codex),
            &Reporter::silent(),
        )
        .unwrap();

        assert_eq!(summary.created_file_count, 1);
        let relayed = fs::read_to_string(linked_repo.join("agents/auditor.toml")).unwrap();
        assert!(relayed.contains("name = \"auditor\""));
        assert!(relayed.contains("developer_instructions = \"Audit changes carefully.\""));
    }

    #[test]
    fn relay_create_missing_copies_new_codex_command_skill_into_source() {
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

        write_codex_command_skill(
            &project.path().join(".codex/skills/__cmd_draft/SKILL.md"),
            "__cmd_draft",
            "draft",
            "# Draft\nrelay me\n",
        );

        let summary = relay_dependency_in_dir_create_missing(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            Some(Adapter::Codex),
            &Reporter::silent(),
        )
        .unwrap();

        assert_eq!(summary.created_file_count, 1);
        assert_eq!(
            fs::read_to_string(linked_repo.join("commands/draft.md")).unwrap(),
            "# Draft\nrelay me\n"
        );
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
        assert_eq!(link.repo_path, canonicalize_path(&linked_repo).unwrap());
        assert_eq!(link.via, Some(Adapter::Claude));
    }

    #[test]
    fn relay_allows_successive_managed_edits_after_successful_relay() {
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

        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            None,
            &Reporter::silent(),
        )
        .unwrap();

        append_file(&managed_skill, "\nFirst relay update.\n");
        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            None,
            None,
            &Reporter::silent(),
        )
        .unwrap();

        append_file(&managed_skill, "Second relay update.\n");
        let summary = relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            None,
            None,
            &Reporter::silent(),
        )
        .unwrap();

        assert_eq!(summary.updated_file_count, 1);
        let relayed = fs::read_to_string(linked_repo.join("skills/review/SKILL.md")).unwrap();
        assert!(relayed.ends_with("\nFirst relay update.\nSecond relay update.\n"));

        let local_config = LocalConfig::load_in_dir(project.path()).unwrap();
        let link = local_config.relay_link("playbook_ios").unwrap();
        assert_eq!(
            link.package_digest.as_deref(),
            Some(package.digest.as_str())
        );
        assert_eq!(
            link.files["skills/review/SKILL.md"].source_hash,
            content_hash(relayed.as_bytes())
        );
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
    fn relay_writes_back_new_files_inside_package_managed_export_directory() {
        let (_remote_root, remote_repo) = create_learning_dependency();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        write_file(
            &project.path().join("nodus.toml"),
            &format!(
                r#"
[adapters]
enabled = ["claude"]

[dependencies.learning]
url = "{}"
tag = "v0.1.0"
"#,
                toml_path_value(&remote_repo)
            ),
        );

        sync_project(project.path(), cache.path(), &[Adapter::Claude]);

        append_file(
            &project.path().join("learnings/general.md"),
            "\nRelay learning index update.\n",
        );
        write_file(
            &project
                .path()
                .join("learnings/log/20260408/LRN-20260408-1000.md"),
            "# New learning\n",
        );

        let summary = relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "learning",
            Some(&linked_repo),
            None,
            &Reporter::silent(),
        )
        .unwrap();

        assert_eq!(summary.created_file_count, 1);
        assert_eq!(summary.updated_file_count, 1);
        assert!(
            fs::read_to_string(linked_repo.join("learnings/general.md"))
                .unwrap()
                .ends_with("\nRelay learning index update.\n")
        );
        assert_eq!(
            fs::read_to_string(linked_repo.join("learnings/log/20260408/LRN-20260408-1000.md"))
                .unwrap(),
            "# New learning\n"
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
    fn relay_rejects_manual_linked_edits_after_successful_relay() {
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

        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            None,
            &Reporter::silent(),
        )
        .unwrap();

        append_file(&managed_skill, "\nManaged relay update.\n");
        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            None,
            None,
            &Reporter::silent(),
        )
        .unwrap();

        append_file(
            &linked_repo.join("skills/review/SKILL.md"),
            "\nManual linked change.\n",
        );
        append_file(&managed_skill, "Managed second update.\n");

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
            &managed_skill_root(project.path(), Adapter::Claude, &package, "review")
                .join("SKILL.md"),
            "\nClaude change.\n",
        );
        append_file(
            &managed_skill_root(project.path(), Adapter::Codex, &package, "review")
                .join("SKILL.md"),
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
    fn pending_relay_edits_for_dev_dependencies_block_sync() {
        let (_remote_root, remote_repo) = create_remote_dependency();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        install_dependency_with_kind(
            project.path(),
            cache.path(),
            &remote_repo,
            &[Adapter::Claude],
            crate::manifest::DependencyKind::DevDependency,
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

        let package = resolved_package_by_alias(
            project.path(),
            cache.path(),
            &[Adapter::Claude],
            "playbook_ios",
        );
        append_file(
            &managed_skill_root(project.path(), Adapter::Claude, &package, "review")
                .join("SKILL.md"),
            "\nPending dev relay change.\n",
        );

        let sync_error = crate::resolver::sync_in_dir_with_adapters(
            project.path(),
            cache.path(),
            false,
            false,
            false,
            &[],
            false,
            &Reporter::silent(),
        )
        .unwrap_err()
        .to_string();

        assert!(sync_error.contains("pending relay edits"));
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
            managed_skill_root(project.path(), Adapter::Codex, &package, "review")
                .join("SKILL.md")
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
            managed_skill_root(project.path(), Adapter::Codex, &package, "review")
                .join("SKILL.md")
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
            false,
            &[],
            false,
            &Reporter::silent(),
        )
        .unwrap();

        assert!(project.path().join(LOCKFILE_NAME).exists());
    }

    #[test]
    fn relay_batch_supports_same_repo_disjoint_write_sets() {
        let (_remote_root, remote_repo) = create_versioned_dependency_with_disjoint_skills();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        write_file(
            &project.path().join("nodus.toml"),
            &format!(
                r#"
[adapters]
enabled = ["claude"]

[dependencies.review_pkg]
url = "{}"
tag = "v0.1.0"

[dependencies.checks_pkg]
url = "{}"
tag = "v0.2.0"
"#,
                toml_path_value(&remote_repo),
                toml_path_value(&remote_repo)
            ),
        );
        sync_project(project.path(), cache.path(), &[Adapter::Claude]);

        let review_pkg = resolved_package_by_alias(
            project.path(),
            cache.path(),
            &[Adapter::Claude],
            "review_pkg",
        );
        let checks_pkg = resolved_package_by_alias(
            project.path(),
            cache.path(),
            &[Adapter::Claude],
            "checks_pkg",
        );

        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "review_pkg",
            Some(&linked_repo),
            Some(Adapter::Claude),
            &Reporter::silent(),
        )
        .unwrap();
        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "checks_pkg",
            Some(&linked_repo),
            Some(Adapter::Claude),
            &Reporter::silent(),
        )
        .unwrap();

        append_file(
            &managed_skill_root(project.path(), Adapter::Claude, &review_pkg, "review")
                .join("SKILL.md"),
            "\nReview relay update.\n",
        );
        append_file(
            &managed_skill_root(project.path(), Adapter::Claude, &checks_pkg, "checks")
                .join("SKILL.md"),
            "\nChecks relay update.\n",
        );

        let summaries = super::relay_dependencies_in_dir(
            project.path(),
            cache.path(),
            &["review_pkg".into(), "checks_pkg".into()],
            None,
            Some(Adapter::Claude),
            false,
            &Reporter::silent(),
        )
        .unwrap();

        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].created_file_count, 1);
        assert_eq!(summaries[0].updated_file_count, 0);
        assert_eq!(summaries[1].created_file_count, 0);
        assert_eq!(summaries[1].updated_file_count, 1);
        assert!(
            fs::read_to_string(linked_repo.join("skills/review/SKILL.md"))
                .unwrap()
                .ends_with("\nReview relay update.\n")
        );
        assert!(
            fs::read_to_string(linked_repo.join("skills/checks/SKILL.md"))
                .unwrap()
                .ends_with("\nChecks relay update.\n")
        );
    }

    #[test]
    fn relay_batch_rejects_overlapping_write_sets() {
        let (_remote_root, remote_repo) = create_versioned_dependency_with_same_skill();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        write_file(
            &project.path().join("nodus.toml"),
            &format!(
                r#"
[adapters]
enabled = ["claude"]

[dependencies.review_v1]
url = "{}"
tag = "v0.1.0"

[dependencies.review_v2]
url = "{}"
tag = "v0.2.0"
"#,
                toml_path_value(&remote_repo),
                toml_path_value(&remote_repo)
            ),
        );
        sync_project(project.path(), cache.path(), &[Adapter::Claude]);

        let review_v1 = resolved_package_by_alias(
            project.path(),
            cache.path(),
            &[Adapter::Claude],
            "review_v1",
        );
        let review_v2 = resolved_package_by_alias(
            project.path(),
            cache.path(),
            &[Adapter::Claude],
            "review_v2",
        );

        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "review_v1",
            Some(&linked_repo),
            Some(Adapter::Claude),
            &Reporter::silent(),
        )
        .unwrap();
        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "review_v2",
            Some(&linked_repo),
            Some(Adapter::Claude),
            &Reporter::silent(),
        )
        .unwrap();

        append_file(
            &managed_skill_root(project.path(), Adapter::Claude, &review_v1, "review")
                .join("SKILL.md"),
            "\nReview v1 relay update.\n",
        );
        append_file(
            &managed_skill_root(project.path(), Adapter::Claude, &review_v2, "review")
                .join("SKILL.md"),
            "\nReview v2 relay update.\n",
        );

        let error = super::relay_dependencies_in_dir(
            project.path(),
            cache.path(),
            &["review_v1".into(), "review_v2".into()],
            None,
            Some(Adapter::Claude),
            false,
            &Reporter::silent(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("both write"));
        assert!(error.contains("skills/review/SKILL.md"));
    }

    #[test]
    fn relay_batch_persists_successful_job_state_before_later_failure() {
        let (_remote_root, remote_repo) = create_versioned_dependency_with_disjoint_skills();
        let review_linked = clone_linked_repo(&remote_repo);
        let checks_linked = clone_linked_repo(&remote_repo);
        let review_linked_repo = review_linked.path().join("linked");
        let checks_linked_repo = checks_linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        write_file(
            &project.path().join("nodus.toml"),
            &format!(
                r#"
[adapters]
enabled = ["claude"]

[dependencies.review_pkg]
url = "{}"
tag = "v0.1.0"

[dependencies.checks_pkg]
url = "{}"
tag = "v0.2.0"
"#,
                toml_path_value(&remote_repo),
                toml_path_value(&remote_repo)
            ),
        );
        sync_project(project.path(), cache.path(), &[Adapter::Claude]);

        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "review_pkg",
            Some(&review_linked_repo),
            Some(Adapter::Claude),
            &Reporter::silent(),
        )
        .unwrap();
        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "checks_pkg",
            Some(&checks_linked_repo),
            Some(Adapter::Claude),
            &Reporter::silent(),
        )
        .unwrap();

        let review_pkg = resolved_package_by_alias(
            project.path(),
            cache.path(),
            &[Adapter::Claude],
            "review_pkg",
        );
        let checks_pkg = resolved_package_by_alias(
            project.path(),
            cache.path(),
            &[Adapter::Claude],
            "checks_pkg",
        );

        append_file(
            &managed_skill_root(project.path(), Adapter::Claude, &checks_pkg, "checks")
                .join("SKILL.md"),
            "\nChecks baseline relay.\n",
        );
        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "checks_pkg",
            None,
            Some(Adapter::Claude),
            &Reporter::silent(),
        )
        .unwrap();
        append_file(
            &managed_skill_root(project.path(), Adapter::Claude, &review_pkg, "review")
                .join("SKILL.md"),
            "\nReview relay update.\n",
        );
        append_file(
            &managed_skill_root(project.path(), Adapter::Claude, &checks_pkg, "checks")
                .join("SKILL.md"),
            "\nChecks relay update.\n",
        );

        let checks_hash_before = LocalConfig::load_in_dir(project.path())
            .unwrap()
            .relay_link("checks_pkg")
            .unwrap()
            .files["skills/checks/SKILL.md"]
            .source_hash
            .clone();

        let reporter = Reporter::silent();
        let mut workspace = load_workspace(project.path(), cache.path(), &reporter).unwrap();
        let original_local_config = workspace.local_config.clone();
        let mut jobs = Vec::new();
        for alias in ["review_pkg", "checks_pkg"] {
            let dependency = dependency_context(&workspace, alias).unwrap();
            let linked_repo = resolve_existing_link(&workspace.local_config, &dependency).unwrap();
            let plan = build_relay_plan(
                &workspace,
                &dependency,
                &workspace.project_root,
                workspace.selected_adapters,
                workspace.local_config.relay_link(alias),
                &linked_repo,
                false,
            )
            .unwrap();
            update_relay_link_state(&mut workspace.local_config, &dependency, &plan).unwrap();
            let relay_link = workspace
                .local_config
                .relay_link(&dependency.alias)
                .cloned()
                .unwrap();
            jobs.push(RelayJobPlan {
                dependency,
                linked_repo,
                relay_link,
                plan,
            });
        }

        let blocked_parent = checks_linked_repo.join("blocked");
        write_file(&blocked_parent, "not a directory");
        let (original_write_path, write) = jobs[1]
            .plan
            .writes
            .iter()
            .next()
            .map(|(path, write)| (path.clone(), write.clone()))
            .unwrap();
        jobs[1].plan.writes.remove(&original_write_path);
        jobs[1]
            .plan
            .writes
            .insert(blocked_parent.join("skills/checks/SKILL.md"), write);

        let error = apply_relay_jobs_and_persist_state(
            project.path(),
            &jobs,
            original_local_config,
            &Reporter::silent(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("failed to create"));

        let review_contents =
            fs::read_to_string(review_linked_repo.join("skills/review/SKILL.md")).unwrap();
        assert!(review_contents.ends_with("\nReview relay update.\n"));
        let checks_contents =
            fs::read_to_string(checks_linked_repo.join("skills/checks/SKILL.md")).unwrap();
        assert!(!checks_contents.ends_with("\nChecks relay update.\n"));

        let local_config = LocalConfig::load_in_dir(project.path()).unwrap();
        let review_hash =
            local_config.relay_link("review_pkg").unwrap().files["skills/review/SKILL.md"]
                .source_hash
                .clone();
        let checks_hash =
            local_config.relay_link("checks_pkg").unwrap().files["skills/checks/SKILL.md"]
                .source_hash
                .clone();
        assert_eq!(review_hash, content_hash(review_contents.as_bytes()));
        assert_eq!(checks_hash, checks_hash_before);
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
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let repo_ref = linked_repo_for_watch;
                watch_dependency_in_dir_with_options(
                    &project_root,
                    &cache_root,
                    "playbook_ios",
                    RelayWatchInvocation {
                        repo_path_override: Some(&repo_ref),
                        via_override: None,
                        create_missing: false,
                        options: RelayWatchOptions {
                            debounce: Duration::from_millis(10),
                            fallback_interval: Duration::from_secs(30),
                            max_events: Some(2),
                            timeout: Some(Duration::from_secs(5)),
                        },
                    },
                    &Reporter::sink(ColorMode::Never, output_for_watch),
                )
                .await
                .unwrap()
            })
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
    fn relay_watch_syncs_multiple_follow_up_edits_to_same_file() {
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
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let repo_ref = linked_repo_for_watch;
                watch_dependency_in_dir_with_options(
                    &project_root,
                    &cache_root,
                    "playbook_ios",
                    RelayWatchInvocation {
                        repo_path_override: Some(&repo_ref),
                        via_override: None,
                        create_missing: false,
                        options: RelayWatchOptions {
                            debounce: Duration::from_millis(10),
                            fallback_interval: Duration::from_secs(30),
                            max_events: Some(3),
                            timeout: Some(Duration::from_secs(5)),
                        },
                    },
                    &Reporter::sink(ColorMode::Never, output_for_watch),
                )
                .await
                .unwrap()
            })
        });

        wait_until(
            || {
                output
                    .contents()
                    .contains("watching managed outputs for changes")
            },
            "watcher never reported readiness",
        );

        append_file(&managed_skill, "\nWatched relay update one.\n");
        wait_until(
            || {
                fs::read_to_string(linked_repo.join("skills/review/SKILL.md"))
                    .unwrap()
                    .ends_with("\nWatched relay update one.\n")
            },
            "first watched relay update was never applied",
        );
        wait_until(
            || {
                output
                    .contents()
                    .matches("relayed playbook_ios into")
                    .count()
                    >= 2
            },
            "watcher never recorded the first follow-up relay",
        );
        thread::sleep(Duration::from_millis(200));

        append_file(&managed_skill, "Watched relay update two.\n");

        let summaries = watch_handle.join().unwrap();
        assert_eq!(summaries.len(), 3);
        assert_eq!(summaries[1].updated_file_count, 1);
        assert_eq!(summaries[2].updated_file_count, 1);
        assert!(
            fs::read_to_string(linked_repo.join("skills/review/SKILL.md"))
                .unwrap()
                .ends_with("\nWatched relay update one.\nWatched relay update two.\n")
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
                version_req: None,
                kind: crate::manifest::DependencyKind::Dependency,
                adapters: &[Adapter::Claude],
                components: &[],
                sync_on_launch: false,
                accept_all_dependencies: false,
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
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                watch_dependencies_in_dir_with_options(
                    &project_root,
                    &cache_root,
                    &watch_packages,
                    RelayWatchInvocation {
                        repo_path_override: None,
                        via_override: None,
                        create_missing: false,
                        options: RelayWatchOptions {
                            debounce: Duration::from_millis(10),
                            fallback_interval: Duration::from_secs(30),
                            max_events: Some(3),
                            timeout: Some(Duration::from_secs(5)),
                        },
                    },
                    &Reporter::sink(ColorMode::Never, output_for_watch),
                )
                .await
                .unwrap()
            })
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
    fn relay_ignores_stale_file_state_after_dependency_digest_changes() {
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

        let package_v1 = resolved_package(project.path(), cache.path(), &[Adapter::Claude]);
        let managed_skill_v1 =
            managed_skill_root(project.path(), Adapter::Claude, &package_v1, "review")
                .join("SKILL.md");

        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            Some(&linked_repo),
            None,
            &Reporter::silent(),
        )
        .unwrap();
        append_file(&managed_skill_v1, "\nRelayed from v0.1.0.\n");
        relay_dependency_in_dir(
            project.path(),
            cache.path(),
            "playbook_ios",
            None,
            None,
            &Reporter::silent(),
        )
        .unwrap();

        let mut local_config = LocalConfig::load_in_dir(project.path()).unwrap();
        let link = local_config.relay_link_mut("playbook_ios").unwrap();
        assert_eq!(
            link.package_digest.as_deref(),
            Some(package_v1.digest.as_str())
        );
        link.package_digest = Some("sha256:stale".into());
        local_config.save_in_dir(project.path()).unwrap();

        append_file(&managed_skill_v1, "Relayed after digest changed.\n");

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

        assert!(error.contains("changed in both managed outputs and linked source"));
    }

    #[test]
    fn restore_opencode_skill_name_preserves_crlf() {
        let managed = b"---\r\nname: review_abcd12\r\ndescription: Example.\r\n---\r\n# Review\r\n";
        let baseline = b"---\r\nname: Review\r\ndescription: Example.\r\n---\r\n# Review\r\n";

        let restored = mappings::restore_rewritten_skill_name(
            managed,
            baseline,
            "review_abcd12",
            "review",
            "OpenCode",
        )
        .unwrap();
        let restored = String::from_utf8(restored).unwrap();

        assert!(restored.contains("name: Review\r\n"));
        assert!(restored.contains("description: Example.\r\n"));
        assert!(restored.ends_with("\r\n"));
    }

    #[test]
    fn restore_skill_name_falls_back_to_artifact_id_when_baseline_omits_name() {
        let managed = b"---\nname: review_abcd12\ndescription: Example.\n---\n# Review\n";
        let baseline = b"---\ndescription: Example.\n---\n# Review\n";

        let restored = mappings::restore_rewritten_skill_name(
            managed,
            baseline,
            "review_abcd12",
            "review",
            "OpenCode",
        )
        .unwrap();
        let restored = String::from_utf8(restored).unwrap();

        assert!(restored.contains("name: review\n"));
        assert!(restored.contains("description: Example.\n"));
    }
}
