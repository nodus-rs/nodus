use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rayon::prelude::*;

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

#[derive(Debug, Clone)]
struct DependencySnapshot {
    alias: String,
    spec: DependencySpec,
}

#[derive(Debug, Clone)]
enum DependencyProbe {
    Path {
        path: PathBuf,
    },
    GitTag {
        current_tag: String,
        latest_tag: String,
    },
    GitBranch {
        branch: String,
        locked_rev: Option<String>,
        latest_rev: String,
    },
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
    let dependencies = root
        .manifest
        .dependencies
        .iter()
        .map(|(alias, spec)| DependencySnapshot {
            alias: alias.clone(),
            spec: spec.clone(),
        })
        .collect::<Vec<_>>();
    let probes = probe_dependencies(&dependencies, lockfile.as_ref(), cache_root)?;
    let reports = dependencies
        .iter()
        .map(|dependency| {
            report_for_dependency(
                &dependency.alias,
                probes.get(&dependency.alias).with_context(|| {
                    format!("missing dependency probe for `{}`", dependency.alias)
                })?,
            )
        })
        .collect::<Result<Vec<_>>>()?;

    let outdated_count = reports.iter().filter(|report| report.is_outdated()).count();
    for report in reports {
        reporter.line(report.render())?;
    }

    Ok(OutdatedSummary {
        dependency_count,
        outdated_count,
    })
}

fn probe_dependencies(
    dependencies: &[DependencySnapshot],
    lockfile: Option<&Lockfile>,
    cache_root: &Path,
) -> Result<BTreeMap<String, DependencyProbe>> {
    let mut probes = BTreeMap::new();
    let mut git_groups = BTreeMap::<String, Vec<&DependencySnapshot>>::new();

    for dependency in dependencies {
        match dependency.spec.source_kind()? {
            DependencySourceKind::Path => {
                let path = dependency.spec.path.clone().with_context(|| {
                    format!("dependency `{}` must declare `path`", dependency.alias)
                })?;
                probes.insert(dependency.alias.clone(), DependencyProbe::Path { path });
            }
            DependencySourceKind::Git => {
                let url = dependency.spec.resolved_git_url()?;
                git_groups.entry(url).or_default().push(dependency);
            }
        }
    }

    let git_probes = git_groups
        .into_par_iter()
        .map(|(url, dependencies)| {
            let reporter = Reporter::silent();
            let mirror_path = prepare_repository_mirror(cache_root, &url, true, &reporter)?;
            let mut latest_tag_name = None;
            let mut latest_branch_revs = BTreeMap::<String, String>::new();
            let mut group_probes = Vec::with_capacity(dependencies.len());

            for dependency in dependencies {
                let probe = match dependency.spec.requested_git_ref()? {
                    RequestedGitRef::Tag(current_tag) => {
                        let latest_tag = match latest_tag_name.clone() {
                            Some(tag) => tag,
                            None => {
                                let tag = latest_tag(&mirror_path)?;
                                latest_tag_name = Some(tag.clone());
                                tag
                            }
                        };
                        DependencyProbe::GitTag {
                            current_tag: current_tag.to_string(),
                            latest_tag,
                        }
                    }
                    RequestedGitRef::Branch(branch) => {
                        let latest_rev = match latest_branch_revs.get(branch) {
                            Some(rev) => rev.clone(),
                            None => {
                                let rev = ensure_git_dependency(
                                    cache_root,
                                    &url,
                                    None,
                                    Some(branch),
                                    true,
                                    &reporter,
                                )?
                                .rev;
                                latest_branch_revs.insert(branch.to_string(), rev.clone());
                                rev
                            }
                        };
                        DependencyProbe::GitBranch {
                            branch: branch.to_string(),
                            locked_rev: locked_rev(lockfile, &dependency.alias),
                            latest_rev,
                        }
                    }
                };
                group_probes.push((dependency.alias.clone(), probe));
            }

            Ok(group_probes)
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<Result<Vec<_>>>()?;

    for group in git_probes {
        for (alias, probe) in group {
            probes.insert(alias, probe);
        }
    }

    Ok(probes)
}

fn report_for_dependency(alias: &str, probe: &DependencyProbe) -> Result<DependencyReport> {
    let status = match probe {
        DependencyProbe::Path { path } => DependencyStatus::Path { path: path.clone() },
        DependencyProbe::GitTag {
            current_tag,
            latest_tag,
        } => {
            if latest_tag == current_tag {
                DependencyStatus::GitTagCurrent {
                    tag: current_tag.clone(),
                }
            } else {
                DependencyStatus::GitTagOutdated {
                    current: current_tag.clone(),
                    latest: latest_tag.clone(),
                }
            }
        }
        DependencyProbe::GitBranch {
            branch,
            locked_rev,
            latest_rev,
        } => match locked_rev {
            Some(current_rev) if current_rev == latest_rev => DependencyStatus::GitBranchCurrent {
                branch: branch.clone(),
                rev: current_rev.clone(),
            },
            Some(current_rev) => DependencyStatus::GitBranchOutdated {
                branch: branch.clone(),
                current_rev: current_rev.clone(),
                latest_rev: latest_rev.clone(),
            },
            None => DependencyStatus::GitBranchUnlocked {
                branch: branch.clone(),
                latest_rev: latest_rev.clone(),
            },
        },
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
