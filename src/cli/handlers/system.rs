use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::cli::args::Cli;
use crate::cli::handlers::CommandContext;

pub(crate) fn handle_upgrade(context: &CommandContext<'_>, check: bool) -> anyhow::Result<()> {
    crate::update_checker::upgrade(context.reporter, check)
}

pub(crate) fn handle_completion(shell: Shell) -> anyhow::Result<()> {
    let mut command = Cli::command();
    let name = command.get_name().to_string();
    generate(shell, &mut command, name, &mut std::io::stdout());
    Ok(())
}
