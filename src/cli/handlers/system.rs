use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::cli::args::Cli;
use crate::cli::handlers::CommandContext;
use crate::execution::ExecutionMode;

pub(crate) fn handle_upgrade(context: &CommandContext<'_>, check: bool) -> anyhow::Result<()> {
    crate::update_checker::upgrade(context.reporter, check)
}

pub(crate) fn handle_clean(
    context: &CommandContext<'_>,
    all: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    let execution_mode = if dry_run {
        ExecutionMode::DryRun
    } else {
        ExecutionMode::Apply
    };
    let summary = if all {
        crate::clean::clean_all_cache(context.cache_root, execution_mode, context.reporter)?
    } else {
        crate::clean::clean_project_cache(
            context.cwd,
            context.cache_root,
            execution_mode,
            context.reporter,
        )?
    };

    let message = if all {
        format!(
            "{}clear {} shared cache director{}",
            if dry_run { "dry run: would " } else { "" },
            summary.repository_count + summary.checkout_count + summary.snapshot_count,
            if summary.repository_count + summary.checkout_count + summary.snapshot_count == 1 {
                "y"
            } else {
                "ies"
            }
        )
    } else {
        format!(
            "{}remove {} repositor{}, {} checkout{}, {} snapshot{}",
            if dry_run { "dry run: would " } else { "" },
            summary.repository_count,
            if summary.repository_count == 1 {
                "y mirror"
            } else {
                "y mirrors"
            },
            summary.checkout_count,
            if summary.checkout_count == 1 { "" } else { "s" },
            summary.snapshot_count,
            if summary.snapshot_count == 1 { "" } else { "s" },
        )
    };
    context.reporter.finish(message)?;
    Ok(())
}

pub(crate) fn handle_completion(shell: Shell) -> anyhow::Result<()> {
    let mut command = Cli::command();
    let name = command.get_name().to_string();
    generate(shell, &mut command, name, &mut std::io::stdout());
    Ok(())
}
