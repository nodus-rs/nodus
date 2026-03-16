use std::process::ExitCode;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::adapters::Adapter;
use crate::manifest::DependencyComponent;
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
            help = "Pin a specific Git tag instead of resolving the latest tag"
        )]
        tag: Option<String>,
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
    },
    #[command(about = "Remove a dependency and prune its managed outputs")]
    Remove {
        #[arg(help = "Dependency alias or repository reference to remove")]
        package: String,
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
    },
    #[command(about = "Create a minimal nodus.toml and example skill")]
    Init,
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
            adapter,
            component,
            sync_on_launch,
        } => {
            let summary = crate::git::add_dependency_in_dir_with_adapters(
                cwd,
                cache_root,
                &url,
                tag.as_deref(),
                crate::git::AddDependencyOptions {
                    adapters: &adapter,
                    components: &component,
                    sync_on_launch,
                },
                reporter,
            )?;
            reporter.finish(format!(
                "added {} {} with adapters [{}]; wrote {} managed files",
                summary.alias,
                summary.reference,
                format_adapters(&summary.adapters),
                summary.managed_file_count,
            ))?;
            Ok(())
        }
        Command::Remove { package } => {
            let summary =
                crate::git::remove_dependency_in_dir(cwd, cache_root, &package, reporter)?;
            reporter.finish(format!(
                "removed {} and wrote {} managed files",
                summary.alias, summary.managed_file_count,
            ))?;
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
        } => {
            let summary = crate::update::update_direct_dependencies_in_dir(
                cwd,
                cache_root,
                allow_high_sensitivity,
                reporter,
            )?;
            reporter.finish(format!(
                "updated {} direct dependencies; wrote {} managed files",
                summary.updated_count, summary.managed_file_count
            ))?;
            Ok(())
        }
        Command::Init => {
            let summary = crate::manifest::scaffold_init_in_dir(cwd, reporter)?;
            reporter.finish(format!(
                "created {}",
                summary
                    .created_paths
                    .iter()
                    .map(|path| display_path(path))
                    .collect::<Vec<_>>()
                    .join(", "),
            ))?;
            Ok(())
        }
        Command::Sync {
            locked,
            frozen,
            allow_high_sensitivity,
            adapter,
            sync_on_launch,
        } => {
            let summary = if frozen {
                crate::resolver::sync_in_dir_with_adapters_frozen(
                    cwd,
                    cache_root,
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
            };
            reporter.finish(format!(
                "{} packages, adapters [{}], {} managed files",
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

fn display_path(path: &std::path::Path) -> String {
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
    use std::process::Command as ProcessCommand;
    use std::sync::{Arc, Mutex};

    use super::{Cli, Command, run_command_in_dir};
    use clap::Parser;
    use tempfile::TempDir;

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

    #[test]
    fn parses_remove_subcommand() {
        let cli = Cli::try_parse_from(["nodus", "remove", "playbook_ios"]).unwrap();

        match cli.command {
            Command::Remove { package } => assert_eq!(package, "playbook_ios"),
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
    fn parses_update_subcommand() {
        let cli = Cli::try_parse_from(["nodus", "update", "--allow-high-sensitivity"]).unwrap();

        match cli.command {
            Command::Update {
                allow_high_sensitivity,
            } => assert!(allow_high_sensitivity),
            other => panic!("expected update command, got {other:?}"),
        }
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
        assert!(help.contains("Select one or more adapters to persist for this repository"));
        assert!(help.contains("Select which dependency components to install from the package"));
        assert!(help.contains("Persist project startup hooks"));
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

        let output = run_command_output(Command::Init, temp.path(), cache.path());

        assert!(output.contains("Creating"));
        assert!(output.contains("nodus.toml"));
        assert!(output.contains("skills/example/SKILL.md"));
        assert!(output.contains("Finished"));
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
                adapter: vec![Adapter::Codex],
                component: vec![],
                sync_on_launch: false,
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
    fn doctor_command_emits_checking_and_finished_lines() {
        let temp = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join(".codex")).unwrap();

        let reporter = Reporter::silent();
        resolver::sync_in_dir(temp.path(), cache.path(), false, false, &reporter).unwrap();

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
                adapter: vec![Adapter::Codex],
                component: vec![],
                sync_on_launch: false,
            },
            temp.path(),
            cache.path(),
            &Reporter::silent(),
        )
        .unwrap();

        let output = run_command_output(
            Command::Update {
                allow_high_sensitivity: false,
            },
            temp.path(),
            cache.path(),
        );

        assert!(output.contains("Checking"));
        assert!(output.contains("Resolving"));
        assert!(output.contains("Finished"));
    }
}
