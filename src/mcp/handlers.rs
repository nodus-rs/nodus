use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use clap::ValueEnum;
use semver::VersionReq;
use serde_json::Value as JsonValue;

use super::tools::*;
use crate::adapters::Adapter;
use crate::git::{
    AddDependencyOptions, add_dependency_at_paths_with_adapters,
    add_dependency_at_paths_with_adapters_dry_run, remove_dependency_in_dir,
};
use crate::install_paths::InstallPaths;
use crate::local_config::LocalConfig;
use crate::manifest::{DependencyComponent, DependencyKind, RequestedGitRef};
use crate::report::{ColorMode, Reporter};

pub fn dispatch_tool(
    tool_name: &str,
    args: &JsonValue,
    cwd: &Path,
    cache_root: &Path,
) -> Result<String> {
    match tool_name {
        TOOL_LIST => handle_list(cwd),
        TOOL_INFO => handle_info(args, cwd, cache_root),
        TOOL_SYNC => handle_sync(cwd, cache_root),
        TOOL_ADD => handle_add(args, cwd, cache_root),
        TOOL_REMOVE => handle_remove(args, cwd, cache_root),
        TOOL_RELAY => handle_relay(args, cwd, cache_root),
        TOOL_RELAY_STATUS => handle_relay_status(args, cwd, cache_root),
        TOOL_CHECK_UPDATES => handle_check_updates(cwd, cache_root),
        _ => bail!("unknown tool: {tool_name}"),
    }
}

fn handle_list(cwd: &Path) -> Result<String> {
    let list = crate::list::list_dependencies_json_in_dir(cwd)?;
    Ok(serde_json::to_string_pretty(&list)?)
}

fn handle_info(args: &JsonValue, cwd: &Path, cache_root: &Path) -> Result<String> {
    let package = args.get("package").and_then(|v| v.as_str()).unwrap_or(".");
    let info = crate::info::describe_package_json_in_dir(cwd, cache_root, package, None, None)?;
    Ok(serde_json::to_string_pretty(&info)?)
}

fn handle_sync(cwd: &Path, cache_root: &Path) -> Result<String> {
    capture_output(|reporter| {
        crate::resolver::sync_in_dir_with_adapters(
            cwd,
            cache_root,
            false, // locked
            false, // allow_high_sensitivity
            false, // force
            &[],   // adapters
            false, // sync_on_launch
            reporter,
        )?;
        Ok(())
    })
}

fn handle_add(args: &JsonValue, cwd: &Path, cache_root: &Path) -> Result<String> {
    let ParsedAddArgs {
        package,
        global,
        dry_run,
        git_ref,
        version_req,
        kind,
        adapters,
        components,
        sync_on_launch,
        accept_all_dependencies,
    } = parse_add_args(args)?;

    let install_paths = if global {
        InstallPaths::global(cache_root)?
    } else {
        InstallPaths::project(cwd)
    };

    capture_output(|reporter| {
        let options = AddDependencyOptions {
            git_ref,
            version_req,
            kind,
            adapters: &adapters,
            components: &components,
            sync_on_launch,
            accept_all_dependencies,
        };
        if dry_run {
            add_dependency_at_paths_with_adapters_dry_run(
                &install_paths,
                cache_root,
                package,
                options,
                reporter,
            )?;
        } else {
            add_dependency_at_paths_with_adapters(
                &install_paths,
                cache_root,
                package,
                options,
                reporter,
            )?;
        }
        Ok(())
    })
}

#[derive(Debug)]
struct ParsedAddArgs<'a> {
    package: &'a str,
    global: bool,
    dry_run: bool,
    git_ref: Option<RequestedGitRef<'a>>,
    version_req: Option<VersionReq>,
    kind: DependencyKind,
    adapters: Vec<Adapter>,
    components: Vec<DependencyComponent>,
    sync_on_launch: bool,
    accept_all_dependencies: bool,
}

fn parse_add_args(args: &JsonValue) -> Result<ParsedAddArgs<'_>> {
    let package = string_arg(args, "package")?;
    let global = bool_arg(args, "global")?;
    let dev = bool_arg(args, "dev")?;
    let dry_run = bool_arg(args, "dry_run")?;
    let sync_on_launch = bool_arg(args, "sync_on_launch")?;
    let accept_all_dependencies = bool_arg(args, "accept_all_dependencies")?;

    if global && sync_on_launch {
        bail!("`nodus_add` with `global: true` does not support `sync_on_launch: true`");
    }

    let tag = optional_string_arg(args, "tag")?;
    let branch = optional_string_arg(args, "branch")?;
    let version = optional_string_arg(args, "version")?;
    let revision = optional_string_arg(args, "revision")?;
    let selector_count = [tag, branch, version, revision]
        .into_iter()
        .filter(Option::is_some)
        .count();
    if selector_count > 1 {
        bail!(
            "`nodus_add` must not declare more than one of `tag`, `branch`, `version`, or `revision`"
        );
    }

    let git_ref = match (tag, branch, revision) {
        (Some(tag), None, None) => Some(RequestedGitRef::Tag(tag)),
        (None, Some(branch), None) => Some(RequestedGitRef::Branch(branch)),
        (None, None, Some(revision)) => Some(RequestedGitRef::Revision(revision)),
        (None, None, None) => None,
        _ => unreachable!("selector_count validation rejects multiple Git refs"),
    };

    let version_req = version.map(VersionReq::parse).transpose()?;

    let adapters = parse_string_array::<Adapter>(args, "adapter")?;
    let components = DependencyComponent::selected_with_exclusions(
        &parse_string_array::<DependencyComponent>(args, "component")?,
        &parse_string_array::<DependencyComponent>(args, "exclude_component")?,
    )
    .map_err(anyhow::Error::msg)?;

    Ok(ParsedAddArgs {
        package,
        global,
        dry_run,
        git_ref,
        version_req,
        kind: if dev {
            DependencyKind::DevDependency
        } else {
            DependencyKind::Dependency
        },
        adapters,
        components,
        sync_on_launch: sync_on_launch && !global,
        accept_all_dependencies,
    })
}

fn string_arg<'a>(args: &'a JsonValue, key: &str) -> Result<&'a str> {
    args.get(key)
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: {key}"))?
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("parameter `{key}` must be a string"))
}

fn optional_string_arg<'a>(args: &'a JsonValue, key: &str) -> Result<Option<&'a str>> {
    let Some(value) = args.get(key) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("parameter `{key}` must be a string"))
}

fn bool_arg(args: &JsonValue, key: &str) -> Result<bool> {
    let Some(value) = args.get(key) else {
        return Ok(false);
    };
    value
        .as_bool()
        .ok_or_else(|| anyhow::anyhow!("parameter `{key}` must be a boolean"))
}

fn handle_remove(args: &JsonValue, cwd: &Path, cache_root: &Path) -> Result<String> {
    let package = args
        .get("package")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: package"))?;

    capture_output(|reporter| {
        remove_dependency_in_dir(cwd, cache_root, package, reporter)?;
        Ok(())
    })
}

fn handle_relay(args: &JsonValue, cwd: &Path, cache_root: &Path) -> Result<String> {
    let packages = relay_packages(args, cwd)?;

    capture_output(|reporter| {
        crate::relay::relay_dependencies_in_dir(
            cwd, cache_root, &packages, None,  // repo_path_override
            None,  // via_override
            false, // create_missing
            reporter,
        )?;
        Ok(())
    })
}

fn handle_relay_status(args: &JsonValue, cwd: &Path, cache_root: &Path) -> Result<String> {
    let packages = relay_packages(args, cwd)?;

    capture_output(|reporter| {
        crate::relay::relay_dependencies_in_dir_dry_run(
            cwd, cache_root, &packages, None,  // repo_path_override
            None,  // via_override
            false, // create_missing
            reporter,
        )?;
        Ok(())
    })
}

fn handle_check_updates(cwd: &Path, cache_root: &Path) -> Result<String> {
    let report = crate::outdated::check_outdated_json_in_dir(cwd, cache_root)?;
    Ok(serde_json::to_string_pretty(&report)?)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn relay_packages(args: &JsonValue, cwd: &Path) -> Result<Vec<String>> {
    if let Some(pkg) = args.get("package").and_then(|v| v.as_str()) {
        return Ok(vec![pkg.to_string()]);
    }
    let config = LocalConfig::load_in_dir(cwd)?;
    Ok(config.relay.keys().cloned().collect())
}

fn parse_string_array<T: ValueEnum>(args: &JsonValue, key: &str) -> Result<Vec<T>> {
    let Some(arr) = args.get(key) else {
        return Ok(Vec::new());
    };
    let values = arr
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("parameter `{key}` must be an array"))?;
    values
        .iter()
        .map(|v| {
            let s = v
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("elements of `{key}` must be strings"))?;
            T::from_str(s, true).map_err(|_| anyhow::anyhow!("invalid value for `{key}`: {s}"))
        })
        .collect()
}

fn capture_output<F>(f: F) -> Result<String>
where
    F: FnOnce(&Reporter) -> Result<()>,
{
    let buffer = SharedOutputBuffer::default();
    let reporter = Reporter::sink(ColorMode::Never, buffer.clone());
    f(&reporter)?;
    Ok(buffer.into_string())
}

#[derive(Clone, Default)]
struct SharedOutputBuffer(Arc<Mutex<Vec<u8>>>);

impl SharedOutputBuffer {
    fn into_string(self) -> String {
        let bytes = self.0.lock().unwrap().clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Write for SharedOutputBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::manifest::MANIFEST_FILE;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn run_git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_output(path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_git_skill_package(path: &Path) -> String {
        write_file(
            &path.join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Review code.\n---\n# Review\n",
        );
        run_git(path, &["init"]);
        run_git(path, &["config", "user.email", "test@example.com"]);
        run_git(path, &["config", "user.name", "Test User"]);
        run_git(path, &["add", "."]);
        run_git(path, &["commit", "-m", "initial"]);
        git_output(path, &["rev-parse", "HEAD"])
    }

    #[test]
    fn parses_add_cli_parity_options() {
        let args = json!({
            "package": "owner/repo",
            "global": true,
            "dev": true,
            "revision": "abc123",
            "adapter": ["claude", "codex"],
            "component": ["skills", "mcp"],
            "exclude_component": ["mcp"],
            "accept_all_dependencies": true,
            "dry_run": true
        });

        let parsed = parse_add_args(&args).unwrap();

        assert_eq!(parsed.package, "owner/repo");
        assert!(parsed.global);
        assert!(parsed.dry_run);
        assert!(matches!(
            parsed.git_ref,
            Some(RequestedGitRef::Revision("abc123"))
        ));
        assert_eq!(parsed.version_req, None);
        assert_eq!(parsed.kind, DependencyKind::DevDependency);
        assert_eq!(parsed.adapters, vec![Adapter::Claude, Adapter::Codex]);
        assert_eq!(parsed.components, vec![DependencyComponent::Skills]);
        assert!(!parsed.sync_on_launch);
        assert!(parsed.accept_all_dependencies);
    }

    #[test]
    fn rejects_add_with_multiple_selectors() {
        let args = json!({
            "package": "owner/repo",
            "tag": "v1.0.0",
            "revision": "abc123"
        });

        let error = parse_add_args(&args).unwrap_err().to_string();

        assert!(error.contains("more than one of `tag`, `branch`, `version`, or `revision`"));
    }

    #[test]
    fn parses_add_version_requirement() {
        let args = json!({
            "package": "owner/repo",
            "version": "^1.2.0"
        });

        let parsed = parse_add_args(&args).unwrap();

        assert!(parsed.git_ref.is_none());
        assert_eq!(parsed.version_req.unwrap().to_string(), "^1.2.0");
    }

    #[test]
    fn rejects_global_add_sync_on_launch() {
        let args = json!({
            "package": "owner/repo",
            "global": true,
            "sync_on_launch": true
        });

        let error = parse_add_args(&args).unwrap_err().to_string();

        assert!(error.contains("does not support `sync_on_launch: true`"));
    }

    #[test]
    fn dispatch_add_can_write_dev_revision_dependency() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let package = TempDir::new().unwrap();
        let revision = init_git_skill_package(package.path());
        let package_url = package.path().to_string_lossy().into_owned();

        dispatch_tool(
            TOOL_ADD,
            &json!({
                "package": package_url,
                "dev": true,
                "revision": revision,
                "adapter": ["claude"],
                "component": ["skills"]
            }),
            project.path(),
            cache.path(),
        )
        .unwrap();

        let manifest = fs::read_to_string(project.path().join(MANIFEST_FILE)).unwrap();
        assert!(manifest.contains("[dev-dependencies]"));
        assert!(manifest.contains(&format!("revision = \"{revision}\"")));
        assert!(manifest.contains("components = [\"skills\"]"));
    }

    #[test]
    fn dispatch_add_dry_run_does_not_write_project_manifest() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let package = TempDir::new().unwrap();
        init_git_skill_package(package.path());
        let package_url = package.path().to_string_lossy().into_owned();

        dispatch_tool(
            TOOL_ADD,
            &json!({
                "package": package_url,
                "adapter": ["claude"],
                "dry_run": true
            }),
            project.path(),
            cache.path(),
        )
        .unwrap();

        assert!(!project.path().join(MANIFEST_FILE).exists());
    }
}
