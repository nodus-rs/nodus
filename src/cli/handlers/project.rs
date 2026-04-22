use std::path::PathBuf;

use anyhow::Context;

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
    pub(crate) create_missing: bool,
}

pub(crate) struct SyncCommand {
    pub(crate) locked: bool,
    pub(crate) frozen: bool,
    pub(crate) allow_high_sensitivity: bool,
    pub(crate) strict: bool,
    pub(crate) force: bool,
    pub(crate) adapter: Vec<Adapter>,
    pub(crate) sync_on_launch: bool,
    pub(crate) no_sync_on_launch: bool,
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
        create_missing,
    } = command;

    if packages.len() > 1 {
        anyhow::ensure!(
            repo_path.is_none(),
            "`nodus relay --repo-path` requires exactly one dependency"
        );
    }

    if watch {
        let rt = tokio::runtime::Runtime::new()
            .context("failed to create async runtime for relay watch")?;
        if packages.len() == 1 {
            rt.block_on(crate::relay::watch_dependency_in_dir(
                context.cwd,
                context.cache_root,
                &packages[0],
                repo_path.as_deref(),
                via,
                create_missing,
                context.reporter,
            ))
        } else {
            rt.block_on(crate::relay::watch_dependencies_in_dir(
                context.cwd,
                context.cache_root,
                &packages,
                via,
                create_missing,
                context.reporter,
            ))
        }
    } else {
        let summaries = if dry_run {
            crate::relay::relay_dependencies_in_dir_dry_run(
                context.cwd,
                context.cache_root,
                &packages,
                repo_path.as_deref(),
                via,
                create_missing,
                context.reporter,
            )?
        } else {
            crate::relay::relay_dependencies_in_dir(
                context.cwd,
                context.cache_root,
                &packages,
                repo_path.as_deref(),
                via,
                create_missing,
                context.reporter,
            )?
        };

        let message = if let [summary] = summaries.as_slice() {
            if dry_run {
                format!(
                    "dry run: would relay {} into {}; would create {} and update {} source files",
                    summary.alias,
                    display_path(&summary.linked_repo),
                    summary.created_file_count,
                    summary.updated_file_count,
                )
            } else {
                format!(
                    "relayed {} into {}; created {} and updated {} source files",
                    summary.alias,
                    display_path(&summary.linked_repo),
                    summary.created_file_count,
                    summary.updated_file_count,
                )
            }
        } else {
            let created_file_count = summaries
                .iter()
                .map(|summary| summary.created_file_count)
                .sum::<usize>();
            let updated_file_count = summaries
                .iter()
                .map(|summary| summary.updated_file_count)
                .sum::<usize>();
            if dry_run {
                format!(
                    "dry run: would relay {} dependencies; would create {} and update {} source files",
                    summaries.len(),
                    created_file_count,
                    updated_file_count,
                )
            } else {
                format!(
                    "relayed {} dependencies; created {} and updated {} source files",
                    summaries.len(),
                    created_file_count,
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
        strict,
        force,
        adapter,
        sync_on_launch,
        no_sync_on_launch,
        dry_run,
    } = command;
    let sync_on_launch = if locked || frozen {
        sync_on_launch
    } else {
        sync_on_launch && !no_sync_on_launch
    };
    let summary = if frozen {
        if dry_run {
            if strict {
                crate::resolver::sync_in_dir_with_adapters_frozen_strict_dry_run(
                    context.cwd,
                    context.cache_root,
                    allow_high_sensitivity,
                    force,
                    &adapter,
                    sync_on_launch,
                    context.reporter,
                )?
            } else {
                crate::resolver::sync_in_dir_with_adapters_frozen_dry_run(
                    context.cwd,
                    context.cache_root,
                    allow_high_sensitivity,
                    force,
                    &adapter,
                    sync_on_launch,
                    context.reporter,
                )?
            }
        } else {
            if strict {
                crate::resolver::sync_in_dir_with_adapters_frozen_strict(
                    context.cwd,
                    context.cache_root,
                    allow_high_sensitivity,
                    force,
                    &adapter,
                    sync_on_launch,
                    context.reporter,
                )?
            } else {
                crate::resolver::sync_in_dir_with_adapters_frozen(
                    context.cwd,
                    context.cache_root,
                    allow_high_sensitivity,
                    force,
                    &adapter,
                    sync_on_launch,
                    context.reporter,
                )?
            }
        }
    } else if dry_run {
        if strict {
            crate::resolver::sync_in_dir_with_adapters_strict_dry_run(
                context.cwd,
                context.cache_root,
                locked,
                allow_high_sensitivity,
                force,
                &adapter,
                sync_on_launch,
                context.reporter,
            )?
        } else {
            crate::resolver::sync_in_dir_with_adapters_dry_run(
                context.cwd,
                context.cache_root,
                locked,
                allow_high_sensitivity,
                force,
                &adapter,
                sync_on_launch,
                context.reporter,
            )?
        }
    } else {
        if strict {
            crate::resolver::sync_in_dir_with_adapters_strict(
                context.cwd,
                context.cache_root,
                locked,
                allow_high_sensitivity,
                force,
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
                force,
                &adapter,
                sync_on_launch,
                context.reporter,
            )?
        }
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
