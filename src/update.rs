use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, bail};
use rayon::prelude::*;

use crate::domain::dependency_status::{
    display_dependency_alias, load_lockfile, locked_rev, locked_tag, short_identifier,
};
use crate::execution::ExecutionMode;
use crate::git::{
    ensure_git_dependency, latest_compatible_tag, latest_tag, prepare_repository_mirror,
};
use crate::lockfile::Lockfile;
use crate::manifest::{
    DependencyKind, DependencySourceKind, DependencySpec, PackageRole, RequestedGitRef,
    load_root_from_dir,
};
use crate::report::Reporter;
use crate::resolver::sync_in_dir_with_loaded_root;

#[derive(Debug, Clone)]
pub struct UpdateSummary {
    pub updated_count: usize,
    pub managed_file_count: usize,
}

#[derive(Debug, Clone)]
struct DependencySnapshot {
    alias: String,
    spec: DependencySpec,
}

#[derive(Debug, Clone)]
enum DependencyUpdatePlan {
    Path,
    GitTag {
        current_tag: String,
        latest_tag: String,
    },
    GitBranch {
        branch: String,
        locked_rev: Option<String>,
        latest_rev: String,
    },
    GitSemver {
        requirement: String,
        locked_tag: Option<String>,
        latest_tag: String,
    },
    GitRevision,
}

pub fn update_direct_dependencies_in_dir(
    cwd: &Path,
    cache_root: &Path,
    allow_high_sensitivity: bool,
    reporter: &Reporter,
) -> Result<UpdateSummary> {
    update_direct_dependencies_in_dir_mode(
        cwd,
        cache_root,
        allow_high_sensitivity,
        ExecutionMode::Apply,
        reporter,
    )
}

pub fn update_direct_dependencies_in_dir_dry_run(
    cwd: &Path,
    cache_root: &Path,
    allow_high_sensitivity: bool,
    reporter: &Reporter,
) -> Result<UpdateSummary> {
    update_direct_dependencies_in_dir_mode(
        cwd,
        cache_root,
        allow_high_sensitivity,
        ExecutionMode::DryRun,
        reporter,
    )
}

fn update_direct_dependencies_in_dir_mode(
    cwd: &Path,
    cache_root: &Path,
    allow_high_sensitivity: bool,
    execution_mode: ExecutionMode,
    reporter: &Reporter,
) -> Result<UpdateSummary> {
    crate::relay::ensure_no_pending_relay_edits_in_dir(cwd, cache_root)?;
    let mut root = load_root_from_dir(cwd)?;
    let dependency_count = root.manifest.active_dependency_entries().len();
    if dependency_count == 0 {
        reporter.note("no dependencies configured")?;
        return Ok(UpdateSummary {
            updated_count: 0,
            managed_file_count: 0,
        });
    }

    reporter.status("Checking", format!("{dependency_count} dependencies"))?;
    let existing_lockfile = load_lockfile(cwd)?;
    let dependencies = root
        .manifest
        .active_dependency_entries()
        .into_iter()
        .map(|entry| DependencySnapshot {
            alias: entry.alias.to_string(),
            spec: entry.spec.clone(),
        })
        .collect::<Vec<_>>();
    let plans = plan_dependency_updates(&dependencies, existing_lockfile.as_ref(), cache_root)?;
    let mut updated_count = 0;

    for kind in [DependencyKind::Dependency, DependencyKind::DevDependency] {
        for (alias, dependency) in root.manifest.dependency_section_mut(kind) {
            let plan = plans
                .get(alias.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing dependency update plan for `{alias}`"))?;
            match plan {
                DependencyUpdatePlan::Path => {}
                DependencyUpdatePlan::GitTag {
                    current_tag,
                    latest_tag,
                } => {
                    if latest_tag != current_tag {
                        reporter.note(format!(
                            "updating {} tag {current_tag} -> {latest_tag}",
                            display_alias(alias, kind)
                        ))?;
                        dependency.tag = Some(latest_tag.clone());
                        updated_count += 1;
                    }
                }
                DependencyUpdatePlan::GitBranch {
                    branch,
                    locked_rev,
                    latest_rev,
                } => {
                    if locked_rev.as_deref() != Some(latest_rev.as_str()) {
                        let previous = locked_rev
                            .as_ref()
                            .map(|rev| short_rev(rev))
                            .unwrap_or_else(|| "none".into());
                        reporter.note(format!(
                            "updating {} branch {branch} {previous} -> {}",
                            display_alias(alias, kind),
                            short_rev(latest_rev)
                        ))?;
                        updated_count += 1;
                    }
                }
                DependencyUpdatePlan::GitSemver {
                    requirement,
                    locked_tag,
                    latest_tag,
                } => {
                    if locked_tag.as_deref() != Some(latest_tag.as_str()) {
                        let previous = locked_tag.as_deref().unwrap_or("none");
                        reporter.note(format!(
                            "updating {} version {requirement} {previous} -> {latest_tag}",
                            display_alias(alias, kind),
                        ))?;
                        updated_count += 1;
                    }
                }
                DependencyUpdatePlan::GitRevision => {}
            }
        }
    }

    let root = root.with_manifest(root.manifest.clone(), PackageRole::Root)?;
    let sync_summary = sync_in_dir_with_loaded_root(
        cwd,
        cache_root,
        false,
        allow_high_sensitivity,
        &[],
        false,
        execution_mode,
        root,
        reporter,
    )?;
    if updated_count == 0 && sync_summary.package_count == 0 {
        bail!("project contains no packages to sync");
    }

    Ok(UpdateSummary {
        updated_count,
        managed_file_count: sync_summary.managed_file_count,
    })
}

fn plan_dependency_updates(
    dependencies: &[DependencySnapshot],
    existing_lockfile: Option<&Lockfile>,
    cache_root: &Path,
) -> Result<BTreeMap<String, DependencyUpdatePlan>> {
    let mut plans = BTreeMap::new();
    let mut git_groups = BTreeMap::<String, Vec<&DependencySnapshot>>::new();

    for dependency in dependencies {
        match dependency.spec.source_kind()? {
            DependencySourceKind::Path => {
                plans.insert(dependency.alias.clone(), DependencyUpdatePlan::Path);
            }
            DependencySourceKind::Git => {
                let url = dependency.spec.resolved_git_url()?;
                git_groups.entry(url).or_default().push(dependency);
            }
        }
    }

    let git_plans = git_groups
        .into_par_iter()
        .map(|(url, dependencies)| {
            let reporter = Reporter::silent();
            let mut latest_tag_name = None;
            let mut branch_updates = BTreeMap::<String, String>::new();
            let mut compatible_tag_updates = BTreeMap::<String, String>::new();
            let mut group_plans = Vec::with_capacity(dependencies.len());

            for dependency in dependencies {
                let plan = match dependency.spec.requested_git_ref()? {
                    RequestedGitRef::Tag(current_tag) => {
                        let latest_tag = match latest_tag_name.clone() {
                            Some(tag) => tag,
                            None => {
                                let mirror =
                                    prepare_repository_mirror(cache_root, &url, true, &reporter)?;
                                let tag = latest_tag(&mirror)?;
                                latest_tag_name = Some(tag.clone());
                                tag
                            }
                        };
                        DependencyUpdatePlan::GitTag {
                            current_tag: current_tag.to_string(),
                            latest_tag,
                        }
                    }
                    RequestedGitRef::Branch(branch) => {
                        let latest_rev = match branch_updates.get(branch) {
                            Some(rev) => rev.clone(),
                            None => {
                                let checkout = ensure_git_dependency(
                                    cache_root,
                                    &url,
                                    Some(RequestedGitRef::Branch(branch)),
                                    true,
                                    &reporter,
                                )?;
                                branch_updates.insert(branch.to_string(), checkout.rev.clone());
                                checkout.rev
                            }
                        };
                        DependencyUpdatePlan::GitBranch {
                            branch: branch.to_string(),
                            locked_rev: locked_rev(existing_lockfile, &dependency.alias),
                            latest_rev,
                        }
                    }
                    RequestedGitRef::VersionReq(requirement) => {
                        let latest_tag = match compatible_tag_updates.get(&requirement.to_string())
                        {
                            Some(tag) => tag.clone(),
                            None => {
                                let mirror =
                                    prepare_repository_mirror(cache_root, &url, true, &reporter)?;
                                let tag = latest_compatible_tag(&mirror, requirement)?;
                                compatible_tag_updates.insert(requirement.to_string(), tag.clone());
                                tag
                            }
                        };
                        DependencyUpdatePlan::GitSemver {
                            requirement: requirement.to_string(),
                            locked_tag: locked_tag(existing_lockfile, &dependency.alias),
                            latest_tag,
                        }
                    }
                    RequestedGitRef::Revision(_) => DependencyUpdatePlan::GitRevision,
                };
                group_plans.push((dependency.alias.clone(), plan));
            }

            Ok(group_plans)
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<Result<Vec<_>>>()?;

    for group in git_plans {
        for (alias, plan) in group {
            plans.insert(alias, plan);
        }
    }

    Ok(plans)
}

fn short_rev(rev: &str) -> String {
    short_identifier(rev)
}

fn display_alias(alias: &str, kind: DependencyKind) -> String {
    display_dependency_alias(alias, kind)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;
    use crate::adapters::Adapter;
    use crate::git::{add_dependency_in_dir_with_adapters, current_rev};
    use crate::manifest::load_root_from_dir;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn write_skill(path: &Path, name: &str) {
        write_file(
            &path.join("SKILL.md"),
            &format!("---\nname: {name}\ndescription: Example skill.\n---\n# {name}\n"),
        );
    }

    fn init_git_repo(path: &Path) {
        run_git(path, &["init"]);
        run_git(path, &["config", "user.email", "test@example.com"]);
        run_git(path, &["config", "user.name", "Test User"]);
        run_git(path, &["add", "."]);
        run_git(path, &["commit", "-m", "initial"]);
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

    fn rename_current_branch(path: &Path, branch: &str) {
        run_git(path, &["branch", "-m", branch]);
    }

    #[test]
    fn updates_tagged_direct_dependencies_to_the_latest_tag() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        init_git_repo(repo.path());
        run_git(repo.path(), &["tag", "v0.1.0"]);

        add_dependency_in_dir_with_adapters(
            project.path(),
            cache.path(),
            &repo.path().to_string_lossy(),
            crate::git::AddDependencyOptions {
                git_ref: Some(RequestedGitRef::Tag("v0.1.0")),
                version_req: None,
                kind: DependencyKind::Dependency,
                adapters: &[Adapter::Codex],
                components: &[],
                sync_on_launch: false,
            },
            &Reporter::silent(),
        )
        .unwrap();

        run_git(repo.path(), &["tag", "v0.2.0"]);

        let summary = update_direct_dependencies_in_dir(
            project.path(),
            cache.path(),
            false,
            &Reporter::silent(),
        )
        .unwrap();

        let manifest =
            fs::read_to_string(project.path().join(crate::manifest::MANIFEST_FILE)).unwrap();
        let lockfile =
            Lockfile::read(&project.path().join(crate::lockfile::LOCKFILE_NAME)).unwrap();
        let dependency = lockfile
            .packages
            .iter()
            .find(|package| package.alias != "root")
            .unwrap();

        assert_eq!(summary.updated_count, 1);
        assert!(manifest.contains("tag = \"v0.2.0\""));
        assert_eq!(dependency.version_tag.as_deref(), Some("v0.2.0"));
    }

    #[test]
    fn updates_branch_direct_dependencies_to_the_latest_revision() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        write_file(&repo.path().join("nodus.toml"), "version = \"1.0.0\"\n");
        init_git_repo(repo.path());
        rename_current_branch(repo.path(), "main");

        add_dependency_in_dir_with_adapters(
            project.path(),
            cache.path(),
            &repo.path().to_string_lossy(),
            crate::git::AddDependencyOptions {
                git_ref: None,
                version_req: None,
                kind: DependencyKind::Dependency,
                adapters: &[Adapter::Codex],
                components: &[],
                sync_on_launch: false,
            },
            &Reporter::silent(),
        )
        .unwrap();

        write_file(&repo.path().join("rules/policy.md"), "# Policy\n");
        write_file(&repo.path().join("nodus.toml"), "version = \"1.1.0\"\n");
        run_git(repo.path(), &["add", "."]);
        run_git(repo.path(), &["commit", "-m", "advance"]);

        let summary = update_direct_dependencies_in_dir(
            project.path(),
            cache.path(),
            false,
            &Reporter::silent(),
        )
        .unwrap();

        let root = load_root_from_dir(project.path()).unwrap();
        let dependency = root.manifest.dependencies.values().next().unwrap();
        let lockfile =
            Lockfile::read(&project.path().join(crate::lockfile::LOCKFILE_NAME)).unwrap();
        let locked = lockfile
            .packages
            .iter()
            .find(|package| package.alias != "root")
            .unwrap();

        assert_eq!(summary.updated_count, 1);
        assert_eq!(dependency.branch.as_deref(), Some("main"));
        assert!(dependency.version.is_none());
        assert_eq!(
            locked.source.rev.as_deref(),
            Some(current_rev(repo.path()).unwrap().as_str())
        );
    }

    #[test]
    fn keeps_revision_pinned_dependencies_at_the_requested_commit() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        init_git_repo(repo.path());
        let revision = current_rev(repo.path()).unwrap();

        add_dependency_in_dir_with_adapters(
            project.path(),
            cache.path(),
            &repo.path().to_string_lossy(),
            crate::git::AddDependencyOptions {
                git_ref: Some(RequestedGitRef::Revision(revision.as_str())),
                version_req: None,
                kind: DependencyKind::Dependency,
                adapters: &[Adapter::Codex],
                components: &[],
                sync_on_launch: false,
            },
            &Reporter::silent(),
        )
        .unwrap();

        write_file(&repo.path().join("rules/policy.md"), "# Policy\n");
        run_git(repo.path(), &["add", "."]);
        run_git(repo.path(), &["commit", "-m", "advance"]);

        let summary = update_direct_dependencies_in_dir(
            project.path(),
            cache.path(),
            false,
            &Reporter::silent(),
        )
        .unwrap();

        let root = load_root_from_dir(project.path()).unwrap();
        let dependency = root.manifest.dependencies.values().next().unwrap();
        let lockfile =
            Lockfile::read(&project.path().join(crate::lockfile::LOCKFILE_NAME)).unwrap();
        let locked = lockfile
            .packages
            .iter()
            .find(|package| package.alias != "root")
            .unwrap();

        assert_eq!(summary.updated_count, 0);
        assert_eq!(dependency.revision.as_deref(), Some(revision.as_str()));
        assert_eq!(locked.source.rev.as_deref(), Some(revision.as_str()));
    }

    #[test]
    fn updates_dev_dependencies() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        init_git_repo(repo.path());
        run_git(repo.path(), &["tag", "v0.1.0"]);

        add_dependency_in_dir_with_adapters(
            project.path(),
            cache.path(),
            &repo.path().to_string_lossy(),
            crate::git::AddDependencyOptions {
                git_ref: Some(RequestedGitRef::Tag("v0.1.0")),
                version_req: None,
                kind: DependencyKind::DevDependency,
                adapters: &[Adapter::Codex],
                components: &[],
                sync_on_launch: false,
            },
            &Reporter::silent(),
        )
        .unwrap();

        run_git(repo.path(), &["tag", "v0.2.0"]);

        let summary = update_direct_dependencies_in_dir(
            project.path(),
            cache.path(),
            false,
            &Reporter::silent(),
        )
        .unwrap();

        let root = load_root_from_dir(project.path()).unwrap();
        let dependency = root.manifest.dev_dependencies.values().next().unwrap();

        assert_eq!(summary.updated_count, 1);
        assert_eq!(dependency.tag.as_deref(), Some("v0.2.0"));
    }

    #[test]
    fn updates_semver_managed_dependencies_within_requirement() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        init_git_repo(repo.path());
        run_git(repo.path(), &["tag", "v1.0.0"]);

        add_dependency_in_dir_with_adapters(
            project.path(),
            cache.path(),
            &repo.path().to_string_lossy(),
            crate::git::AddDependencyOptions {
                git_ref: None,
                version_req: Some(semver::VersionReq::parse("^1.0.0").unwrap()),
                kind: DependencyKind::Dependency,
                adapters: &[Adapter::Codex],
                components: &[],
                sync_on_launch: false,
            },
            &Reporter::silent(),
        )
        .unwrap();

        run_git(repo.path(), &["tag", "v1.2.0"]);
        run_git(repo.path(), &["tag", "v2.0.0"]);

        let summary = update_direct_dependencies_in_dir(
            project.path(),
            cache.path(),
            false,
            &Reporter::silent(),
        )
        .unwrap();

        let manifest =
            fs::read_to_string(project.path().join(crate::manifest::MANIFEST_FILE)).unwrap();
        let lockfile =
            Lockfile::read(&project.path().join(crate::lockfile::LOCKFILE_NAME)).unwrap();
        let dependency = lockfile
            .packages
            .iter()
            .find(|package| package.alias != "root")
            .unwrap();

        assert_eq!(summary.updated_count, 1);
        assert!(manifest.contains("version = \"^1.0.0\""));
        assert!(!manifest.contains("tag = "));
        assert_eq!(dependency.source.tag.as_deref(), Some("v1.2.0"));
    }

    #[test]
    fn reports_when_no_direct_dependencies_are_configured() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();

        let summary = update_direct_dependencies_in_dir(
            project.path(),
            cache.path(),
            false,
            &Reporter::silent(),
        )
        .unwrap();

        assert_eq!(summary.updated_count, 0);
        assert_eq!(summary.managed_file_count, 0);
    }
}
