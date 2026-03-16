use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::git::{ensure_git_dependency, latest_tag, prepare_repository_mirror};
use crate::lockfile::Lockfile;
use crate::manifest::{DependencySourceKind, DependencySpec, RequestedGitRef, load_root_from_dir};
use crate::report::Reporter;

#[derive(Debug, Clone)]
pub struct OutdatedSummary {
    pub dependency_count: usize,
    pub outdated_count: usize,
}

#[derive(Debug, Clone)]
enum DependencyStatus {
    Path {
        path: PathBuf,
    },
    GitTagCurrent {
        tag: String,
    },
    GitTagOutdated {
        current: String,
        latest: String,
    },
    GitBranchCurrent {
        branch: String,
        rev: String,
    },
    GitBranchOutdated {
        branch: String,
        current_rev: String,
        latest_rev: String,
    },
    GitBranchUnlocked {
        branch: String,
        latest_rev: String,
    },
}

#[derive(Debug, Clone)]
struct DependencyReport {
    alias: String,
    status: DependencyStatus,
}

impl DependencyReport {
    fn is_outdated(&self) -> bool {
        matches!(
            self.status,
            DependencyStatus::GitTagOutdated { .. } | DependencyStatus::GitBranchOutdated { .. }
        )
    }

    fn render(&self) -> String {
        let status = match &self.status {
            DependencyStatus::Path { path } => format!("path {}", display_path(path)),
            DependencyStatus::GitTagCurrent { tag } => format!("current at tag {tag}"),
            DependencyStatus::GitTagOutdated { current, latest } => {
                format!("outdated: tag {current} -> {latest}")
            }
            DependencyStatus::GitBranchCurrent { branch, rev } => {
                format!("current on branch {branch} at {}", short_rev(rev))
            }
            DependencyStatus::GitBranchOutdated {
                branch,
                current_rev,
                latest_rev,
            } => format!(
                "outdated: branch {branch} {} -> {}",
                short_rev(current_rev),
                short_rev(latest_rev)
            ),
            DependencyStatus::GitBranchUnlocked { branch, latest_rev } => format!(
                "branch {branch} at {} (not locked locally)",
                short_rev(latest_rev)
            ),
        };
        format!("{:<20} {}", self.alias, status)
    }
}

pub fn check_outdated_in_dir(
    cwd: &Path,
    cache_root: &Path,
    reporter: &Reporter,
) -> Result<OutdatedSummary> {
    let root = load_root_from_dir(cwd)?;
    let dependency_count = root.manifest.dependencies.len();
    if dependency_count == 0 {
        reporter.note("no direct dependencies configured")?;
        return Ok(OutdatedSummary {
            dependency_count,
            outdated_count: 0,
        });
    }

    reporter.status(
        "Checking",
        format!("{dependency_count} direct dependencies"),
    )?;
    let lockfile = load_lockfile(cwd)?;
    let mut reports = Vec::new();
    for (alias, dependency) in &root.manifest.dependencies {
        reports.push(check_dependency(
            alias,
            dependency,
            lockfile.as_ref(),
            reporter,
            cache_root,
        )?);
    }

    let outdated_count = reports.iter().filter(|report| report.is_outdated()).count();
    for report in reports {
        reporter.line(report.render())?;
    }

    Ok(OutdatedSummary {
        dependency_count,
        outdated_count,
    })
}

fn check_dependency(
    alias: &str,
    dependency: &DependencySpec,
    lockfile: Option<&Lockfile>,
    reporter: &Reporter,
    cache_root: &Path,
) -> Result<DependencyReport> {
    let status = match dependency.source_kind()? {
        DependencySourceKind::Path => {
            let path = dependency
                .path
                .clone()
                .with_context(|| format!("dependency `{alias}` must declare `path`"))?;
            DependencyStatus::Path { path }
        }
        DependencySourceKind::Git => {
            let url = dependency.resolved_git_url()?;
            let mirror_path = prepare_repository_mirror(cache_root, &url, true, reporter)?;
            match dependency.requested_git_ref()? {
                RequestedGitRef::Tag(tag) => {
                    let latest = latest_tag(&mirror_path)?;
                    if latest == tag {
                        DependencyStatus::GitTagCurrent {
                            tag: tag.to_string(),
                        }
                    } else {
                        DependencyStatus::GitTagOutdated {
                            current: tag.to_string(),
                            latest,
                        }
                    }
                }
                RequestedGitRef::Branch(branch) => {
                    let latest_rev = ensure_git_dependency(
                        cache_root,
                        &url,
                        None,
                        Some(branch),
                        true,
                        reporter,
                    )?
                    .rev;
                    match locked_rev(lockfile, alias) {
                        Some(current_rev) if current_rev == latest_rev => {
                            DependencyStatus::GitBranchCurrent {
                                branch: branch.to_string(),
                                rev: current_rev,
                            }
                        }
                        Some(current_rev) => DependencyStatus::GitBranchOutdated {
                            branch: branch.to_string(),
                            current_rev,
                            latest_rev,
                        },
                        None => DependencyStatus::GitBranchUnlocked {
                            branch: branch.to_string(),
                            latest_rev,
                        },
                    }
                }
            }
        }
    };

    Ok(DependencyReport {
        alias: alias.to_string(),
        status,
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

fn display_path(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        ".".into()
    } else {
        path.to_string_lossy().replace('\\', "/")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{self, Write};
    use std::path::Path;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    use tempfile::TempDir;

    use super::*;
    use crate::adapters::Adapter;
    use crate::git::add_dependency_in_dir_with_adapters;
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

    fn make_reporter() -> (Reporter, SharedBuffer) {
        let buffer = SharedBuffer::default();
        (Reporter::sink(ColorMode::Never, buffer.clone()), buffer)
    }

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
        run_git(path, &["add", "."]);
        run_git(path, &["commit", "-m", "initial"]);
    }

    fn rename_current_branch(path: &Path, branch: &str) {
        run_git(path, &["branch", "-m", branch]);
    }

    #[test]
    fn reports_when_all_tag_dependencies_are_current() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        init_git_repo(repo.path());
        run_git(repo.path(), &["tag", "v0.1.0"]);

        let (reporter, _) = make_reporter();
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
            &reporter,
        )
        .unwrap();

        let (reporter, output) = make_reporter();
        let summary = check_outdated_in_dir(project.path(), cache.path(), &reporter).unwrap();

        assert_eq!(summary.dependency_count, 1);
        assert_eq!(summary.outdated_count, 0);
        assert!(output.contents().contains("current at tag v0.1.0"));
    }

    #[test]
    fn reports_newer_tags_for_direct_dependencies() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        init_git_repo(repo.path());
        run_git(repo.path(), &["tag", "v0.1.0"]);

        let (reporter, _) = make_reporter();
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
            &reporter,
        )
        .unwrap();

        run_git(repo.path(), &["tag", "v0.2.0"]);

        let (reporter, output) = make_reporter();
        let summary = check_outdated_in_dir(project.path(), cache.path(), &reporter).unwrap();

        assert_eq!(summary.outdated_count, 1);
        assert!(output.contents().contains("outdated: tag v0.1.0 -> v0.2.0"));
    }

    #[test]
    fn reports_advanced_branch_heads_against_locked_state() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        init_git_repo(repo.path());
        rename_current_branch(repo.path(), "main");

        let (reporter, _) = make_reporter();
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
            &reporter,
        )
        .unwrap();

        write_file(&repo.path().join("skills/review/extra.md"), "# Extra\n");
        run_git(repo.path(), &["add", "."]);
        run_git(repo.path(), &["commit", "-m", "advance"]);

        let (reporter, output) = make_reporter();
        let summary = check_outdated_in_dir(project.path(), cache.path(), &reporter).unwrap();

        assert_eq!(summary.outdated_count, 1);
        assert!(output.contents().contains("outdated: branch main"));
    }

    #[test]
    fn notes_when_no_direct_dependencies_are_configured() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let (reporter, output) = make_reporter();

        let summary = check_outdated_in_dir(project.path(), cache.path(), &reporter).unwrap();

        assert_eq!(summary.dependency_count, 0);
        assert_eq!(summary.outdated_count, 0);
        assert!(
            output
                .contents()
                .contains("no direct dependencies configured")
        );
    }
}
