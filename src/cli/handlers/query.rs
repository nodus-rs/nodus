use crate::cli::handlers::CommandContext;
use crate::cli::output::write_json;
use crate::report::Reporter;
use crate::resolver::DoctorMode;
use crate::review::ReviewProvider;

pub(crate) struct ReviewCommand {
    pub(crate) package: String,
    pub(crate) tag: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) provider: ReviewProvider,
    pub(crate) model: Option<String>,
}

pub(crate) struct DoctorCommand {
    pub(crate) check: bool,
    pub(crate) force: bool,
    pub(crate) json: bool,
}

pub(crate) fn handle_list(context: &CommandContext<'_>, json: bool) -> anyhow::Result<()> {
    if json {
        write_json(
            context.reporter,
            &crate::list::list_dependencies_json_in_dir(context.cwd)?,
        )
    } else {
        crate::list::list_dependencies_in_dir(context.cwd, context.reporter)
    }
}

pub(crate) fn handle_info(
    context: &CommandContext<'_>,
    package: String,
    tag: Option<String>,
    branch: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    if json {
        write_json(
            context.reporter,
            &crate::info::describe_package_json_in_dir(
                context.cwd,
                context.cache_root,
                &package,
                tag.as_deref(),
                branch.as_deref(),
            )?,
        )
    } else {
        crate::info::describe_package_in_dir(
            context.cwd,
            context.cache_root,
            &package,
            tag.as_deref(),
            branch.as_deref(),
            context.reporter,
        )
    }
}

pub(crate) fn handle_review(
    context: &CommandContext<'_>,
    command: ReviewCommand,
) -> anyhow::Result<()> {
    let ReviewCommand {
        package,
        tag,
        branch,
        provider,
        model,
    } = command;
    let summary = crate::review::review_package_in_dir(
        context.cwd,
        context.cache_root,
        crate::review::ReviewRequest {
            package: &package,
            tag: tag.as_deref(),
            branch: branch.as_deref(),
            provider,
            model: model.as_deref(),
        },
        context.reporter,
    )?;
    context.reporter.finish(format!(
        "reviewed {} packages with {}",
        summary.package_count, summary.provider
    ))?;
    Ok(())
}

pub(crate) fn handle_outdated(context: &CommandContext<'_>, json: bool) -> anyhow::Result<()> {
    if json {
        write_json(
            context.reporter,
            &crate::outdated::check_outdated_json_in_dir(context.cwd, context.cache_root)?,
        )
    } else {
        let summary = crate::outdated::check_outdated_in_dir(
            context.cwd,
            context.cache_root,
            context.reporter,
        )?;
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
        context.reporter.finish(outcome)?;
        Ok(())
    }
}

pub(crate) fn handle_doctor(
    context: &CommandContext<'_>,
    command: DoctorCommand,
) -> anyhow::Result<()> {
    let DoctorCommand { check, force, json } = command;
    let mode = if force {
        DoctorMode::Force
    } else if check {
        DoctorMode::Check
    } else {
        DoctorMode::Repair
    };
    if json {
        let summary = crate::resolver::doctor_in_dir_with_mode(
            context.cwd,
            context.cache_root,
            mode,
            &Reporter::silent(),
        )?;
        write_json(context.reporter, &summary)
    } else {
        let summary = crate::resolver::doctor_in_dir_with_mode(
            context.cwd,
            context.cache_root,
            mode,
            context.reporter,
        )?;
        let outcome = match summary.status {
            crate::resolver::DoctorStatus::Healthy => {
                format!(
                    "project state is consistent across {} packages",
                    summary.package_count
                )
            }
            crate::resolver::DoctorStatus::Fixed => {
                format!(
                    "repaired project state across {} packages",
                    summary.package_count
                )
            }
            crate::resolver::DoctorStatus::Blocked => {
                format!(
                    "project state is blocked across {} packages",
                    summary.package_count
                )
            }
        };
        context.reporter.finish(outcome)?;
        Ok(())
    }
}
