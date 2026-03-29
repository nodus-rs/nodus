use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use semver::{Version, VersionReq};
use sha2::{Digest, Sha256};

use crate::adapters::Adapter;
use crate::execution::ExecutionMode;
use crate::install_paths::InstallPaths;
use crate::manifest::{
    DependencyComponent, DependencyKind, DependencySpec, MANIFEST_FILE, Manifest, PackageRole,
    RequestedGitRef, load_dependency_from_dir, load_root_from_dir_allow_missing,
    normalize_dependency_alias,
};
use crate::paths::display_path;
use crate::report::Reporter;
use crate::resolver::sync_with_loaded_root_at_paths;
use crate::selection::{
    resolve_adapter_selection, resolve_global_adapter_selection, should_prompt_for_adapter,
};

#[derive(Debug, Clone)]
pub struct GitCheckout {
    pub path: PathBuf,
    pub url: String,
    pub tag: Option<String>,
    pub branch: Option<String>,
    pub rev: String,
}

#[derive(Debug, Clone)]
pub struct AddSummary {
    pub alias: String,
    pub kind: DependencyKind,
    pub reference: String,
    pub adapters: Vec<Adapter>,
    pub managed_file_count: usize,
    pub dependency_preview: String,
    pub workspace_members: Vec<WorkspaceMemberStatus>,
}

#[derive(Debug, Clone)]
pub struct RemoveSummary {
    pub alias: String,
    pub kind: DependencyKind,
    pub managed_file_count: usize,
}

#[derive(Debug, Clone)]
pub struct WorkspaceMemberStatus {
    pub id: String,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct AddDependencyOptions<'a> {
    pub git_ref: Option<RequestedGitRef<'a>>,
    pub version_req: Option<VersionReq>,
    pub kind: DependencyKind,
    pub adapters: &'a [Adapter],
    pub components: &'a [DependencyComponent],
    pub sync_on_launch: bool,
}

impl GitCheckout {
    fn reference_display(&self) -> String {
        self.tag
            .clone()
            .or_else(|| self.branch.clone())
            .unwrap_or_else(|| self.rev.clone())
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn add_dependency_in_dir_with_adapters(
    project_root: &Path,
    cache_root: &Path,
    url: &str,
    options: AddDependencyOptions<'_>,
    reporter: &Reporter,
) -> Result<AddSummary> {
    let install_paths = InstallPaths::project(project_root);
    add_dependency_at_paths_with_adapters_mode(
        &install_paths,
        cache_root,
        url,
        options,
        ExecutionMode::Apply,
        reporter,
    )
}

#[allow(dead_code)]
pub fn add_dependency_in_dir_with_adapters_dry_run(
    project_root: &Path,
    cache_root: &Path,
    url: &str,
    options: AddDependencyOptions<'_>,
    reporter: &Reporter,
) -> Result<AddSummary> {
    let install_paths = InstallPaths::project(project_root);
    add_dependency_at_paths_with_adapters_mode(
        &install_paths,
        cache_root,
        url,
        options,
        ExecutionMode::DryRun,
        reporter,
    )
}

pub fn add_dependency_at_paths_with_adapters(
    install_paths: &InstallPaths,
    cache_root: &Path,
    url: &str,
    options: AddDependencyOptions<'_>,
    reporter: &Reporter,
) -> Result<AddSummary> {
    add_dependency_at_paths_with_adapters_mode(
        install_paths,
        cache_root,
        url,
        options,
        ExecutionMode::Apply,
        reporter,
    )
}

pub fn add_dependency_at_paths_with_adapters_dry_run(
    install_paths: &InstallPaths,
    cache_root: &Path,
    url: &str,
    options: AddDependencyOptions<'_>,
    reporter: &Reporter,
) -> Result<AddSummary> {
    add_dependency_at_paths_with_adapters_mode(
        install_paths,
        cache_root,
        url,
        options,
        ExecutionMode::DryRun,
        reporter,
    )
}

fn add_dependency_at_paths_with_adapters_mode(
    install_paths: &InstallPaths,
    cache_root: &Path,
    url: &str,
    options: AddDependencyOptions<'_>,
    execution_mode: ExecutionMode,
    reporter: &Reporter,
) -> Result<AddSummary> {
    let normalized_url = normalize_git_url(url);
    let alias = normalize_alias_from_url(&normalized_url)?;
    let checkout =
        ensure_git_dependency(cache_root, &normalized_url, options.git_ref, true, reporter)?;
    let github = github_slug_from_url(&checkout.url);
    let dependency_manifest = load_dependency_from_dir(&checkout.path)
        .with_context(|| format!("dependency `{alias}` does not match the Nodus package layout"))?;
    let workspace_members = dependency_manifest
        .resolved_workspace_members()?
        .into_iter()
        .map(|member| WorkspaceMemberStatus {
            id: member.id,
            enabled: true,
        })
        .collect::<Vec<_>>();

    let mut root = load_root_from_dir_allow_missing(&install_paths.config_root)?;
    if root.manifest.contains_dependency_alias(&alias) {
        bail!(
            "dependency `{alias}` already exists in {}",
            install_paths.config_root.display()
        );
    }
    reporter.status(
        "Adding",
        format!(
            "{alias} {} to {}",
            checkout.reference_display(),
            install_paths.config_root.join(MANIFEST_FILE).display()
        ),
    )?;
    let dependency = DependencySpec {
        github: github.clone(),
        url: github.is_none().then_some(checkout.url.clone()),
        path: None,
        subpath: None,
        tag: options
            .version_req
            .is_none()
            .then_some(checkout.tag.clone())
            .flatten(),
        branch: checkout.branch.clone(),
        revision: options.git_ref.and_then(|git_ref| match git_ref {
            RequestedGitRef::Revision(_) => Some(checkout.rev.clone()),
            _ => None,
        }),
        version: options.version_req.clone(),
        components: (!options.components.is_empty()).then(|| {
            let mut sorted = options.components.to_vec();
            sorted.sort();
            sorted.dedup();
            sorted
        }),
        members: (!workspace_members.is_empty()).then(|| {
            workspace_members
                .iter()
                .map(|member| member.id.clone())
                .collect::<Vec<_>>()
        }),
        managed: None,
        enabled: true,
    };
    let dependency_preview = format!("{alias} = {{ {} }}", dependency.inline_fields().join(", "));
    root.manifest
        .dependency_section_mut(options.kind)
        .insert(alias.clone(), dependency);
    let selection = if install_paths.is_global() {
        if options.sync_on_launch {
            bail!("`nodus add --global` does not support `--sync-on-launch`");
        }
        resolve_global_adapter_selection(
            &install_paths.adapter_detection_root,
            &root.manifest,
            options.adapters,
        )?
    } else {
        resolve_adapter_selection(
            &install_paths.adapter_detection_root,
            &root.manifest,
            options.adapters,
            should_prompt_for_adapter(),
        )?
    };
    if selection.should_persist {
        root.manifest.set_enabled_adapters(&selection.adapters);
    }
    if options.sync_on_launch {
        root.manifest.set_sync_on_launch(true);
    }
    let root = root.with_manifest(root.manifest.clone(), PackageRole::Root)?;
    let sync_summary = sync_with_loaded_root_at_paths(
        install_paths,
        cache_root,
        false,
        false,
        false,
        options.adapters,
        false,
        execution_mode,
        root,
        reporter,
    )?;

    Ok(AddSummary {
        alias,
        kind: options.kind,
        reference: checkout.reference_display(),
        adapters: sync_summary.adapters,
        managed_file_count: sync_summary.managed_file_count,
        dependency_preview,
        workspace_members,
    })
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn remove_dependency_in_dir(
    project_root: &Path,
    cache_root: &Path,
    package: &str,
    reporter: &Reporter,
) -> Result<RemoveSummary> {
    let install_paths = InstallPaths::project(project_root);
    remove_dependency_at_paths_mode(
        &install_paths,
        cache_root,
        package,
        ExecutionMode::Apply,
        reporter,
    )
}

#[allow(dead_code)]
pub fn remove_dependency_in_dir_dry_run(
    project_root: &Path,
    cache_root: &Path,
    package: &str,
    reporter: &Reporter,
) -> Result<RemoveSummary> {
    let install_paths = InstallPaths::project(project_root);
    remove_dependency_at_paths_mode(
        &install_paths,
        cache_root,
        package,
        ExecutionMode::DryRun,
        reporter,
    )
}

pub fn remove_dependency_at_paths(
    install_paths: &InstallPaths,
    cache_root: &Path,
    package: &str,
    reporter: &Reporter,
) -> Result<RemoveSummary> {
    remove_dependency_at_paths_mode(
        install_paths,
        cache_root,
        package,
        ExecutionMode::Apply,
        reporter,
    )
}

pub fn remove_dependency_at_paths_dry_run(
    install_paths: &InstallPaths,
    cache_root: &Path,
    package: &str,
    reporter: &Reporter,
) -> Result<RemoveSummary> {
    remove_dependency_at_paths_mode(
        install_paths,
        cache_root,
        package,
        ExecutionMode::DryRun,
        reporter,
    )
}

fn remove_dependency_at_paths_mode(
    install_paths: &InstallPaths,
    cache_root: &Path,
    package: &str,
    execution_mode: ExecutionMode,
    reporter: &Reporter,
) -> Result<RemoveSummary> {
    if !install_paths.is_global() {
        crate::relay::ensure_no_pending_relay_edits_in_dir(&install_paths.config_root, cache_root)?;
    }
    let mut root = load_root_from_dir_allow_missing(&install_paths.config_root)?;
    let alias = resolve_dependency_alias(&root.manifest, package)?;
    let kind = root
        .manifest
        .dependency_kind(&alias)
        .ok_or_else(|| anyhow!("dependency `{alias}` does not exist"))?;
    reporter.status(
        "Removing",
        format!(
            "{alias} from {}",
            install_paths.config_root.join(MANIFEST_FILE).display()
        ),
    )?;
    root.manifest.dependency_section_mut(kind).remove(&alias);
    let root = root.with_manifest(root.manifest.clone(), PackageRole::Root)?;
    let sync_summary = sync_with_loaded_root_at_paths(
        install_paths,
        cache_root,
        false,
        false,
        false,
        &[],
        false,
        execution_mode,
        root,
        reporter,
    )?;

    Ok(RemoveSummary {
        alias,
        kind,
        managed_file_count: sync_summary.managed_file_count,
    })
}

pub fn ensure_git_dependency(
    cache_root: &Path,
    url: &str,
    requested_ref: Option<RequestedGitRef<'_>>,
    allow_network: bool,
    reporter: &Reporter,
) -> Result<GitCheckout> {
    let normalized_url = normalize_git_url(url);
    let mirror_path = shared_repository_path(cache_root, &normalized_url)?;
    ensure_shared_repository(&mirror_path, &normalized_url, allow_network, reporter)?;

    let (resolved_tag, resolved_branch, rev) = if let Some(requested_ref) = requested_ref {
        match requested_ref {
            RequestedGitRef::Tag(value) => (
                Some(value.to_string()),
                None,
                resolve_ref_to_rev(&mirror_path, value)?,
            ),
            RequestedGitRef::Branch(value) => (
                None,
                Some(value.to_string()),
                resolve_ref_to_rev(&mirror_path, value)?,
            ),
            RequestedGitRef::Revision(value) => {
                (None, None, resolve_ref_to_rev(&mirror_path, value)?)
            }
            RequestedGitRef::VersionReq(value) => {
                reporter.status(
                    "Resolving",
                    format!("latest compatible tag for {normalized_url} ({value})"),
                )?;
                let tag = latest_compatible_tag(&mirror_path, value)?;
                let rev = resolve_ref_to_rev(&mirror_path, &tag)?;
                (Some(tag), None, rev)
            }
        }
    } else {
        reporter.status("Resolving", format!("latest tag for {normalized_url}"))?;
        match latest_tag_name(&mirror_path)? {
            Some(tag) => {
                let rev = resolve_ref_to_rev(&mirror_path, &tag)?;
                (Some(tag), None, rev)
            }
            None => {
                let branch = default_branch(&mirror_path)?;
                reporter.note(format!(
                    "no git tags found for {normalized_url}; using default branch {branch}"
                ))?;
                let rev = resolve_ref_to_rev(&mirror_path, &branch)?;
                (None, Some(branch), rev)
            }
        }
    };
    let checkout_path = shared_checkout_path(cache_root, &normalized_url, &rev)?;

    ensure_shared_checkout(
        &checkout_path,
        &mirror_path,
        &normalized_url,
        &rev,
        allow_network,
        reporter,
    )?;

    Ok(GitCheckout {
        path: checkout_path,
        url: normalized_url,
        tag: resolved_tag,
        branch: resolved_branch,
        rev,
    })
}

pub fn ensure_git_dependency_at_rev(
    cache_root: &Path,
    url: &str,
    tag: Option<&str>,
    branch: Option<&str>,
    rev: &str,
    allow_network: bool,
    reporter: &Reporter,
) -> Result<GitCheckout> {
    let normalized_url = normalize_git_url(url);
    let mirror_path = shared_repository_path(cache_root, &normalized_url)?;
    ensure_shared_repository(&mirror_path, &normalized_url, allow_network, reporter)?;

    let checkout_path = shared_checkout_path(cache_root, &normalized_url, rev)?;
    ensure_shared_checkout(
        &checkout_path,
        &mirror_path,
        &normalized_url,
        rev,
        allow_network,
        reporter,
    )?;

    Ok(GitCheckout {
        path: checkout_path,
        url: normalized_url,
        tag: tag.map(ToOwned::to_owned),
        branch: branch.map(ToOwned::to_owned),
        rev: rev.to_string(),
    })
}

pub fn prepare_repository_mirror(
    cache_root: &Path,
    url: &str,
    allow_network: bool,
    reporter: &Reporter,
) -> Result<PathBuf> {
    let normalized_url = normalize_git_url(url);
    let mirror_path = shared_repository_path(cache_root, &normalized_url)?;
    ensure_shared_repository(&mirror_path, &normalized_url, allow_network, reporter)?;
    Ok(mirror_path)
}

pub fn shared_repository_path(cache_root: &Path, url: &str) -> Result<PathBuf> {
    let normalized_url = normalize_git_url(url);
    let repositories_root = cache_root.join("repositories");
    let repo_name = normalize_repository_name_from_url(&normalized_url)?;
    let hash = short_hash(&normalized_url);
    Ok(repositories_root.join(format!("{repo_name}-{hash}.git")))
}

pub fn shared_checkout_path(cache_root: &Path, url: &str, rev: &str) -> Result<PathBuf> {
    let normalized_url = normalize_git_url(url);
    let checkouts_root = cache_root.join("checkouts");
    let repo_name = normalize_repository_name_from_url(&normalized_url)?;
    let hash = short_hash(&normalized_url);
    Ok(checkouts_root.join(format!("{repo_name}-{hash}")).join(rev))
}

pub fn current_rev(path: &Path) -> Result<String> {
    git_output(path, ["rev-parse", "HEAD"])
}

pub fn resolve_ref_to_rev(path: &Path, git_ref: &str) -> Result<String> {
    git_output(path, ["rev-parse", &format!("{git_ref}^{{commit}}")])
}

pub fn latest_tag(path: &Path) -> Result<String> {
    latest_tag_name(path)?.ok_or_else(|| anyhow!("no git tags found in {}", path.display()))
}

pub fn latest_compatible_tag(path: &Path, requirement: &VersionReq) -> Result<String> {
    latest_compatible_tag_name(path, requirement)?.ok_or_else(|| {
        anyhow!(
            "no git tags in {} satisfy version requirement `{requirement}`",
            path.display()
        )
    })
}

fn latest_tag_name(path: &Path) -> Result<Option<String>> {
    let tags = git_output(
        path,
        [
            "for-each-ref",
            "--sort=-v:refname",
            "--format=%(refname:strip=2)",
            "refs/tags",
        ],
    )?;
    Ok(tags
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string()))
}

fn latest_compatible_tag_name(path: &Path, requirement: &VersionReq) -> Result<Option<String>> {
    let tags = git_output(
        path,
        [
            "for-each-ref",
            "--sort=-v:refname",
            "--format=%(refname:strip=2)",
            "refs/tags",
        ],
    )?;
    Ok(tags.lines().find_map(|line| {
        let tag = line.trim();
        if tag.is_empty() {
            return None;
        }
        parse_semver_tag(tag)
            .filter(|version| requirement.matches(version))
            .map(|_| tag.to_string())
    }))
}

pub fn parse_semver_tag(tag: &str) -> Option<Version> {
    Version::parse(tag).ok().or_else(|| {
        tag.strip_prefix('v')
            .and_then(|value| Version::parse(value).ok())
    })
}

pub fn default_branch(path: &Path) -> Result<String> {
    if let Ok(head) = remote_head_branch(path) {
        return Ok(head);
    }

    git_output(path, ["symbolic-ref", "--short", "HEAD"])
        .with_context(|| format!("failed to determine default branch for {}", path.display()))
}

fn remote_head_branch(path: &Path) -> Result<String> {
    let output = git_output(path, ["ls-remote", "--symref", "origin", "HEAD"])?;
    let head = output
        .lines()
        .find_map(|line| line.strip_prefix("ref: refs/heads/")?.split_once('\t'))
        .and_then(|(branch, target)| (target == "HEAD").then_some(branch))
        .ok_or_else(|| anyhow!("failed to determine remote HEAD for {}", path.display()))?;
    Ok(head.to_string())
}

pub fn normalize_git_url(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("git@")
        || trimmed.starts_with("ssh://")
        || trimmed.starts_with('/')
        || trimmed.starts_with(r"\\")
        || looks_like_windows_path(trimmed)
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
    {
        return trimmed.to_string();
    }

    if let Some((owner, repo)) = trimmed.split_once('/')
        && !owner.is_empty()
        && !repo.is_empty()
        && !repo.contains('/')
    {
        return format!("https://github.com/{owner}/{repo}");
    }

    trimmed.to_string()
}

pub fn github_slug_from_url(url: &str) -> Option<String> {
    let normalized = normalize_git_url(url);
    let trimmed = normalized
        .strip_prefix("https://github.com/")?
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let (owner, repo) = trimmed.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

pub fn normalize_alias_from_url(url: &str) -> Result<String> {
    let normalized = normalize_git_url(url);
    let trimmed = normalized
        .trim_end_matches(['/', '\\'])
        .trim_end_matches(".git");
    let tail = trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("failed to infer a dependency alias from `{url}`"))?;
    normalize_dependency_alias(tail)
        .with_context(|| format!("failed to infer a dependency alias from `{url}`"))
}

fn looks_like_windows_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

pub fn resolve_dependency_alias(manifest: &Manifest, package: &str) -> Result<String> {
    if manifest.contains_dependency_alias(package) {
        return Ok(package.to_string());
    }

    let normalized = normalize_alias_from_url(package)?;
    if manifest.contains_dependency_alias(&normalized) {
        return Ok(normalized);
    }

    bail!("dependency `{package}` does not exist")
}

pub fn repository_origin_url(path: &Path) -> Result<String> {
    git_output(path, ["remote", "get-url", "origin"]).map(|value| value.trim().to_string())
}

pub fn is_git_repository(path: &Path) -> bool {
    git_output(path, ["rev-parse", "--git-dir"]).is_ok()
}

pub fn git_urls_match(left: &str, right: &str) -> bool {
    let left = normalize_git_url(left);
    let right = normalize_git_url(right);
    if left == right {
        return true;
    }

    match (github_slug_from_url(&left), github_slug_from_url(&right)) {
        (Some(left), Some(right)) => left == right,
        _ => {
            local_git_identity(&left)
                .zip(local_git_identity(&right))
                .is_some_and(|(left, right)| left == right)
                || canonical_local_git_path(&left)
                    .zip(canonical_local_git_path(&right))
                    .is_some_and(|(left, right)| left == right)
        }
    }
}

fn normalize_repository_name_from_url(url: &str) -> Result<String> {
    let normalized = normalize_git_url(url);
    let trimmed = normalized
        .trim_end_matches(['/', '\\'])
        .trim_end_matches(".git");
    let tail = trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("failed to infer a repository name from `{url}`"))?;

    let mut name = String::new();
    for character in tail.chars() {
        if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
            name.push(character);
        } else if !name.ends_with('_') {
            name.push('_');
        }
    }

    let name = name.trim_matches('_').to_string();
    if name.is_empty() {
        bail!("failed to derive a valid repository name from `{url}`");
    }
    Ok(name)
}

fn canonical_local_git_path(url: &str) -> Option<PathBuf> {
    looks_like_local_path(url)
        .then(|| PathBuf::from(url))
        .and_then(|path| dunce::canonicalize(path).ok())
}

fn local_git_identity(url: &str) -> Option<String> {
    if !looks_like_local_path(url) {
        return None;
    }

    let git_dir = git_output(Path::new(url), ["rev-parse", "--absolute-git-dir"]).ok()?;
    let canonical = dunce::canonicalize(git_dir.trim()).ok()?;
    Some(display_path(&canonical).to_ascii_lowercase())
}

fn looks_like_local_path(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with(r"\\")
        || looks_like_windows_path(value)
}

fn short_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    format!("{digest:x}")[..8].to_string()
}

fn ensure_shared_repository(
    mirror_path: &Path,
    normalized_url: &str,
    allow_network: bool,
    reporter: &Reporter,
) -> Result<()> {
    if mirror_path.exists() {
        let bare = is_bare_repository(mirror_path).unwrap_or(false);
        let remote_matches = bare
            && git_output(mirror_path, ["remote", "get-url", "origin"])
                .ok()
                .is_some_and(|remote_url| git_urls_match(remote_url.trim(), normalized_url));

        if !bare || !remote_matches {
            if !allow_network {
                bail!(
                    "shared repository mirror at {} is invalid or out of date",
                    mirror_path.display()
                );
            }

            match fs::remove_dir_all(mirror_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to remove invalid shared repository mirror {}",
                            mirror_path.display()
                        )
                    });
                }
            }
        } else {
            if allow_network {
                reporter.status(
                    "Updating",
                    format!("repository mirror for {normalized_url}"),
                )?;
                git_run(
                    mirror_path,
                    [
                        "fetch",
                        "--tags",
                        "--prune",
                        "origin",
                        "+refs/heads/*:refs/heads/*",
                    ],
                )?;
            }
            git_run(mirror_path, ["config", "core.autocrlf", "false"])?;
            return Ok(());
        }
    }

    if !allow_network {
        bail!(
            "missing shared repository mirror for `{normalized_url}` at {}",
            mirror_path.display()
        );
    }

    let parent = mirror_path.parent().ok_or_else(|| {
        anyhow!(
            "cannot determine parent directory for {}",
            mirror_path.display()
        )
    })?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    reporter.status(
        "Updating",
        format!("repository mirror for {normalized_url}"),
    )?;
    git_run(
        parent,
        [
            "-c",
            "core.autocrlf=false",
            "clone",
            "--bare",
            normalized_url,
            mirror_path.to_string_lossy().as_ref(),
        ],
    )?;
    git_run(mirror_path, ["config", "core.autocrlf", "false"])
}

fn ensure_shared_checkout(
    checkout_path: &Path,
    mirror_path: &Path,
    normalized_url: &str,
    rev: &str,
    allow_network: bool,
    reporter: &Reporter,
) -> Result<()> {
    if checkout_path.exists() {
        match validate_shared_checkout(checkout_path, mirror_path, normalized_url) {
            Ok(()) => {
                git_run(checkout_path, ["config", "core.autocrlf", "false"])?;
                git_run(
                    checkout_path,
                    [
                        "-c",
                        "core.autocrlf=false",
                        "checkout",
                        "--detach",
                        "--force",
                        rev,
                    ],
                )?;
                return Ok(());
            }
            Err(error) if !allow_network => {
                return Err(error).context(format!(
                    "shared checkout at {} is invalid or out of date",
                    checkout_path.display()
                ));
            }
            Err(_) => {
                remove_invalid_shared_checkout(checkout_path, mirror_path)?;
            }
        }
    }

    if !allow_network {
        bail!(
            "missing shared checkout for `{normalized_url}` at {}",
            checkout_path.display()
        );
    }

    let parent = checkout_path.parent().ok_or_else(|| {
        anyhow!(
            "cannot determine parent directory for shared checkout {}",
            checkout_path.display()
        )
    })?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    reporter.status(
        "Updating",
        format!(
            "shared checkout {} for {}",
            short_display_rev(rev),
            normalized_url
        ),
    )?;
    clear_stale_shared_checkout_registration(mirror_path, checkout_path);
    git_run(mirror_path, ["config", "core.autocrlf", "false"])?;
    git_run(
        mirror_path,
        [
            "-c",
            "core.autocrlf=false",
            "worktree",
            "add",
            "--detach",
            checkout_path.to_string_lossy().as_ref(),
            rev,
        ],
    )
    .with_context(|| {
        format!(
            "failed to materialize shared checkout {} from shared mirror {}",
            checkout_path.display(),
            mirror_path.display(),
        )
    })?;
    git_run(checkout_path, ["config", "core.autocrlf", "false"])?;
    git_run(
        checkout_path,
        [
            "-c",
            "core.autocrlf=false",
            "checkout",
            "--detach",
            "--force",
            rev,
        ],
    )
}

fn remove_invalid_shared_checkout(checkout_path: &Path, mirror_path: &Path) -> Result<()> {
    clear_stale_shared_checkout_registration(mirror_path, checkout_path);

    match fs::remove_dir_all(checkout_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to remove invalid shared checkout {}",
                checkout_path.display()
            )
        }),
    }
}

fn clear_stale_shared_checkout_registration(mirror_path: &Path, checkout_path: &Path) {
    let checkout = checkout_path.to_string_lossy();
    let _ = git_run(
        mirror_path,
        ["worktree", "remove", "--force", checkout.as_ref()],
    );
}

fn short_display_rev(rev: &str) -> String {
    rev.chars().take(12).collect()
}

pub fn validate_shared_checkout(
    checkout_path: &Path,
    mirror_path: &Path,
    normalized_url: &str,
) -> Result<()> {
    let remote_url = git_output(checkout_path, ["remote", "get-url", "origin"])?;
    if !git_urls_match(remote_url.trim(), normalized_url) {
        bail!(
            "dependency checkout at {} has remote `{}` instead of `{}`",
            checkout_path.display(),
            remote_url.trim(),
            normalized_url
        );
    }

    let common_dir = git_output(
        checkout_path,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common_dir = PathBuf::from(common_dir.trim());
    let expected_common_dir = mirror_path
        .canonicalize()
        .with_context(|| format!("failed to access shared mirror {}", mirror_path.display()))?;
    let actual_common_dir = common_dir.canonicalize().with_context(|| {
        format!(
            "failed to resolve git common dir for shared checkout {}",
            checkout_path.display()
        )
    })?;

    if actual_common_dir != expected_common_dir {
        bail!(
            "shared checkout at {} is not backed by shared mirror {}",
            checkout_path.display(),
            mirror_path.display()
        );
    }

    Ok(())
}

fn is_bare_repository(path: &Path) -> Result<bool> {
    Ok(git_output(path, ["rev-parse", "--is-bare-repository"])? == "true")
}

fn git_run<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to run git in {}", cwd.display()))?;
    if !output.status.success() {
        bail!(
            "git {:?} failed in {}: {}",
            args,
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn git_output<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to run git in {}", cwd.display()))?;
    if !output.status.success() {
        bail!(
            "git {:?} failed in {}: {}",
            args,
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;
    use std::process::Command;

    use tempfile::TempDir;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::File::create(path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }

    fn init_git_repo(path: &Path) {
        let run = |args: &[&str]| {
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
        };

        run(&["init"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test User"]);
        write_file(&path.join(".gitattributes"), "* text eol=lf\n");
        run(&["add", "."]);
        run(&["commit", "-m", "initial"]);
    }

    fn rename_current_branch(path: &Path, branch: &str) {
        let output = Command::new("git")
            .args(["branch", "-m", branch])
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn normalizes_repo_names_into_aliases() {
        assert_eq!(
            normalize_alias_from_url("https://github.com/wenext-limited/playbook-ios").unwrap(),
            "playbook_ios"
        );
        assert_eq!(
            normalize_alias_from_url("git@github.com:foo/bar_baz.git").unwrap(),
            "bar_baz"
        );
        assert_eq!(
            normalize_alias_from_url("wenext-limited/playbook-ios").unwrap(),
            "playbook_ios"
        );
        assert_eq!(
            normalize_alias_from_url(r"C:\Users\runneradmin\AppData\Local\Temp\playbook-ios")
                .unwrap(),
            "playbook_ios"
        );
    }

    #[test]
    fn expands_github_shortcuts() {
        assert_eq!(
            normalize_git_url("wenext-limited/playbook-ios"),
            "https://github.com/wenext-limited/playbook-ios"
        );
        assert_eq!(
            normalize_git_url("https://github.com/wenext-limited/playbook-ios"),
            "https://github.com/wenext-limited/playbook-ios"
        );
        assert_eq!(
            normalize_git_url(r"C:\Users\runneradmin\AppData\Local\Temp\playbook-ios"),
            r"C:\Users\runneradmin\AppData\Local\Temp\playbook-ios"
        );
    }

    #[test]
    fn extracts_github_slugs_from_https_urls() {
        assert_eq!(
            github_slug_from_url("https://github.com/wenext-limited/playbook-ios"),
            Some("wenext-limited/playbook-ios".into())
        );
        assert_eq!(
            github_slug_from_url("wenext-limited/playbook-ios"),
            Some("wenext-limited/playbook-ios".into())
        );
        assert_eq!(
            github_slug_from_url("git@github.com:wenext-limited/playbook-ios.git"),
            None
        );
    }

    #[test]
    fn matches_equivalent_local_git_paths() {
        let temp = TempDir::new().unwrap();
        let canonical = temp.path().canonicalize().unwrap();
        let native = canonical.to_string_lossy().to_string();
        let forward = native.replace('\\', "/");

        assert!(git_urls_match(&native, &forward));
    }

    #[test]
    fn resolves_dependency_alias_from_exact_name() {
        let mut manifest = Manifest::default();
        manifest.dependencies.insert(
            "playbook_ios".into(),
            DependencySpec {
                github: None,
                url: Some("https://github.com/wenext-limited/playbook-ios".into()),
                path: None,
                subpath: None,
                tag: Some("v0.1.0".into()),
                branch: None,
                revision: None,
                version: None,
                components: None,
                members: None,
                managed: None,
                enabled: true,
            },
        );

        assert_eq!(
            resolve_dependency_alias(&manifest, "playbook_ios").unwrap(),
            "playbook_ios"
        );
    }

    #[test]
    fn resolves_dependency_alias_from_repository_reference() {
        let mut manifest = Manifest::default();
        manifest.dependencies.insert(
            "playbook_ios".into(),
            DependencySpec {
                github: None,
                url: Some("https://github.com/wenext-limited/playbook-ios".into()),
                path: None,
                subpath: None,
                tag: Some("v0.1.0".into()),
                branch: None,
                revision: None,
                version: None,
                components: None,
                members: None,
                managed: None,
                enabled: true,
            },
        );

        assert_eq!(
            resolve_dependency_alias(&manifest, "wenext-limited/playbook-ios").unwrap(),
            "playbook_ios"
        );
    }

    #[test]
    fn picks_latest_tag_by_version_sort() {
        let temp = TempDir::new().unwrap();
        write_file(&temp.path().join("README.md"), "hello\n");
        init_git_repo(temp.path());

        for tag in ["v0.1.0", "v1.2.0", "v0.9.0"] {
            let output = Command::new("git")
                .args(["tag", tag])
                .current_dir(temp.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        assert_eq!(latest_tag(temp.path()).unwrap(), "v1.2.0");
    }

    #[test]
    fn picks_latest_compatible_semver_tag() {
        let temp = TempDir::new().unwrap();
        write_file(&temp.path().join("README.md"), "hello\n");
        init_git_repo(temp.path());

        for tag in ["v0.9.0", "v1.2.0", "v1.4.0", "v2.0.0"] {
            let output = Command::new("git")
                .args(["tag", tag])
                .current_dir(temp.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        assert_eq!(
            latest_compatible_tag(temp.path(), &VersionReq::parse("^1.0.0").unwrap()).unwrap(),
            "v1.4.0"
        );
    }

    #[test]
    fn resolves_default_branch_when_repo_has_no_tags() {
        let cache_root = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        write_file(
            &repo.path().join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
        );
        init_git_repo(repo.path());
        rename_current_branch(repo.path(), "main");

        let reporter = Reporter::silent();
        let checkout = ensure_git_dependency(
            cache_root.path(),
            &repo.path().to_string_lossy(),
            None,
            true,
            &reporter,
        )
        .unwrap();

        assert_eq!(checkout.tag, None);
        assert_eq!(checkout.branch.as_deref(), Some("main"));
        assert_eq!(checkout.rev, current_rev(repo.path()).unwrap());
    }

    #[test]
    fn refreshes_default_branch_when_the_remote_head_changes() {
        let cache_root = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        write_file(
            &repo.path().join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
        );
        init_git_repo(repo.path());
        rename_current_branch(repo.path(), "main");

        let reporter = Reporter::silent();
        let initial = ensure_git_dependency(
            cache_root.path(),
            &repo.path().to_string_lossy(),
            None,
            true,
            &reporter,
        )
        .unwrap();
        assert_eq!(initial.branch.as_deref(), Some("main"));

        let output = Command::new("git")
            .args(["checkout", "-b", "trunk"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        write_file(&repo.path().join("rules/policy.md"), "keep moving\n");
        let output = Command::new("git")
            .args(["add", "."])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = Command::new("git")
            .args(["commit", "-m", "switch default branch"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let updated = ensure_git_dependency(
            cache_root.path(),
            &repo.path().to_string_lossy(),
            None,
            true,
            &reporter,
        )
        .unwrap();

        assert_eq!(updated.tag, None);
        assert_eq!(updated.branch.as_deref(), Some("trunk"));
        assert_eq!(updated.rev, current_rev(repo.path()).unwrap());
    }

    #[test]
    fn computes_shared_repository_path_from_the_normalized_url() {
        let cache_root = TempDir::new().unwrap();
        let path =
            shared_repository_path(cache_root.path(), "wenext-limited/playbook-ios").unwrap();

        assert_eq!(
            path,
            cache_root
                .path()
                .join("repositories")
                .join("playbook-ios-3fbb5d0f.git")
        );
    }

    #[test]
    fn computes_shared_checkout_path_from_the_normalized_url_and_revision() {
        let cache_root = TempDir::new().unwrap();
        let path = shared_checkout_path(
            cache_root.path(),
            "wenext-limited/playbook-ios",
            "abc123def456",
        )
        .unwrap();

        assert_eq!(
            path,
            cache_root
                .path()
                .join("checkouts")
                .join("playbook-ios-3fbb5d0f")
                .join("abc123def456")
        );
    }

    #[test]
    fn recreates_invalid_shared_repository_mirrors() {
        let cache_root = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        write_file(
            &repo.path().join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
        );
        init_git_repo(repo.path());

        let url = repo.path().to_string_lossy().to_string();
        let mirror_path = shared_repository_path(cache_root.path(), &url).unwrap();
        fs::create_dir_all(&mirror_path).unwrap();
        write_file(&mirror_path.join("README.txt"), "not a git repo\n");

        let recreated =
            prepare_repository_mirror(cache_root.path(), &url, true, &Reporter::silent()).unwrap();

        assert_eq!(recreated, mirror_path);
        assert!(is_bare_repository(&mirror_path).unwrap());
    }

    #[test]
    fn recreates_missing_registered_shared_checkouts() {
        let cache_root = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        write_file(
            &repo.path().join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
        );
        init_git_repo(repo.path());
        rename_current_branch(repo.path(), "main");

        let url = repo.path().to_string_lossy().to_string();
        let reporter = Reporter::silent();
        let initial =
            ensure_git_dependency(cache_root.path(), &url, None, true, &reporter).unwrap();

        fs::remove_dir_all(&initial.path).unwrap();

        let recovered =
            ensure_git_dependency(cache_root.path(), &url, None, true, &reporter).unwrap();

        assert_eq!(recovered.path, initial.path);
        assert!(recovered.path.is_dir());
        assert_eq!(recovered.rev, current_rev(repo.path()).unwrap());
    }
}
