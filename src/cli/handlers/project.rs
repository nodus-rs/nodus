use std::path::PathBuf;

use crate::adapters::Adapter;
use crate::cli::handlers::CommandContext;
use crate::cli::output::format_adapters;
use crate::paths::display_path;

pub(crate) struct RelayCommand {
    pub(crate) packages: Vec<String>,
    pub(crate) repo_path: Option<PathBuf>,
    pub(crate) via: Option<Adapter>,
    pub(crate) watch: bool,
    pub(crate) dry_run: bool,
}

pub(crate) struct SyncCommand {
    pub(crate) locked: bool,
    pub(crate) frozen: bool,
    pub(crate) allow_high_sensitivity: bool,
    pub(crate) adapter: Vec<Adapter>,
    pub(crate) sync_on_launch: bool,
    pub(crate) dry_run: bool,
}

pub(crate) fn handle_init(context: &CommandContext<'_>, dry_run: bool) -> anyhow::Result<()> {
    let summary = if dry_run {
        crate::manifest::scaffold_init_in_dir_dry_run(context.cwd, context.reporter)?
    } else {
        crate::manifest::scaffold_init_in_dir(context.cwd, context.reporter)?
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
    context.reporter.finish(message)?;
    Ok(())
}

pub(crate) fn handle_relay(
    context: &CommandContext<'_>,
    command: RelayCommand,
) -> anyhow::Result<()> {
    let RelayCommand {
        packages,
        repo_path,
        via,
        watch,
        dry_run,
    } = command;

    if packages.len() > 1 {
        anyhow::ensure!(
            repo_path.is_none(),
            "`nodus relay --repo-path` requires exactly one dependency"
        );
    }

    if watch {
        if packages.len() == 1 {
            crate::relay::watch_dependency_in_dir(
                context.cwd,
                context.cache_root,
                &packages[0],
                repo_path.as_deref(),
                via,
                context.reporter,
            )
        } else {
            crate::relay::watch_dependencies_in_dir(
                context.cwd,
                context.cache_root,
                &packages,
                via,
                context.reporter,
            )
        }
    } else {
        let mut summaries = Vec::with_capacity(packages.len());
        for package in &packages {
            let summary = if dry_run {
                crate::relay::relay_dependency_in_dir_dry_run(
                    context.cwd,
                    context.cache_root,
                    package,
                    repo_path.as_deref(),
                    via,
                    context.reporter,
                )?
            } else {
                crate::relay::relay_dependency_in_dir(
                    context.cwd,
                    context.cache_root,
                    package,
                    repo_path.as_deref(),
                    via,
                    context.reporter,
                )?
            };
            summaries.push(summary);
        }

        let message = if let [summary] = summaries.as_slice() {
            if dry_run {
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
            }
        } else {
            let updated_file_count = summaries
                .iter()
                .map(|summary| summary.updated_file_count)
                .sum::<usize>();
            if dry_run {
                format!(
                    "dry run: would relay {} dependencies; would update {} source files",
                    summaries.len(),
                    updated_file_count,
                )
            } else {
                format!(
                    "relayed {} dependencies; updated {} source files",
                    summaries.len(),
                    updated_file_count,
                )
            }
        };
        context.reporter.finish(message)?;
        Ok(())
    }
}

pub(crate) fn handle_sync(
    context: &CommandContext<'_>,
    command: SyncCommand,
) -> anyhow::Result<()> {
    let SyncCommand {
        locked,
        frozen,
        allow_high_sensitivity,
        adapter,
        sync_on_launch,
        dry_run,
    } = command;
    let summary = if frozen {
        if dry_run {
            crate::resolver::sync_in_dir_with_adapters_frozen_dry_run(
                context.cwd,
                context.cache_root,
                allow_high_sensitivity,
                &adapter,
                sync_on_launch,
                context.reporter,
            )?
        } else {
            crate::resolver::sync_in_dir_with_adapters_frozen(
                context.cwd,
                context.cache_root,
                allow_high_sensitivity,
                &adapter,
                sync_on_launch,
                context.reporter,
            )?
        }
    } else if dry_run {
        crate::resolver::sync_in_dir_with_adapters_dry_run(
            context.cwd,
            context.cache_root,
            locked,
            allow_high_sensitivity,
            &adapter,
            sync_on_launch,
            context.reporter,
        )?
    } else {
        crate::resolver::sync_in_dir_with_adapters(
            context.cwd,
            context.cache_root,
            locked,
            allow_high_sensitivity,
            &adapter,
            sync_on_launch,
            context.reporter,
        )?
    };
    context.reporter.finish(format!(
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
