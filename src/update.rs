use std::path::Path;

use anyhow::{Result, bail};

use crate::git::{ensure_git_dependency, latest_tag, prepare_repository_mirror};
use crate::lockfile::Lockfile;
use crate::manifest::{
    DependencySourceKind, RequestedGitRef, load_dependency_from_dir, load_root_from_dir,
    write_manifest,
};
use crate::report::Reporter;
use crate::resolver::sync_in_dir;

#[derive(Debug, Clone)]
pub struct UpdateSummary {
    pub updated_count: usize,
    pub managed_file_count: usize,
}

pub fn update_direct_dependencies_in_dir(
    cwd: &Path,
    cache_root: &Path,
    allow_high_sensitivity: bool,
    reporter: &Reporter,
) -> Result<UpdateSummary> {
    let mut root = load_root_from_dir(cwd)?;
    let dependency_count = root.manifest.dependencies.len();
    if dependency_count == 0 {
        reporter.note("no direct dependencies configured")?;
        return Ok(UpdateSummary {
            updated_count: 0,
            managed_file_count: 0,
        });
    }

    reporter.status(
        "Checking",
        format!("{dependency_count} direct dependencies"),
    )?;
    let existing_lockfile = load_lockfile(cwd)?;
    let mut updated_count = 0;
    let mut manifest_changed = false;

    for (alias, dependency) in &mut root.manifest.dependencies {
        match dependency.source_kind()? {
            DependencySourceKind::Path => {}
            DependencySourceKind::Git => {
                let url = dependency.resolved_git_url()?;
                match dependency.requested_git_ref()? {
                    RequestedGitRef::Tag(current_tag) => {
                        let mirror = prepare_repository_mirror(cache_root, &url, true, reporter)?;
                        let latest = latest_tag(&mirror)?;
                        if latest != current_tag {
                            reporter
                                .note(format!("updating {alias} tag {current_tag} -> {latest}"))?;
                            dependency.tag = Some(latest);
                            updated_count += 1;
                            manifest_changed = true;
                        }
                    }
                    RequestedGitRef::Branch(branch) => {
                        let checkout = ensure_git_dependency(
                            cache_root,
                            &url,
                            None,
                            Some(branch),
                            true,
                            reporter,
                        )?;
                        let latest_version = load_dependency_from_dir(&checkout.path)?
                            .effective_version()
                            .or_else(|| dependency.version.clone());
                        let locked_rev = locked_rev(existing_lockfile.as_ref(), alias);
                        if locked_rev.as_deref() != Some(checkout.rev.as_str()) {
                            let previous = locked_rev
                                .map(|rev| short_rev(&rev))
                                .unwrap_or_else(|| "none".into());
                            reporter.note(format!(
                                "updating {alias} branch {branch} {previous} -> {}",
                                short_rev(&checkout.rev)
                            ))?;
                            updated_count += 1;
                        }
                        if dependency.version != latest_version {
                            dependency.version = latest_version;
                            manifest_changed = true;
                        }
                    }
                }
            }
        }
    }

    if manifest_changed {
        reporter.status(
            "Writing",
            cwd.join(crate::manifest::MANIFEST_FILE).display(),
        )?;
        write_manifest(&cwd.join(crate::manifest::MANIFEST_FILE), &root.manifest)?;
    }

    let sync_summary = sync_in_dir(cwd, cache_root, false, allow_high_sensitivity, reporter)?;
    if updated_count == 0 && sync_summary.package_count == 0 {
        bail!("project contains no packages to sync");
    }

    Ok(UpdateSummary {
        updated_count,
        managed_file_count: sync_summary.managed_file_count,
    })
}

fn load_lockfile(cwd: &Path) -> Result<Option<Lockfile>> {
    let path = cwd.join(crate::lockfile::LOCKFILE_NAME);
    if path.exists() {
        Ok(Some(Lockfile::read(&path)?))
    } else {
        Ok(None)
    }
}

fn locked_rev(lockfile: Option<&Lockfile>, alias: &str) -> Option<String> {
    lockfile?
        .packages
        .iter()
        .find(|package| package.alias == alias)
        .and_then(|package| package.source.rev.clone())
}

fn short_rev(rev: &str) -> String {
    rev.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use semver::Version;
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
            Some("v0.1.0"),
            crate::git::AddDependencyOptions {
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
            None,
            crate::git::AddDependencyOptions {
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
        assert_eq!(
            dependency.version.as_ref(),
            Some(&Version::parse("1.1.0").unwrap())
        );
        assert_eq!(
            locked.source.rev.as_deref(),
            Some(current_rev(repo.path()).unwrap().as_str())
        );
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
