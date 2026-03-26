use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

use clap::{CommandFactory, Parser};
use clap_complete::generate;
use serde::Serialize;

use super::args::{Cli, Command};
use crate::adapters::Adapter;
use crate::manifest::{DependencyKind, RequestedGitRef};
use crate::paths::display_path;
use crate::report::Reporter;

pub fn run() -> ExitCode {
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

pub(super) fn run_command_in_dir(
    command: Command,
    cwd: &Path,
    cache_root: &Path,
    reporter: &Reporter,
) -> anyhow::Result<()> {
    match command {
        Command::Add {
            url,
            dev,
            tag,
            branch,
            version,
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
                        version_req: version
                            .as_deref()
                            .map(semver::VersionReq::parse)
                            .transpose()?,
                        kind: if dev {
                            DependencyKind::DevDependency
                        } else {
                            DependencyKind::Dependency
                        },
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
                        version_req: version
                            .as_deref()
                            .map(semver::VersionReq::parse)
                            .transpose()?,
                        kind: if dev {
                            DependencyKind::DevDependency
                        } else {
                            DependencyKind::Dependency
                        },
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
                    display_dependency(summary.kind, &summary.alias),
                    summary.reference,
                    format_adapters(&summary.adapters),
                    summary.managed_file_count,
                )
            } else {
                format!(
                    "added {} {} with adapters [{}]; wrote {} managed files",
                    display_dependency(summary.kind, &summary.alias),
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
                    display_dependency(summary.kind, &summary.alias),
                    summary.managed_file_count,
                )
            } else {
                format!(
                    "removed {} and wrote {} managed files",
                    display_dependency(summary.kind, &summary.alias),
                    summary.managed_file_count,
                )
            };
            reporter.finish(message)?;
            Ok(())
        }
        Command::List { json } => {
            if json {
                write_json(reporter, &crate::list::list_dependencies_json_in_dir(cwd)?)
            } else {
                crate::list::list_dependencies_in_dir(cwd, reporter)
            }
        }
        Command::Info {
            package,
            tag,
            branch,
            json,
        } => {
            if json {
                write_json(
                    reporter,
                    &crate::info::describe_package_json_in_dir(
                        cwd,
                        cache_root,
                        &package,
                        tag.as_deref(),
                        branch.as_deref(),
                    )?,
                )
            } else {
                crate::info::describe_package_in_dir(
                    cwd,
                    cache_root,
                    &package,
                    tag.as_deref(),
                    branch.as_deref(),
                    reporter,
                )
            }
        }
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
        Command::Outdated { json } => {
            if json {
                write_json(
                    reporter,
                    &crate::outdated::check_outdated_json_in_dir(cwd, cache_root)?,
                )
            } else {
                let summary = crate::outdated::check_outdated_in_dir(cwd, cache_root, reporter)?;
                let outcome = if summary.outdated_count == 0 {
                    format!(
                        "checked {} dependencies; all current",
                        summary.dependency_count
                    )
                } else {
                    format!(
                        "checked {} dependencies; {} outdated",
                        summary.dependency_count, summary.outdated_count
                    )
                };
                reporter.finish(outcome)?;
                Ok(())
            }
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
                    "dry run: would update {} dependencies; would write {} managed files",
                    summary.updated_count, summary.managed_file_count
                )
            } else {
                format!(
                    "updated {} dependencies; wrote {} managed files",
                    summary.updated_count, summary.managed_file_count
                )
            };
            reporter.finish(message)?;
            Ok(())
        }
        Command::Upgrade { check } => crate::update_checker::upgrade(reporter, check),
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
            packages,
            repo_path,
            via,
            watch,
            dry_run,
        } => {
            if packages.len() > 1 {
                anyhow::ensure!(
                    repo_path.is_none(),
                    "`nodus relay --repo-path` requires exactly one dependency"
                );
            }

            if watch {
                if packages.len() == 1 {
                    crate::relay::watch_dependency_in_dir(
                        cwd,
                        cache_root,
                        &packages[0],
                        repo_path.as_deref(),
                        via,
                        reporter,
                    )
                } else {
                    crate::relay::watch_dependencies_in_dir(
                        cwd, cache_root, &packages, via, reporter,
                    )
                }
            } else {
                let mut summaries = Vec::with_capacity(packages.len());
                for package in &packages {
                    let summary = if dry_run {
                        crate::relay::relay_dependency_in_dir_dry_run(
                            cwd,
                            cache_root,
                            package,
                            repo_path.as_deref(),
                            via,
                            reporter,
                        )?
                    } else {
                        crate::relay::relay_dependency_in_dir(
                            cwd,
                            cache_root,
                            package,
                            repo_path.as_deref(),
                            via,
                            reporter,
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
        Command::Doctor { json } => {
            if json {
                let summary = crate::resolver::doctor_in_dir(cwd, cache_root, &Reporter::silent())?;
                write_json(reporter, &summary)
            } else {
                let summary = crate::resolver::doctor_in_dir(cwd, cache_root, reporter)?;
                reporter.finish(format!(
                    "project state is consistent across {} packages",
                    summary.package_count,
                ))?;
                Ok(())
            }
        }
        Command::Completion { shell } => {
            let mut command = Cli::command();
            let name = command.get_name().to_string();
            generate(shell, &mut command, name, &mut std::io::stdout());
            Ok(())
        }
    }
}

fn uses_json_output(command: &Command) -> bool {
    match command {
        Command::List { json }
        | Command::Info { json, .. }
        | Command::Outdated { json }
        | Command::Doctor { json } => *json,
        _ => false,
    }
}

pub(super) fn should_auto_check_for_updates(
    command: &Command,
    stderr_is_terminal: bool,
    update_check_disabled: bool,
) -> bool {
    stderr_is_terminal
        && !update_check_disabled
        && !uses_json_output(command)
        && !matches!(
            command,
            Command::Completion { .. } | Command::Upgrade { .. }
        )
}

fn update_check_disabled() -> bool {
    std::env::var_os("NODUS_NO_UPDATE_CHECK").is_some_and(|value| value != "0")
}

fn write_json<T: Serialize>(reporter: &Reporter, value: &T) -> anyhow::Result<()> {
    reporter.line(serde_json::to_string_pretty(value)?)
}

fn format_adapters(adapters: &[Adapter]) -> String {
    adapters
        .iter()
        .map(|adapter| adapter.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn display_dependency(kind: DependencyKind, alias: &str) -> String {
    if kind.is_dev() {
        format!("{alias} [dev]")
    } else {
        alias.to_string()
    }
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
