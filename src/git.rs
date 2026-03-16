use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use crate::adapters::Adapter;
use crate::manifest::{
    DependencyComponent, DependencySpec, MANIFEST_FILE, PackageRole, load_dependency_from_dir,
    load_from_dir, normalize_dependency_alias, write_manifest,
};
use crate::report::Reporter;
use crate::resolver::{sync_in_dir, sync_in_dir_with_adapters};
use crate::selection::{resolve_adapter_selection, should_prompt_for_adapter};

#[derive(Debug, Clone, Copy)]
enum RequestedGitRef<'a> {
    Tag(&'a str),
    Branch(&'a str),
}

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
    pub reference: String,
    pub adapters: Vec<Adapter>,
    pub managed_file_count: usize,
}

#[derive(Debug, Clone)]
pub struct RemoveSummary {
    pub alias: String,
    pub managed_file_count: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct AddDependencyOptions<'a> {
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

#[allow(dead_code)]
pub fn add_dependency_with_adapters(
    cache_root: &Path,
    url: &str,
    tag: Option<&str>,
    options: AddDependencyOptions<'_>,
    reporter: &Reporter,
) -> Result<AddSummary> {
    let cwd = std::env::current_dir().context("failed to determine the current directory")?;
    add_dependency_in_dir_with_adapters(&cwd, cache_root, url, tag, options, reporter)
}

#[allow(dead_code)]
pub fn remove_dependency(
    cache_root: &Path,
    package: &str,
    reporter: &Reporter,
) -> Result<RemoveSummary> {
    let cwd = std::env::current_dir().context("failed to determine the current directory")?;
    remove_dependency_in_dir(&cwd, cache_root, package, reporter)
}

pub fn add_dependency_in_dir_with_adapters(
    project_root: &Path,
    cache_root: &Path,
    url: &str,
    tag: Option<&str>,
    options: AddDependencyOptions<'_>,
    reporter: &Reporter,
) -> Result<AddSummary> {
    let normalized_url = normalize_git_url(url);
    let alias = normalize_alias_from_url(&normalized_url)?;
    let checkout = ensure_git_dependency(cache_root, &normalized_url, tag, None, true, reporter)?;
    let github = github_slug_from_url(&checkout.url);
    let dependency_manifest = load_dependency_from_dir(&checkout.path)
        .with_context(|| format!("dependency `{alias}` does not match the Nodus package layout"))?;

    let mut root = load_from_dir(project_root, PackageRole::Root)?;
    if root.manifest.dependencies.contains_key(&alias) {
        bail!(
            "dependency `{alias}` already exists in {}",
            project_root.display()
        );
    }
    reporter.status(
        "Adding",
        format!(
            "{alias} {} to {}",
            checkout.reference_display(),
            project_root.join(MANIFEST_FILE).display()
        ),
    )?;
    root.manifest.dependencies.insert(
        alias.clone(),
        DependencySpec {
            github: github.clone(),
            url: github.is_none().then_some(checkout.url.clone()),
            path: None,
            tag: checkout.tag.clone(),
            branch: checkout.branch.clone(),
            version: checkout
                .branch
                .as_ref()
                .and(dependency_manifest.effective_version()),
            components: (!options.components.is_empty()).then(|| {
                let mut sorted = options.components.to_vec();
                sorted.sort();
                sorted.dedup();
                sorted
            }),
        },
    );
    let selection = resolve_adapter_selection(
        project_root,
        &root.manifest,
        options.adapters,
        should_prompt_for_adapter(),
    )?;
    if selection.should_persist {
        root.manifest.set_enabled_adapters(&selection.adapters);
    }
    if options.sync_on_launch {
        root.manifest.set_sync_on_launch(true);
    }

    reporter.status("Writing", project_root.join(MANIFEST_FILE).display())?;
    write_manifest(&project_root.join(MANIFEST_FILE), &root.manifest)?;
    let sync_summary = sync_in_dir_with_adapters(
        project_root,
        cache_root,
        false,
        false,
        options.adapters,
        false,
        reporter,
    )?;

    Ok(AddSummary {
        alias,
        reference: checkout.reference_display(),
        adapters: sync_summary.adapters,
        managed_file_count: sync_summary.managed_file_count,
    })
}

pub fn remove_dependency_in_dir(
    project_root: &Path,
    cache_root: &Path,
    package: &str,
    reporter: &Reporter,
) -> Result<RemoveSummary> {
    let mut root = load_from_dir(project_root, PackageRole::Root)?;
    let alias = resolve_dependency_alias(&root.manifest.dependencies, package)?;
    reporter.status(
        "Removing",
        format!(
            "{alias} from {}",
            project_root.join(MANIFEST_FILE).display()
        ),
    )?;
    root.manifest.dependencies.remove(&alias);

    reporter.status("Writing", project_root.join(MANIFEST_FILE).display())?;
    write_manifest(&project_root.join(MANIFEST_FILE), &root.manifest)?;
    let sync_summary = sync_in_dir(project_root, cache_root, false, false, reporter)?;

    Ok(RemoveSummary {
        alias,
        managed_file_count: sync_summary.managed_file_count,
    })
}

pub fn ensure_git_dependency(
    cache_root: &Path,
    url: &str,
    tag: Option<&str>,
    branch: Option<&str>,
    allow_network: bool,
    reporter: &Reporter,
) -> Result<GitCheckout> {
    let normalized_url = normalize_git_url(url);
    let mirror_path = shared_repository_path(cache_root, &normalized_url)?;
    ensure_shared_repository(&mirror_path, &normalized_url, allow_network, reporter)?;

    let requested_ref = match (tag, branch) {
        (Some(tag), None) => Some(RequestedGitRef::Tag(tag)),
        (None, Some(branch)) => Some(RequestedGitRef::Branch(branch)),
        (Some(_), Some(_)) => bail!("git dependency must not request both `tag` and `branch`"),
        (None, None) => None,
    };
    let (resolved_tag, resolved_branch) = if let Some(requested_ref) = requested_ref {
        match requested_ref {
            RequestedGitRef::Tag(value) => (Some(value.to_string()), None),
            RequestedGitRef::Branch(value) => (None, Some(value.to_string())),
        }
    } else {
        reporter.status("Resolving", format!("latest tag for {normalized_url}"))?;
        match latest_tag_name(&mirror_path)? {
            Some(tag) => (Some(tag), None),
            None => {
                let branch = default_branch(&mirror_path)?;
                reporter.note(format!(
                    "no git tags found for {normalized_url}; using default branch {branch}"
                ))?;
                (None, Some(branch))
            }
        }
    };
    let resolved_ref = resolved_tag
        .as_deref()
        .or(resolved_branch.as_deref())
        .ok_or_else(|| anyhow!("failed to resolve git reference for {normalized_url}"))?;
    let rev = resolve_ref_to_rev(&mirror_path, resolved_ref)?;
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

#[allow(dead_code)]
pub fn latest_tag(path: &Path) -> Result<String> {
    latest_tag_name(path)?.ok_or_else(|| anyhow!("no git tags found in {}", path.display()))
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

pub fn default_branch(path: &Path) -> Result<String> {
    git_output(path, ["symbolic-ref", "--short", "HEAD"])
        .with_context(|| format!("failed to determine default branch for {}", path.display()))
}

pub fn normalize_git_url(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("git@")
        || trimmed.starts_with("ssh://")
        || trimmed.starts_with('/')
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
    let trimmed = normalized.trim_end_matches('/').trim_end_matches(".git");
    let tail = trimmed
        .rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("failed to infer a dependency alias from `{url}`"))?;
    normalize_dependency_alias(tail)
        .with_context(|| format!("failed to infer a dependency alias from `{url}`"))
}

fn resolve_dependency_alias(
    dependencies: &std::collections::BTreeMap<String, DependencySpec>,
    package: &str,
) -> Result<String> {
    if dependencies.contains_key(package) {
        return Ok(package.to_string());
    }

    let normalized = normalize_alias_from_url(package)?;
    if dependencies.contains_key(&normalized) {
        return Ok(normalized);
    }

    bail!("dependency `{package}` does not exist")
}

fn normalize_repository_name_from_url(url: &str) -> Result<String> {
    let normalized = normalize_git_url(url);
    let trimmed = normalized.trim_end_matches('/').trim_end_matches(".git");
    let tail = trimmed
        .rsplit('/')
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
        if !is_bare_repository(mirror_path)? {
            bail!(
                "shared repository mirror at {} is not a bare git repository",
                mirror_path.display()
            );
        }

        let remote_url = git_output(mirror_path, ["remote", "get-url", "origin"])?;
        if remote_url.trim() != normalized_url {
            bail!(
                "shared repository mirror at {} has remote `{}` instead of `{}`",
                mirror_path.display(),
                remote_url.trim(),
                normalized_url
            );
        }

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
        return Ok(());
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
            "clone",
            "--bare",
            normalized_url,
            mirror_path.to_string_lossy().as_ref(),
        ],
    )
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
        validate_shared_checkout(checkout_path, mirror_path, normalized_url)?;
        git_run(checkout_path, ["checkout", "--detach", rev])?;
        return Ok(());
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
    git_run(
        mirror_path,
        [
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
    })
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
    if remote_url.trim() != normalized_url {
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
    fn resolves_dependency_alias_from_exact_name() {
        let mut dependencies = std::collections::BTreeMap::new();
        dependencies.insert(
            "playbook_ios".into(),
            DependencySpec {
                github: None,
                url: Some("https://github.com/wenext-limited/playbook-ios".into()),
                path: None,
                tag: Some("v0.1.0".into()),
                branch: None,
                version: None,
                components: None,
            },
        );

        assert_eq!(
            resolve_dependency_alias(&dependencies, "playbook_ios").unwrap(),
            "playbook_ios"
        );
    }

    #[test]
    fn resolves_dependency_alias_from_repository_reference() {
        let mut dependencies = std::collections::BTreeMap::new();
        dependencies.insert(
            "playbook_ios".into(),
            DependencySpec {
                github: None,
                url: Some("https://github.com/wenext-limited/playbook-ios".into()),
                path: None,
                tag: Some("v0.1.0".into()),
                branch: None,
                version: None,
                components: None,
            },
        );

        assert_eq!(
            resolve_dependency_alias(&dependencies, "wenext-limited/playbook-ios").unwrap(),
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
}
