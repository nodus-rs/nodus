use std::path::Path;

use super::args::Command;
use super::handlers::{CommandContext, dependency, project, query, system};
use crate::report::Reporter;

pub(super) fn run_command_in_dir(
    command: Command,
    cwd: &Path,
    cache_root: &Path,
    reporter: &Reporter,
) -> anyhow::Result<()> {
    let context = CommandContext {
        cwd,
        cache_root,
        reporter,
    };

    match command {
        Command::Add {
            url,
            global,
            dev,
            tag,
            branch,
            version,
            revision,
            adapter,
            component,
            sync_on_launch,
            accept_all_dependencies,
            dry_run,
        } => dependency::handle_add(
            &context,
            dependency::AddCommand {
                url,
                global,
                dev,
                tag,
                branch,
                version,
                revision,
                adapter,
                component,
                sync_on_launch,
                accept_all_dependencies,
                dry_run,
            },
        ),
        Command::Remove {
            package,
            global,
            dry_run,
        } => dependency::handle_remove(&context, package, global, dry_run),
        Command::List { json } => query::handle_list(&context, json),
        Command::Info {
            package,
            tag,
            branch,
            json,
        } => query::handle_info(&context, package, tag, branch, json),
        Command::Review {
            package,
            tag,
            branch,
            provider,
            model,
        } => query::handle_review(
            &context,
            query::ReviewCommand {
                package,
                tag,
                branch,
                provider,
                model,
            },
        ),
        Command::Outdated { json } => query::handle_outdated(&context, json),
        Command::Update {
            allow_high_sensitivity,
            dry_run,
        } => dependency::handle_update(&context, allow_high_sensitivity, dry_run),
        Command::Upgrade { check } => system::handle_upgrade(&context, check),
        Command::Relay {
            packages,
            repo_path,
            via,
            watch,
            dry_run,
            create_missing,
        } => project::handle_relay(
            &context,
            project::RelayCommand {
                packages,
                repo_path,
                via,
                watch,
                dry_run,
                create_missing,
            },
        ),
        Command::Init { dry_run } => project::handle_init(&context, dry_run),
        Command::Sync {
            locked,
            frozen,
            allow_high_sensitivity,
            force,
            adapter,
            sync_on_launch,
            dry_run,
        } => project::handle_sync(
            &context,
            project::SyncCommand {
                locked,
                frozen,
                allow_high_sensitivity,
                force,
                adapter,
                sync_on_launch,
                dry_run,
            },
        ),
        Command::Completion { shell } => system::handle_completion(shell),
        Command::Doctor { json } => query::handle_doctor(&context, json),
    }
}
