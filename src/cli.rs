use std::process::ExitCode;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::adapters::Adapter;
use crate::report::{ColorMode, Reporter};

#[derive(Debug, Parser)]
#[command(author, version, about = "Nodus manages project-scoped agent packages", long_about = None)]
struct Cli {
    #[arg(long, global = true)]
    cache_path: Option<PathBuf>,

    #[arg(long, global = true, value_enum, default_value_t = ColorMode::Auto)]
    color: ColorMode,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Add {
        url: String,
        #[arg(long)]
        tag: Option<String>,
        #[arg(long, value_enum)]
        adapter: Vec<Adapter>,
    },
    Remove {
        package: String,
    },
    Init,
    Sync {
        #[arg(long)]
        locked: bool,
        #[arg(long = "allow-high-sensitivity")]
        allow_high_sensitivity: bool,
        #[arg(long, value_enum)]
        adapter: Vec<Adapter>,
    },
    Doctor,
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let reporter = Reporter::stderr(cli.color);
    let result = run_command(cli);

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

fn run_command(cli: Cli) -> anyhow::Result<()> {
    let cache_root = crate::cache::resolve_cache_root(cli.cache_path.as_deref())?;

    match cli.command {
        Command::Add { url, tag, adapter } => {
            crate::git::add_dependency_with_adapters(&cache_root, &url, tag.as_deref(), &adapter)
        }
        Command::Remove { package } => crate::git::remove_dependency(&cache_root, &package),
        Command::Init => crate::manifest::scaffold_init(),
        Command::Sync {
            locked,
            allow_high_sensitivity,
            adapter,
        } => crate::resolver::sync_with_adapters(
            &cache_root,
            locked,
            allow_high_sensitivity,
            &adapter,
        ),
        Command::Doctor => crate::resolver::doctor(&cache_root),
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command};
    use clap::Parser;

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
    fn parses_color_flag() {
        let cli = Cli::try_parse_from(["nodus", "--color", "never", "doctor"]).unwrap();

        assert_eq!(cli.color, super::ColorMode::Never);
    }
}
