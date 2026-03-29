use crate::adapters::Adapter;
use crate::cli::handlers::CommandContext;
use crate::cli::output::{display_dependency, format_adapters};
use crate::install_paths::InstallPaths;
use crate::manifest::{DependencyComponent, DependencyKind, RequestedGitRef};

pub(crate) struct AddCommand {
    pub(crate) url: String,
    pub(crate) global: bool,
    pub(crate) dev: bool,
    pub(crate) tag: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) revision: Option<String>,
    pub(crate) adapter: Vec<Adapter>,
    pub(crate) component: Vec<DependencyComponent>,
    pub(crate) sync_on_launch: bool,
    pub(crate) accept_all_dependencies: bool,
    pub(crate) dry_run: bool,
}

pub(crate) fn handle_add(context: &CommandContext<'_>, command: AddCommand) -> anyhow::Result<()> {
    let AddCommand {
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
    } = command;
    if global && sync_on_launch {
        anyhow::bail!("`nodus add --global` does not support `--sync-on-launch`");
    }
    let install_paths = if global {
        InstallPaths::global(context.cache_root)?
    } else {
        InstallPaths::project(context.cwd)
    };
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
        accept_all_dependencies,
    };
    let summary = if dry_run {
        crate::git::add_dependency_at_paths_with_adapters_dry_run(
            &install_paths,
            context.cache_root,
            &url,
            options,
            context.reporter,
        )?
    } else {
        crate::git::add_dependency_at_paths_with_adapters(
            &install_paths,
            context.cache_root,
            &url,
            options,
            context.reporter,
        )?
    };
    if !summary.dependency_members.is_empty() {
        let intro = if dry_run {
            "dependency child selection:"
        } else {
            "dependency child packages:"
        };
        context.reporter.line(intro)?;
        context
            .reporter
            .line(format!("  config: {}", summary.dependency_preview))?;
        for member in &summary.dependency_members {
            let status = if member.enabled {
                "enabled"
            } else {
                "disabled"
            };
            context
                .reporter
                .line(format!("  {} ({status})", member.id))?;
        }
        if summary
            .dependency_members
            .iter()
            .all(|member| !member.enabled)
        {
            let message = if dry_run {
                "multiple child packages were detected; Nodus would record the wrapper only. Edit `members` after install or rerun with `--accept-all-dependencies` to enable every child package."
            } else {
                "multiple child packages were detected; Nodus recorded the wrapper only. Edit `members` in `nodus.toml` to enable the child packages you want."
            };
            context.reporter.note(message)?;
        }
    }
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
    global: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    let install_paths = if global {
        InstallPaths::global(context.cache_root)?
    } else {
        InstallPaths::project(context.cwd)
    };
    let summary = if dry_run {
        crate::git::remove_dependency_at_paths_dry_run(
            &install_paths,
            context.cache_root,
            &package,
            context.reporter,
        )?
    } else {
        crate::git::remove_dependency_at_paths(
            &install_paths,
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
