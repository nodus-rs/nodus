use std::process::ExitCode;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::adapters::Adapter;
use crate::manifest::{DependencyComponent, RequestedGitRef};
use crate::paths::display_path;
use crate::report::Reporter;
use crate::review::ReviewProvider;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Manage project-scoped agent packages",
    long_about = "Nodus resolves agent packages from local paths and Git tags, locks exact revisions, and writes managed runtime outputs for supported adapters."
)]
struct Cli {
    #[arg(
        long = "store-path",
        alias = "cache-path",
        global = true,
        help = "Override the shared storage root for repository mirrors, checkouts, and snapshots"
    )]
    store_path: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Add a dependency and run sync")]
    Add {
        #[arg(help = "Git URL, local path, or GitHub shortcut like owner/repo")]
        url: String,
        #[arg(
            long,
            conflicts_with_all = ["branch", "revision"],
            help = "Pin a specific Git tag instead of resolving the latest tag"
        )]
        tag: Option<String>,
        #[arg(
            long,
            conflicts_with_all = ["tag", "revision"],
            help = "Track a specific Git branch instead of resolving the latest tag"
        )]
        branch: Option<String>,
        #[arg(
            long,
            conflicts_with_all = ["tag", "branch"],
            help = "Pin a specific Git commit revision"
        )]
        revision: Option<String>,
        #[arg(
            long,
            value_enum,
            help = "Select one or more adapters to persist for this repository"
        )]
        adapter: Vec<Adapter>,
        #[arg(
            long,
            value_enum,
            help = "Select which dependency components to install from the package"
        )]
        component: Vec<DependencyComponent>,
        #[arg(
            long = "sync-on-launch",
            help = "Persist project startup hooks so supported tools run `nodus sync` when they open this repository"
        )]
        sync_on_launch: bool,
        #[arg(
            long = "dry-run",
            help = "Preview project changes without writing to the project or linked repo; may still populate the shared store to compute the result"
        )]
        dry_run: bool,
    },
    #[command(about = "Remove a dependency and prune its managed outputs")]
    Remove {
        #[arg(help = "Dependency alias or repository reference to remove")]
        package: String,
        #[arg(
            long = "dry-run",
            help = "Preview project changes without writing to the project or linked repo; may still populate the shared store to compute the result"
        )]
        dry_run: bool,
    },
    #[command(about = "Display resolved package metadata")]
    Info {
        #[arg(
            help = "Dependency alias, local package path, Git URL, or GitHub shortcut like owner/repo"
        )]
        package: String,
        #[arg(long, conflicts_with = "branch", help = "Inspect a specific Git tag")]
        tag: Option<String>,
        #[arg(long, conflicts_with = "tag", help = "Inspect a specific Git branch")]
        branch: Option<String>,
    },
    #[command(about = "Use an AI review agent to assess whether a package graph looks safe to use")]
    Review {
        #[arg(
            default_value = ".",
            help = "Dependency alias, local package path, Git URL, or GitHub shortcut like owner/repo"
        )]
        package: String,
        #[arg(long, conflicts_with = "branch", help = "Inspect a specific Git tag")]
        tag: Option<String>,
        #[arg(long, conflicts_with = "tag", help = "Inspect a specific Git branch")]
        branch: Option<String>,
        #[arg(
            long,
            value_enum,
            default_value_t = ReviewProvider::Openai,
            help = "LLM provider to use for the safety review"
        )]
        provider: ReviewProvider,
        #[arg(
            long,
            help = "Specific model id to use; defaults to $MENTRA_MODEL or the provider's newest available model"
        )]
        model: Option<String>,
    },
    #[command(about = "Check direct dependencies for newer tags or branch head changes")]
    Outdated,
    #[command(about = "Update direct dependencies and resync managed outputs")]
    Update {
        #[arg(
            long = "allow-high-sensitivity",
            help = "Allow packages that declare high-sensitivity capabilities"
        )]
        allow_high_sensitivity: bool,
        #[arg(
            long = "dry-run",
            help = "Preview project changes without writing to the project or linked repo; may still populate the shared store to compute the result"
        )]
        dry_run: bool,
    },
    #[command(about = "Relay linked managed edits back into a maintainer checkout")]
    Relay {
        #[arg(help = "Dependency alias or repository reference to relay")]
        package: String,
        #[arg(long, help = "Local checkout path to persist and relay into")]
        repo_path: Option<PathBuf>,
        #[arg(
            long = "via",
            alias = "relay-via",
            alias = "prefer",
            value_enum,
            help = "Persist the preferred adapter for relay metadata when one adapter should be treated as canonical"
        )]
        via: Option<Adapter>,
        #[arg(
            long,
            help = "Keep watching managed outputs and relay new edits automatically"
        )]
        watch: bool,
        #[arg(
            long = "dry-run",
            conflicts_with = "watch",
            help = "Preview project changes without writing to the project or linked repo; may still populate the shared store to compute the result"
        )]
        dry_run: bool,
    },
    #[command(about = "Create a minimal nodus.toml and example skill")]
    Init {
        #[arg(
            long = "dry-run",
            help = "Preview project changes without writing to the project or linked repo; may still populate the shared store to compute the result"
        )]
        dry_run: bool,
    },
    #[command(about = "Resolve dependencies and write managed runtime outputs")]
    Sync {
        #[arg(long, help = "Fail if nodus.lock would change")]
        locked: bool,
        #[arg(
            long,
            help = "Install exact Git revisions from nodus.lock and fail if the lockfile is missing or stale"
        )]
        frozen: bool,
        #[arg(
            long = "allow-high-sensitivity",
            help = "Allow packages that declare high-sensitivity capabilities"
        )]
        allow_high_sensitivity: bool,
        #[arg(
            long,
            value_enum,
            help = "Override and persist the adapter selection for this repository"
        )]
        adapter: Vec<Adapter>,
        #[arg(
            long = "sync-on-launch",
            help = "Persist project startup hooks so supported tools run `nodus sync` when they open this repository"
        )]
        sync_on_launch: bool,
        #[arg(
            long = "dry-run",
            help = "Preview project changes without writing to the project or linked repo; may still populate the shared store to compute the result"
        )]
        dry_run: bool,
    },
    #[command(about = "Validate lockfile, shared store, and managed output consistency")]
    Doctor,
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let reporter = Reporter::stderr();
    let result = run_command(cli, &reporter);

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if reporter.error(&error).is_err() {
                eprintln!("error: {error:#}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run_command(cli: Cli, reporter: &Reporter) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let store_root = crate::cache::resolve_store_root(cli.store_path.as_deref())?;
    run_command_in_dir(cli.command, &cwd, &store_root, reporter)
}

fn run_command_in_dir(
    command: Command,
    cwd: &std::path::Path,
    cache_root: &std::path::Path,
    reporter: &Reporter,
) -> anyhow::Result<()> {
    match command {
        Command::Add {
            url,
            tag,
            branch,
            revision,
            adapter,
            component,
            sync_on_launch,
            dry_run,
        } => {
            let summary = if dry_run {
                crate::git::add_dependency_in_dir_with_adapters_dry_run(
                    cwd,
                    cache_root,
                    &url,
                    crate::git::AddDependencyOptions {
                        git_ref: requested_git_ref(
                            tag.as_deref(),
                            branch.as_deref(),
                            revision.as_deref(),
                        )?,
                        adapters: &adapter,
                        components: &component,
                        sync_on_launch,
                    },
                    reporter,
                )?
            } else {
                crate::git::add_dependency_in_dir_with_adapters(
                    cwd,
                    cache_root,
                    &url,
                    crate::git::AddDependencyOptions {
                        git_ref: requested_git_ref(
                            tag.as_deref(),
                            branch.as_deref(),
                            revision.as_deref(),
                        )?,
                        adapters: &adapter,
                        components: &component,
                        sync_on_launch,
                    },
                    reporter,
                )?
            };
            let message = if dry_run {
                format!(
                    "dry run: would add {} {} with adapters [{}]; would write {} managed files",
                    summary.alias,
                    summary.reference,
                    format_adapters(&summary.adapters),
                    summary.managed_file_count,
                )
            } else {
                format!(
                    "added {} {} with adapters [{}]; wrote {} managed files",
                    summary.alias,
                    summary.reference,
                    format_adapters(&summary.adapters),
                    summary.managed_file_count,
                )
            };
            reporter.finish(message)?;
            Ok(())
        }
        Command::Remove { package, dry_run } => {
            let summary = if dry_run {
                crate::git::remove_dependency_in_dir_dry_run(cwd, cache_root, &package, reporter)?
            } else {
                crate::git::remove_dependency_in_dir(cwd, cache_root, &package, reporter)?
            };
            let message = if dry_run {
                format!(
                    "dry run: would remove {} and would write {} managed files",
                    summary.alias, summary.managed_file_count,
                )
            } else {
                format!(
                    "removed {} and wrote {} managed files",
                    summary.alias, summary.managed_file_count,
                )
            };
            reporter.finish(message)?;
            Ok(())
        }
        Command::Info {
            package,
            tag,
            branch,
        } => crate::info::describe_package_in_dir(
            cwd,
            cache_root,
            &package,
            tag.as_deref(),
            branch.as_deref(),
            reporter,
        ),
        Command::Review {
            package,
            tag,
            branch,
            provider,
            model,
        } => {
            let summary = crate::review::review_package_in_dir(
                cwd,
                cache_root,
                crate::review::ReviewRequest {
                    package: &package,
                    tag: tag.as_deref(),
                    branch: branch.as_deref(),
                    provider,
                    model: model.as_deref(),
                },
                reporter,
            )?;
            reporter.finish(format!(
                "reviewed {} packages with {}",
                summary.package_count, summary.provider
            ))?;
            Ok(())
        }
        Command::Outdated => {
            let summary = crate::outdated::check_outdated_in_dir(cwd, cache_root, reporter)?;
            let outcome = if summary.outdated_count == 0 {
                format!(
                    "checked {} direct dependencies; all current",
                    summary.dependency_count
                )
            } else {
                format!(
                    "checked {} direct dependencies; {} outdated",
                    summary.dependency_count, summary.outdated_count
                )
            };
            reporter.finish(outcome)?;
            Ok(())
        }
        Command::Update {
            allow_high_sensitivity,
            dry_run,
        } => {
            let summary = if dry_run {
                crate::update::update_direct_dependencies_in_dir_dry_run(
                    cwd,
                    cache_root,
                    allow_high_sensitivity,
                    reporter,
                )?
            } else {
                crate::update::update_direct_dependencies_in_dir(
                    cwd,
                    cache_root,
                    allow_high_sensitivity,
                    reporter,
                )?
            };
            let message = if dry_run {
                format!(
                    "dry run: would update {} direct dependencies; would write {} managed files",
                    summary.updated_count, summary.managed_file_count
                )
            } else {
                format!(
                    "updated {} direct dependencies; wrote {} managed files",
                    summary.updated_count, summary.managed_file_count
                )
            };
            reporter.finish(message)?;
            Ok(())
        }
        Command::Init { dry_run } => {
            let summary = if dry_run {
                crate::manifest::scaffold_init_in_dir_dry_run(cwd, reporter)?
            } else {
                crate::manifest::scaffold_init_in_dir(cwd, reporter)?
            };
            let created = summary
                .created_paths
                .iter()
                .map(|path| display_path(path))
                .collect::<Vec<_>>()
                .join(", ");
            let message = if dry_run {
                format!("dry run: would create {created}")
            } else {
                format!("created {created}")
            };
            reporter.finish(message)?;
            Ok(())
        }
        Command::Relay {
            package,
            repo_path,
            via,
            watch,
            dry_run,
        } => {
            if watch {
                crate::relay::watch_dependency_in_dir(
                    cwd,
                    cache_root,
                    &package,
                    repo_path.as_deref(),
                    via,
                    reporter,
                )
            } else {
                let summary = if dry_run {
                    crate::relay::relay_dependency_in_dir_dry_run(
                        cwd,
                        cache_root,
                        &package,
                        repo_path.as_deref(),
                        via,
                        reporter,
                    )?
                } else {
                    crate::relay::relay_dependency_in_dir(
                        cwd,
                        cache_root,
                        &package,
                        repo_path.as_deref(),
                        via,
                        reporter,
                    )?
                };
                let message = if dry_run {
                    format!(
                        "dry run: would relay {} into {}; would update {} source files",
                        summary.alias,
                        display_path(&summary.linked_repo),
                        summary.updated_file_count,
                    )
                } else {
                    format!(
                        "relayed {} into {}; updated {} source files",
                        summary.alias,
                        display_path(&summary.linked_repo),
                        summary.updated_file_count,
                    )
                };
                reporter.finish(message)?;
                Ok(())
            }
        }
        Command::Sync {
            locked,
            frozen,
            allow_high_sensitivity,
            adapter,
            sync_on_launch,
            dry_run,
        } => {
            let summary = if frozen {
                if dry_run {
                    crate::resolver::sync_in_dir_with_adapters_frozen_dry_run(
                        cwd,
                        cache_root,
                        allow_high_sensitivity,
                        &adapter,
                        sync_on_launch,
                        reporter,
                    )?
                } else {
                    crate::resolver::sync_in_dir_with_adapters_frozen(
                        cwd,
                        cache_root,
                        allow_high_sensitivity,
                        &adapter,
                        sync_on_launch,
                        reporter,
                    )?
                }
            } else {
                if dry_run {
                    crate::resolver::sync_in_dir_with_adapters_dry_run(
                        cwd,
                        cache_root,
                        locked,
                        allow_high_sensitivity,
                        &adapter,
                        sync_on_launch,
                        reporter,
                    )?
                } else {
                    crate::resolver::sync_in_dir_with_adapters(
                        cwd,
                        cache_root,
                        locked,
                        allow_high_sensitivity,
                        &adapter,
                        sync_on_launch,
                        reporter,
                    )?
                }
            };
            reporter.finish(format!(
                "{}{} packages, adapters [{}], {} managed files",
                if dry_run {
                    "dry run: would resolve "
                } else {
                    ""
                },
                summary.package_count,
                format_adapters(&summary.adapters),
                summary.managed_file_count,
            ))?;
            Ok(())
        }
        Command::Doctor => {
            let summary = crate::resolver::doctor_in_dir(cwd, cache_root, reporter)?;
            reporter.finish(format!(
                "project state is consistent across {} packages",
                summary.package_count,
            ))?;
            Ok(())
        }
    }
}

fn format_adapters(adapters: &[Adapter]) -> String {
    adapters
        .iter()
        .map(|adapter| adapter.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn requested_git_ref<'a>(
    tag: Option<&'a str>,
    branch: Option<&'a str>,
    revision: Option<&'a str>,
) -> anyhow::Result<Option<RequestedGitRef<'a>>> {
    match (tag, branch, revision) {
        (Some(tag), None, None) => Ok(Some(RequestedGitRef::Tag(tag))),
        (None, Some(branch), None) => Ok(Some(RequestedGitRef::Branch(branch))),
        (None, None, Some(revision)) => Ok(Some(RequestedGitRef::Revision(revision))),
        (None, None, None) => Ok(None),
        _ => anyhow::bail!(
            "git dependency must not declare more than one of `tag`, `branch`, or `revision`"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::process::Command as ProcessCommand;
    use std::sync::{Arc, Mutex};

    use super::{Cli, Command, run_command_in_dir};
    use clap::Parser;
    use tempfile::TempDir;
    use walkdir::WalkDir;

    use crate::adapters::Adapter;
    use crate::report::{ColorMode, Reporter};
    use crate::resolver;

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
        fs::write(path, contents).unwrap();
    }

    fn write_skill(path: &Path, name: &str) {
        write_file(
            &path.join("SKILL.md"),
            &format!("---\nname: {name}\ndescription: Example skill.\n---\n# {name}\n"),
        );
    }

    fn init_git_repo(path: &Path) {
        let run = |args: &[&str]| {
            let output = ProcessCommand::new("git")
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

    fn create_git_dependency() -> (TempDir, String) {
        let repo = TempDir::new().unwrap();
        write_skill(&repo.path().join("skills/review"), "Review");
        init_git_repo(repo.path());

        let output = ProcessCommand::new("git")
            .args(["tag", "v0.1.0"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let url = repo.path().to_string_lossy().to_string();
        (repo, url)
    }

    fn run_command_output(command: Command, cwd: &Path, cache_root: &Path) -> String {
        let buffer = SharedBuffer::default();
        let reporter = Reporter::sink(ColorMode::Never, buffer.clone());

        run_command_in_dir(command, cwd, cache_root, &reporter).unwrap();

        buffer.contents()
    }

    fn read_optional(path: &Path) -> Option<Vec<u8>> {
        fs::read(path).ok()
    }

    fn first_file_under(root: &Path, file_name: &str) -> PathBuf {
        WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
            .find(|entry| entry.file_type().is_file() && entry.file_name() == file_name)
            .unwrap()
            .path()
            .to_path_buf()
    }

    #[test]
    fn parses_remove_subcommand() {
        let cli = Cli::try_parse_from(["nodus", "remove", "playbook_ios"]).unwrap();

        match cli.command {
            Command::Remove { package, .. } => assert_eq!(package, "playbook_ios"),
            other => panic!("expected remove command, got {other:?}"),
        }
    }

    #[test]
    fn rejects_uninstall_subcommand() {
        let error = Cli::try_parse_from(["nodus", "uninstall", "playbook_ios"]).unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn parses_info_subcommand() {
        let cli =
            Cli::try_parse_from(["nodus", "info", "obra/superpowers", "--branch", "main"]).unwrap();

        match cli.command {
            Command::Info {
                package,
                tag,
                branch,
            } => {
                assert_eq!(package, "obra/superpowers");
                assert_eq!(tag, None);
                assert_eq!(branch.as_deref(), Some("main"));
            }
            other => panic!("expected info command, got {other:?}"),
        }
    }

    #[test]
    fn parses_review_subcommand() {
        let cli = Cli::try_parse_from([
            "nodus",
            "review",
            "obra/superpowers",
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet",
        ])
        .unwrap();

        match cli.command {
            Command::Review {
                package,
                tag,
                branch,
                provider,
                model,
            } => {
                assert_eq!(package, "obra/superpowers");
                assert_eq!(tag, None);
                assert_eq!(branch, None);
                assert_eq!(provider, crate::review::ReviewProvider::Anthropic);
                assert_eq!(model.as_deref(), Some("claude-sonnet"));
            }
            other => panic!("expected review command, got {other:?}"),
        }
    }

    #[test]
    fn parses_outdated_subcommand() {
        let cli = Cli::try_parse_from(["nodus", "outdated"]).unwrap();

        match cli.command {
            Command::Outdated => {}
            other => panic!("expected outdated command, got {other:?}"),
        }
    }

    #[test]
    fn parses_relay_subcommand() {
        let cli = Cli::try_parse_from([
            "nodus",
            "relay",
            "wenext-limited/playbook-ios",
            "--repo-path",
            "/tmp/playbook-ios",
            "--watch",
        ])
        .unwrap();

        match cli.command {
            Command::Relay {
                package,
                repo_path,
                via,
                watch,
                ..
            } => {
                assert_eq!(package, "wenext-limited/playbook-ios");
                assert_eq!(repo_path.as_deref(), Some(Path::new("/tmp/playbook-ios")));
                assert_eq!(via, None);
                assert!(watch);
            }
            other => panic!("expected relay command, got {other:?}"),
        }
    }

    #[test]
    fn parses_relay_via_aliases() {
        let via =
            Cli::try_parse_from(["nodus", "relay", "example/repo", "--via", "claude"]).unwrap();
        let relay_via =
            Cli::try_parse_from(["nodus", "relay", "example/repo", "--relay-via", "codex"])
                .unwrap();
        let prefer =
            Cli::try_parse_from(["nodus", "relay", "example/repo", "--prefer", "opencode"])
                .unwrap();

        assert!(matches!(
            via.command,
            Command::Relay {
                via: Some(Adapter::Claude),
                ..
            }
        ));
        assert!(matches!(
            relay_via.command,
            Command::Relay {
                via: Some(Adapter::Codex),
                ..
            }
        ));
        assert!(matches!(
            prefer.command,
            Command::Relay {
                via: Some(Adapter::OpenCode),
                ..
            }
        ));
    }

    #[test]
    fn parses_update_subcommand() {
        let cli = Cli::try_parse_from(["nodus", "update", "--allow-high-sensitivity"]).unwrap();

        match cli.command {
            Command::Update {
                allow_high_sensitivity,
                ..
            } => assert!(allow_high_sensitivity),
            other => panic!("expected update command, got {other:?}"),
        }
    }

    #[test]
    fn parses_dry_run_flags_for_mutating_commands() {
        let add = Cli::try_parse_from(["nodus", "add", "example/repo", "--dry-run"]).unwrap();
        let remove = Cli::try_parse_from(["nodus", "remove", "example/repo", "--dry-run"]).unwrap();
        let update = Cli::try_parse_from(["nodus", "update", "--dry-run"]).unwrap();
        let relay = Cli::try_parse_from(["nodus", "relay", "example/repo", "--dry-run"]).unwrap();
        let init = Cli::try_parse_from(["nodus", "init", "--dry-run"]).unwrap();
        let sync = Cli::try_parse_from(["nodus", "sync", "--dry-run"]).unwrap();

        assert!(matches!(add.command, Command::Add { dry_run: true, .. }));
        assert!(matches!(
            remove.command,
            Command::Remove { dry_run: true, .. }
        ));
        assert!(matches!(
            update.command,
            Command::Update { dry_run: true, .. }
        ));
        assert!(matches!(
            relay.command,
            Command::Relay { dry_run: true, .. }
        ));
        assert!(matches!(init.command, Command::Init { dry_run: true }));
        assert!(matches!(sync.command, Command::Sync { dry_run: true, .. }));
    }

    #[test]
    fn rejects_relay_watch_with_dry_run() {
        let error = Cli::try_parse_from(["nodus", "relay", "example/repo", "--watch", "--dry-run"])
            .unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn root_help_describes_commands() {
        let help = <Cli as clap::CommandFactory>::command()
            .render_long_help()
            .to_string();

        assert!(help.contains("Nodus resolves agent packages from local paths and Git tags"));
        assert!(help.contains("Add a dependency and run sync"));
        assert!(help.contains("Display resolved package metadata"));
        assert!(help.contains("Check direct dependencies for newer tags or branch head changes"));
        assert!(help.contains("Update direct dependencies and resync managed outputs"));
        assert!(help.contains(
            "Use an AI review agent to assess whether a package graph looks safe to use"
        ));
        assert!(help.contains("Validate lockfile, shared store, and managed output consistency"));
    }

    #[test]
    fn add_help_describes_arguments() {
        let mut root = <Cli as clap::CommandFactory>::command();
        let help = root
            .find_subcommand_mut("add")
            .unwrap()
            .render_long_help()
            .to_string();

        assert!(help.contains("Git URL, local path, or GitHub shortcut like owner/repo"));
        assert!(help.contains("Pin a specific Git tag instead of resolving the latest tag"));
        assert!(help.contains("Track a specific Git branch instead of resolving the latest tag"));
        assert!(help.contains("Pin a specific Git commit revision"));
        assert!(help.contains("Select one or more adapters to persist for this repository"));
        assert!(help.contains("Select which dependency components to install from the package"));
        assert!(help.contains("Persist project startup hooks"));
    }

    #[test]
    fn mutating_subcommand_help_mentions_dry_run() {
        let mut root = <Cli as clap::CommandFactory>::command();
        for name in ["add", "remove", "update", "relay", "init", "sync"] {
            let help = root
                .find_subcommand_mut(name)
                .unwrap()
                .render_long_help()
                .to_string();
            assert!(help.contains("--dry-run"), "{name} help missing dry-run");
            assert!(
                help.contains("may still populate the shared store"),
                "{name} help missing shared-store explanation"
            );
        }
    }

    #[test]
    fn review_help_describes_arguments() {
        let mut root = <Cli as clap::CommandFactory>::command();
        let help = root
            .find_subcommand_mut("review")
            .unwrap()
            .render_long_help()
            .to_string();

        assert!(help.contains(
            "Dependency alias, local package path, Git URL, or GitHub shortcut like owner/repo"
        ));
        assert!(help.contains("LLM provider to use for the safety review"));
        assert!(help.contains("Specific model id to use"));
    }

    #[test]
    fn parses_repeatable_add_adapter_flags() {
        let cli = Cli::try_parse_from([
            "nodus",
            "add",
            "example/repo",
            "--adapter",
            "codex",
            "--adapter",
            "opencode",
        ])
        .unwrap();

        match cli.command {
            Command::Add { adapter, .. } => {
                assert_eq!(
                    adapter,
                    vec![super::Adapter::Codex, super::Adapter::OpenCode]
                );
            }
            other => panic!("expected add command, got {other:?}"),
        }
    }

    #[test]
    fn parses_add_branch_and_revision_flags() {
        let branch =
            Cli::try_parse_from(["nodus", "add", "example/repo", "--branch", "main"]).unwrap();
        let revision =
            Cli::try_parse_from(["nodus", "add", "example/repo", "--revision", "abc1234"]).unwrap();

        match branch.command {
            Command::Add {
                tag,
                branch,
                revision,
                ..
            } => {
                assert_eq!(tag, None);
                assert_eq!(branch.as_deref(), Some("main"));
                assert_eq!(revision, None);
            }
            other => panic!("expected add command, got {other:?}"),
        }

        match revision.command {
            Command::Add {
                tag,
                branch,
                revision,
                ..
            } => {
                assert_eq!(tag, None);
                assert_eq!(branch, None);
                assert_eq!(revision.as_deref(), Some("abc1234"));
            }
            other => panic!("expected add command, got {other:?}"),
        }
    }

    #[test]
    fn parses_sync_on_launch_flags() {
        let add =
            Cli::try_parse_from(["nodus", "add", "example/repo", "--sync-on-launch"]).unwrap();
        let sync = Cli::try_parse_from(["nodus", "sync", "--sync-on-launch"]).unwrap();

        match add.command {
            Command::Add { sync_on_launch, .. } => assert!(sync_on_launch),
            other => panic!("expected add command, got {other:?}"),
        }

        match sync.command {
            Command::Sync { sync_on_launch, .. } => assert!(sync_on_launch),
            other => panic!("expected sync command, got {other:?}"),
        }
    }

    #[test]
    fn parses_sync_frozen_flag() {
        let cli = Cli::try_parse_from(["nodus", "sync", "--frozen"]).unwrap();

        match cli.command {
            Command::Sync { frozen, locked, .. } => {
                assert!(frozen);
                assert!(!locked);
            }
            other => panic!("expected sync command, got {other:?}"),
        }
    }

    #[test]
    fn parses_repeatable_add_component_flags() {
        let cli = Cli::try_parse_from([
            "nodus",
            "add",
            "example/repo",
            "--component",
            "skills",
            "--component",
            "agents",
        ])
        .unwrap();

        match cli.command {
            Command::Add { component, .. } => {
                assert_eq!(
                    component,
                    vec![
                        crate::manifest::DependencyComponent::Skills,
                        crate::manifest::DependencyComponent::Agents
                    ]
                );
            }
            other => panic!("expected add command, got {other:?}"),
        }
    }

    #[test]
    fn init_command_emits_creating_and_finished_lines() {
        let temp = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();

        let output =
            run_command_output(Command::Init { dry_run: false }, temp.path(), cache.path());

        assert!(output.contains("Creating"));
        assert!(output.contains("nodus.toml"));
        assert!(output.contains("skills/example/SKILL.md"));
        assert!(output.contains("Finished"));
    }

    #[test]
    fn init_dry_run_previews_without_writing() {
        let temp = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();

        let output = run_command_output(Command::Init { dry_run: true }, temp.path(), cache.path());

        assert!(output.contains("would create"));
        assert!(output.contains("dry run: would create"));
        assert!(!temp.path().join("nodus.toml").exists());
        assert!(!temp.path().join("skills/example/SKILL.md").exists());
    }

    #[test]
    fn info_command_emits_package_metadata_lines() {
        let temp = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        write_file(
            &temp.path().join("nodus.toml"),
            r#"
name = "playbook-ios"
version = "0.1.0"
"#,
        );
        write_skill(&temp.path().join("skills/review"), "Review");

        let output = run_command_output(
            Command::Info {
                package: ".".into(),
                tag: None,
                branch: None,
            },
            temp.path(),
            cache.path(),
        );

        assert!(output.contains("playbook-ios"));
        assert!(output.contains("version: 0.1.0"));
        assert!(output.contains("alias: playbook_ios"));
        assert!(output.contains("artifacts:"));
        assert!(output.contains("skills = [review]"));
        assert!(!output.contains("Finished"));
    }

    #[test]
    fn add_command_emits_resolving_and_adding_lines() {
        let temp = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let (_repo, url) = create_git_dependency();

        let output = run_command_output(
            Command::Add {
                url,
                tag: None,
                branch: None,
                revision: None,
                adapter: vec![Adapter::Codex],
                component: vec![],
                sync_on_launch: false,
                dry_run: false,
            },
            temp.path(),
            cache.path(),
        );

        assert!(output.contains("Resolving"));
        assert!(output.contains("latest tag"));
        assert!(output.contains("Adding"));
        assert!(output.contains("Finished"));
    }

    #[test]
    fn add_dry_run_previews_without_writing_project_files() {
        let temp = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let (_repo, url) = create_git_dependency();

        let output = run_command_output(
            Command::Add {
                url,
                tag: None,
                branch: None,
                revision: None,
                adapter: vec![Adapter::Codex],
                component: vec![],
                sync_on_launch: false,
                dry_run: true,
            },
            temp.path(),
            cache.path(),
        );

        assert!(output.contains("dry run: would added") || output.contains("dry run: would add"));
        assert!(output.contains("would create"));
        assert!(!temp.path().join("nodus.toml").exists());
        assert!(!temp.path().join("nodus.lock").exists());
        assert!(!temp.path().join(".codex").exists());
    }

    #[test]
    fn sync_command_emits_statuses_and_notes() {
        let temp = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join(".codex")).unwrap();
        write_file(
            &temp.path().join("nodus.toml"),
            r#"
[[capabilities]]
id = "shell.exec"
sensitivity = "high"
justification = "Run checks."
"#,
        );

        let output = run_command_output(
            Command::Sync {
                locked: false,
                frozen: false,
                allow_high_sensitivity: true,
                adapter: vec![],
                sync_on_launch: false,
                dry_run: false,
            },
            temp.path(),
            cache.path(),
        );

        assert!(output.contains("Resolving"));
        assert!(output.contains("Checking"));
        assert!(output.contains("Snapshotting"));
        assert!(output.contains("note: capability root shell.exec (high)"));
        assert!(output.contains("Finished"));
    }

    #[test]
    fn sync_dry_run_previews_without_writing_project_files() {
        let temp = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();

        let output = run_command_output(
            Command::Sync {
                locked: false,
                frozen: false,
                allow_high_sensitivity: false,
                adapter: vec![Adapter::Codex],
                sync_on_launch: true,
                dry_run: true,
            },
            temp.path(),
            cache.path(),
        );

        assert!(output.contains("would create"));
        assert!(output.contains("dry run: would resolve"));
        assert!(!temp.path().join("nodus.toml").exists());
        assert!(!temp.path().join("nodus.lock").exists());
        assert!(!temp.path().join(".codex").exists());
    }

    #[test]
    fn doctor_command_emits_checking_and_finished_lines() {
        let temp = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join(".codex")).unwrap();

        let reporter = Reporter::silent();
        resolver::sync_in_dir_with_adapters(
            temp.path(),
            cache.path(),
            false,
            false,
            &[],
            false,
            &reporter,
        )
        .unwrap();

        let output = run_command_output(Command::Doctor, temp.path(), cache.path());

        assert!(output.contains("Checking"));
        assert!(output.contains("Finished"));
        assert!(output.contains("project state is consistent"));
    }

    #[test]
    fn update_command_emits_updating_and_finished_lines() {
        let temp = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let (_repo, url) = create_git_dependency();

        run_command_in_dir(
            Command::Add {
                url,
                tag: Some("v0.1.0".into()),
                branch: None,
                revision: None,
                adapter: vec![Adapter::Codex],
                component: vec![],
                sync_on_launch: false,
                dry_run: false,
            },
            temp.path(),
            cache.path(),
            &Reporter::silent(),
        )
        .unwrap();

        let output = run_command_output(
            Command::Update {
                allow_high_sensitivity: false,
                dry_run: false,
            },
            temp.path(),
            cache.path(),
        );

        assert!(output.contains("Checking"));
        assert!(output.contains("Resolving"));
        assert!(output.contains("Finished"));
    }

    #[test]
    fn remove_dry_run_keeps_manifest_and_lockfile_unchanged() {
        let temp = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let (_repo, url) = create_git_dependency();

        run_command_in_dir(
            Command::Add {
                url,
                tag: None,
                branch: None,
                revision: None,
                adapter: vec![Adapter::Codex],
                component: vec![],
                sync_on_launch: false,
                dry_run: false,
            },
            temp.path(),
            cache.path(),
            &Reporter::silent(),
        )
        .unwrap();

        let alias = crate::manifest::load_root_from_dir(temp.path())
            .unwrap()
            .manifest
            .dependencies
            .keys()
            .next()
            .unwrap()
            .clone();
        let manifest_before = read_optional(&temp.path().join("nodus.toml")).unwrap();
        let lockfile_before = read_optional(&temp.path().join("nodus.lock")).unwrap();

        let output = run_command_output(
            Command::Remove {
                package: alias,
                dry_run: true,
            },
            temp.path(),
            cache.path(),
        );

        assert!(output.contains("dry run: would remove"));
        assert_eq!(
            read_optional(&temp.path().join("nodus.toml")).unwrap(),
            manifest_before
        );
        assert_eq!(
            read_optional(&temp.path().join("nodus.lock")).unwrap(),
            lockfile_before
        );
    }

    #[test]
    fn update_dry_run_keeps_manifest_and_lockfile_unchanged() {
        let temp = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let (repo, url) = create_git_dependency();

        run_command_in_dir(
            Command::Add {
                url,
                tag: Some("v0.1.0".into()),
                branch: None,
                revision: None,
                adapter: vec![Adapter::Codex],
                component: vec![],
                sync_on_launch: false,
                dry_run: false,
            },
            temp.path(),
            cache.path(),
            &Reporter::silent(),
        )
        .unwrap();

        let output = ProcessCommand::new("git")
            .args(["tag", "v0.2.0"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let manifest_before = read_optional(&temp.path().join("nodus.toml")).unwrap();
        let lockfile_before = read_optional(&temp.path().join("nodus.lock")).unwrap();

        let output = run_command_output(
            Command::Update {
                allow_high_sensitivity: false,
                dry_run: true,
            },
            temp.path(),
            cache.path(),
        );

        assert!(output.contains("dry run: would update"));
        assert_eq!(
            read_optional(&temp.path().join("nodus.toml")).unwrap(),
            manifest_before
        );
        assert_eq!(
            read_optional(&temp.path().join("nodus.lock")).unwrap(),
            lockfile_before
        );
    }

    #[test]
    fn sync_dry_run_locked_and_frozen_leave_state_unchanged() {
        let temp = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let (_repo, url) = create_git_dependency();

        run_command_in_dir(
            Command::Add {
                url,
                tag: None,
                branch: None,
                revision: None,
                adapter: vec![Adapter::Codex],
                component: vec![],
                sync_on_launch: false,
                dry_run: false,
            },
            temp.path(),
            cache.path(),
            &Reporter::silent(),
        )
        .unwrap();

        let manifest_before = read_optional(&temp.path().join("nodus.toml")).unwrap();
        let lockfile_before = read_optional(&temp.path().join("nodus.lock")).unwrap();

        let locked_output = run_command_output(
            Command::Sync {
                locked: true,
                frozen: false,
                allow_high_sensitivity: false,
                adapter: vec![],
                sync_on_launch: false,
                dry_run: true,
            },
            temp.path(),
            cache.path(),
        );
        let frozen_output = run_command_output(
            Command::Sync {
                locked: false,
                frozen: true,
                allow_high_sensitivity: false,
                adapter: vec![],
                sync_on_launch: false,
                dry_run: true,
            },
            temp.path(),
            cache.path(),
        );

        assert!(locked_output.contains("dry run: would resolve"));
        assert!(frozen_output.contains("dry run: would resolve"));
        assert_eq!(
            read_optional(&temp.path().join("nodus.toml")).unwrap(),
            manifest_before
        );
        assert_eq!(
            read_optional(&temp.path().join("nodus.lock")).unwrap(),
            lockfile_before
        );
    }

    #[test]
    fn relay_dry_run_does_not_persist_local_config_or_repo_edits() {
        let temp = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let (repo, url) = create_git_dependency();

        let output = ProcessCommand::new("git")
            .args(["remote", "add", "origin", &repo.path().to_string_lossy()])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        run_command_in_dir(
            Command::Add {
                url,
                tag: None,
                branch: None,
                revision: None,
                adapter: vec![Adapter::Codex],
                component: vec![],
                sync_on_launch: false,
                dry_run: false,
            },
            temp.path(),
            cache.path(),
            &Reporter::silent(),
        )
        .unwrap();

        let managed_skill = first_file_under(&temp.path().join(".codex"), "SKILL.md");
        write_file(
            &managed_skill,
            "---\nname: Review\ndescription: Example skill.\n---\n# Edited\n",
        );
        let repo_skill = repo.path().join("skills/review/SKILL.md");
        let repo_before = read_optional(&repo_skill).unwrap();

        let output = run_command_output(
            Command::Relay {
                package: crate::manifest::load_root_from_dir(temp.path())
                    .unwrap()
                    .manifest
                    .dependencies
                    .keys()
                    .next()
                    .unwrap()
                    .clone(),
                repo_path: Some(repo.path().to_path_buf()),
                via: Some(Adapter::Codex),
                watch: false,
                dry_run: true,
            },
            temp.path(),
            cache.path(),
        );

        assert!(output.contains("would persist local config"));
        assert!(output.contains("would relay"));
        assert_eq!(read_optional(&repo_skill).unwrap(), repo_before);
        assert!(!temp.path().join(".nodus/local.toml").exists());
        assert!(!temp.path().join(".nodus/.gitignore").exists());
    }
}
