use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;

use super::args::Cli;
use super::output::{should_auto_check_for_updates, uses_json_output};
use super::router::run_command_in_dir;
use crate::report::Reporter;

pub(super) fn run() -> ExitCode {
    let cli = Cli::parse();
    let output_reporter = if uses_json_output(&cli.command) {
        Reporter::stdout()
    } else {
        Reporter::stderr()
    };
    let error_reporter = Reporter::stderr();
    let should_check_for_updates = should_auto_check_for_updates(
        &cli.command,
        std::io::stderr().is_terminal(),
        update_check_disabled(),
    );
    let result = (|| -> anyhow::Result<()> {
        let cwd = std::env::current_dir()?;
        let store_root = crate::cache::resolve_store_root(cli.store_path.as_deref())?;
        run_command_in_dir(cli.command, &cwd, &store_root, &output_reporter)?;
        if should_check_for_updates {
            crate::update_checker::maybe_notify(&store_root, &error_reporter);
        }
        Ok(())
    })();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if error_reporter.error(&error).is_err() {
                eprintln!("error: {error:#}");
            }
            ExitCode::FAILURE
        }
    }
}

fn update_check_disabled() -> bool {
    std::env::var_os("NODUS_NO_UPDATE_CHECK").is_some_and(|value| value != "0")
}
