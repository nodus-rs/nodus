use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::Serialize;

use crate::domain::dependency_status::{
    display_dependency_alias, load_lockfile, locked_rev, locked_tag, short_identifier,
};
use crate::git::{
    ensure_git_dependency, latest_compatible_tag, latest_tag, parse_semver_tag,
    prepare_repository_mirror,
};
use crate::lockfile::Lockfile;
use crate::manifest::{
    DependencyKind, DependencySourceKind, DependencySpec, RequestedGitRef, load_root_from_dir,
};
use crate::paths::display_path;
use crate::report::Reporter;

#[derive(Debug, Clone)]
pub struct OutdatedSummary {
    pub dependency_count: usize,
    pub outdated_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutdatedReportSet {
    pub dependency_count: usize,
    pub outdated_count: usize,
    pub dependencies: Vec<DependencyReport>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
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
    GitRevisionCurrent {
        rev: String,
    },
    GitSemverCurrent {
        requirement: String,
        current: String,
        latest: String,
    },
    GitSemverCompatibleUpdate {
        requirement: String,
        current: String,
        latest_compatible: String,
        latest: String,
    },
    GitSemverMajorUpdateAvailable {
        requirement: String,
        current: String,
        latest_compatible: String,
        latest: String,
    },
    GitSemverUnlocked {
        requirement: String,
        latest_compatible: String,
        latest: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct DependencyReport {
    alias: String,
    kind: DependencyKind,
    #[serde(flatten)]
    status: DependencyStatus,
}

#[derive(Debug, Clone)]
struct DependencySnapshot {
    alias: String,
    kind: DependencyKind,
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
    GitRevision {
        rev: String,
    },
    GitSemver {
        requirement: String,
        locked_tag: Option<String>,
        latest_compatible: String,
        latest: String,
    },
}

impl DependencyReport {
    fn is_outdated(&self) -> bool {
        matches!(
            self.status,
            DependencyStatus::GitTagOutdated { .. }
                | DependencyStatus::GitBranchOutdated { .. }
                | DependencyStatus::GitSemverCompatibleUpdate { .. }
                | DependencyStatus::GitSemverMajorUpdateAvailable { .. }
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
            DependencyStatus::GitRevisionCurrent { rev } => {
                format!("pinned to revision {}", short_rev(rev))
            }
            DependencyStatus::GitSemverCurrent {
                requirement,
                current,
                latest,
            } => format!("current at tag {current} for {requirement} (latest overall {latest})"),
            DependencyStatus::GitSemverCompatibleUpdate {
                requirement,
                current,
                latest_compatible,
                latest,
            } => format!(
                "compatible update for {requirement}: {current} -> {latest_compatible} (latest overall {latest})"
            ),
            DependencyStatus::GitSemverMajorUpdateAvailable {
                requirement,
                current,
                latest_compatible,
                latest,
            } => format!(
                "major update available for {requirement}: current {current}, latest compatible {latest_compatible}, latest overall {latest}"
            ),
            DependencyStatus::GitSemverUnlocked {
                requirement,
                latest_compatible,
                latest,
            } => format!(
                "version {requirement} resolves to {latest_compatible} (latest overall {latest}; not locked locally)"
            ),
        };
        format!("{:<20} {}", display_alias(&self.alias, self.kind), status)
    }
}

pub fn check_outdated_in_dir(
    cwd: &Path,
    cache_root: &Path,
    reporter: &Reporter,
) -> Result<OutdatedSummary> {
    let report = collect_outdated_reports_in_dir(cwd, cache_root)?;
    if report.dependency_count == 0 {
        reporter.note("no dependencies configured")?;
        return Ok(OutdatedSummary {
            dependency_count: report.dependency_count,
            outdated_count: 0,
        });
    }

    reporter.status(
        "Checking",
        format!("{} dependencies", report.dependency_count),
    )?;
    for report in &report.dependencies {
        reporter.line(report.render())?;
    }

    Ok(OutdatedSummary {
        dependency_count: report.dependency_count,
        outdated_count: report.outdated_count,
    })
}

pub fn check_outdated_json_in_dir(cwd: &Path, cache_root: &Path) -> Result<OutdatedReportSet> {
    collect_outdated_reports_in_dir(cwd, cache_root)
}

fn collect_outdated_reports_in_dir(cwd: &Path, cache_root: &Path) -> Result<OutdatedReportSet> {
    let root = load_root_from_dir(cwd)?;
    let dependency_count = root.manifest.active_dependency_entries().len();
    if dependency_count == 0 {
        return Ok(OutdatedReportSet {
            dependency_count,
            outdated_count: 0,
            dependencies: Vec::new(),
        });
    }

    let lockfile = load_lockfile(cwd)?;
    let dependencies = root
        .manifest
        .active_dependency_entries()
        .into_iter()
        .map(|entry| DependencySnapshot {
            alias: entry.alias.to_string(),
            kind: entry.kind,
            spec: entry.spec.clone(),
        })
        .collect::<Vec<_>>();
    let probes = probe_dependencies(&dependencies, lockfile.as_ref(), cache_root)?;
    let reports = dependencies
        .iter()
        .map(|dependency| {
            report_for_dependency(
                &dependency.alias,
                dependency.kind,
                probes.get(&dependency.alias).with_context(|| {
                    format!("missing dependency probe for `{}`", dependency.alias)
                })?,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let outdated_count = reports.iter().filter(|report| report.is_outdated()).count();

    Ok(OutdatedReportSet {
        dependency_count,
        outdated_count,
        dependencies: reports,
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
                                    Some(RequestedGitRef::Branch(branch)),
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
                    RequestedGitRef::Revision(rev) => DependencyProbe::GitRevision {
                        rev: rev.to_string(),
                    },
                    RequestedGitRef::VersionReq(requirement) => {
                        let latest_compatible = latest_compatible_tag(&mirror_path, requirement)?;
                        let latest = latest_tag(&mirror_path)
                            .ok()
                            .filter(|tag| parse_semver_tag(tag).is_some())
                            .unwrap_or_else(|| latest_compatible.clone());
                        DependencyProbe::GitSemver {
                            requirement: requirement.to_string(),
                            locked_tag: locked_tag(lockfile, &dependency.alias),
                            latest_compatible,
                            latest,
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

fn report_for_dependency(
    alias: &str,
    kind: DependencyKind,
    probe: &DependencyProbe,
) -> Result<DependencyReport> {
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
        DependencyProbe::GitRevision { rev } => {
            DependencyStatus::GitRevisionCurrent { rev: rev.clone() }
        }
        DependencyProbe::GitSemver {
            requirement,
            locked_tag,
            latest_compatible,
            latest,
        } => match locked_tag {
            Some(current) if current == latest_compatible && current == latest => {
                DependencyStatus::GitSemverCurrent {
                    requirement: requirement.clone(),
                    current: current.clone(),
                    latest: latest.clone(),
                }
            }
            Some(current) if current == latest_compatible => {
                DependencyStatus::GitSemverMajorUpdateAvailable {
                    requirement: requirement.clone(),
                    current: current.clone(),
                    latest_compatible: latest_compatible.clone(),
                    latest: latest.clone(),
                }
            }
            Some(current) => DependencyStatus::GitSemverCompatibleUpdate {
                requirement: requirement.clone(),
                current: current.clone(),
                latest_compatible: latest_compatible.clone(),
                latest: latest.clone(),
            },
            None => DependencyStatus::GitSemverUnlocked {
                requirement: requirement.clone(),
                latest_compatible: latest_compatible.clone(),
                latest: latest.clone(),
            },
        },
    };

    Ok(DependencyReport {
        alias: alias.to_string(),
        kind,
        status,
    })
}

fn display_alias(alias: &str, kind: DependencyKind) -> String {
    display_dependency_alias(alias, kind)
}

fn short_rev(rev: &str) -> String {
    short_identifier(rev)
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
            crate::git::AddDependencyOptions {
                git_ref: Some(RequestedGitRef::Tag("v0.1.0")),
                version_req: None,
                kind: DependencyKind::Dependency,
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
            crate::git::AddDependencyOptions {
                git_ref: Some(RequestedGitRef::Tag("v0.1.0")),
                version_req: None,
                kind: DependencyKind::Dependency,
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
            crate::git::AddDependencyOptions {
                git_ref: None,
                version_req: None,
                kind: DependencyKind::Dependency,
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
    fn reports_revision_pins_as_current() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        init_git_repo(repo.path());
        let revision = crate::git::current_rev(repo.path()).unwrap();

        let (reporter, _) = make_reporter();
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
            &reporter,
        )
        .unwrap();

        write_file(&repo.path().join("skills/review/extra.md"), "# Extra\n");
        run_git(repo.path(), &["add", "."]);
        run_git(repo.path(), &["commit", "-m", "advance"]);

        let (reporter, output) = make_reporter();
        let summary = check_outdated_in_dir(project.path(), cache.path(), &reporter).unwrap();

        assert_eq!(summary.outdated_count, 0);
        assert!(output.contents().contains("pinned to revision"));
    }

    #[test]
    fn reports_semver_compatible_updates_and_major_availability() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        init_git_repo(repo.path());
        run_git(repo.path(), &["tag", "v1.0.0"]);

        let (reporter, _) = make_reporter();
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
            &reporter,
        )
        .unwrap();

        run_git(repo.path(), &["tag", "v1.2.0"]);
        run_git(repo.path(), &["tag", "v2.0.0"]);

        let (reporter, output) = make_reporter();
        let summary = check_outdated_in_dir(project.path(), cache.path(), &reporter).unwrap();

        assert_eq!(summary.outdated_count, 1);
        assert!(output.contents().contains("compatible update for ^1.0.0"));
        assert!(output.contents().contains("latest overall v2.0.0"));
    }

    #[test]
    fn json_reports_include_dev_dependency_kind() {
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
            crate::git::AddDependencyOptions {
                git_ref: Some(RequestedGitRef::Tag("v0.1.0")),
                version_req: None,
                kind: DependencyKind::DevDependency,
                adapters: &[Adapter::Codex],
                components: &[],
                sync_on_launch: false,
            },
            &reporter,
        )
        .unwrap();

        let report = check_outdated_json_in_dir(project.path(), cache.path()).unwrap();

        assert_eq!(report.dependencies.len(), 1);
        assert_eq!(report.dependencies[0].kind, DependencyKind::DevDependency);
    }

    #[test]
    fn notes_when_no_direct_dependencies_are_configured() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let (reporter, output) = make_reporter();

        let summary = check_outdated_in_dir(project.path(), cache.path(), &reporter).unwrap();

        assert_eq!(summary.dependency_count, 0);
        assert_eq!(summary.outdated_count, 0);
        assert!(output.contents().contains("no dependencies configured"));
    }

    #[test]
    fn ignores_disabled_dependencies() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        init_git_repo(repo.path());
        run_git(repo.path(), &["tag", "v0.1.0"]);

        write_file(
            &project.path().join("nodus.toml"),
            &format!(
                r#"
[dependencies]
review = {{ path = "vendor/review" }}
disabled = {{ url = "{}", tag = "v0.1.0", enabled = false }}
"#,
                repo.path().to_string_lossy(),
            ),
        );
        write_skill(
            &project.path().join("vendor/review/skills/review"),
            "Review",
        );

        let report = check_outdated_json_in_dir(project.path(), cache.path()).unwrap();

        assert_eq!(report.dependency_count, 1);
        assert_eq!(report.dependencies.len(), 1);
        assert_eq!(report.dependencies[0].alias, "review");
    }
}
