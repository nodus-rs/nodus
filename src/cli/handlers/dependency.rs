use crate::adapters::Adapter;
use crate::cli::handlers::CommandContext;
use crate::cli::output::{display_dependency, format_adapters};
use crate::manifest::{DependencyComponent, DependencyKind, RequestedGitRef};

pub(crate) struct AddCommand {
    pub(crate) url: String,
    pub(crate) dev: bool,
    pub(crate) tag: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) revision: Option<String>,
    pub(crate) adapter: Vec<Adapter>,
    pub(crate) component: Vec<DependencyComponent>,
    pub(crate) sync_on_launch: bool,
    pub(crate) dry_run: bool,
}

pub(crate) fn handle_add(context: &CommandContext<'_>, command: AddCommand) -> anyhow::Result<()> {
    let AddCommand {
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
    } = command;
    let options = crate::git::AddDependencyOptions {
        git_ref: requested_git_ref(tag.as_deref(), branch.as_deref(), revision.as_deref())?,
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
    };
    let summary = if dry_run {
        crate::git::add_dependency_in_dir_with_adapters_dry_run(
            context.cwd,
            context.cache_root,
            &url,
            options,
            context.reporter,
        )?
    } else {
        crate::git::add_dependency_in_dir_with_adapters(
            context.cwd,
            context.cache_root,
            &url,
            options,
            context.reporter,
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
    context.reporter.finish(message)?;
    Ok(())
}

pub(crate) fn handle_remove(
    context: &CommandContext<'_>,
    package: String,
    dry_run: bool,
) -> anyhow::Result<()> {
    let summary = if dry_run {
        crate::git::remove_dependency_in_dir_dry_run(
            context.cwd,
            context.cache_root,
            &package,
            context.reporter,
        )?
    } else {
        crate::git::remove_dependency_in_dir(
            context.cwd,
            context.cache_root,
            &package,
            context.reporter,
        )?
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
    context.reporter.finish(message)?;
    Ok(())
}

pub(crate) fn handle_update(
    context: &CommandContext<'_>,
    allow_high_sensitivity: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    let summary = if dry_run {
        crate::update::update_direct_dependencies_in_dir_dry_run(
            context.cwd,
            context.cache_root,
            allow_high_sensitivity,
            context.reporter,
        )?
    } else {
        crate::update::update_direct_dependencies_in_dir(
            context.cwd,
            context.cache_root,
            allow_high_sensitivity,
            context.reporter,
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
    context.reporter.finish(message)?;
    Ok(())
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
