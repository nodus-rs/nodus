use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use clap::ValueEnum;
use serde_json::Value as JsonValue;

use super::tools::*;
use crate::adapters::Adapter;
use crate::git::{
    AddDependencyOptions, add_dependency_in_dir_with_adapters, remove_dependency_in_dir,
};
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
    let package = args
        .get("package")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: package"))?;

    let tag = args.get("tag").and_then(|v| v.as_str());
    let branch = args.get("branch").and_then(|v| v.as_str());
    let version = args.get("version").and_then(|v| v.as_str());

    let git_ref = match (tag, branch) {
        (Some(t), _) => Some(RequestedGitRef::Tag(t)),
        (_, Some(b)) => Some(RequestedGitRef::Branch(b)),
        _ => None,
    };

    let version_req = version.map(semver::VersionReq::parse).transpose()?;

    let adapters = parse_string_array::<Adapter>(args, "adapter")?;
    let components = parse_string_array::<DependencyComponent>(args, "component")?;

    capture_output(|reporter| {
        add_dependency_in_dir_with_adapters(
            cwd,
            cache_root,
            package,
            AddDependencyOptions {
                git_ref,
                version_req,
                kind: DependencyKind::Dependency,
                adapters: &adapters,
                components: &components,
                sync_on_launch: false,
                accept_all_dependencies: false,
            },
            reporter,
        )?;
        Ok(())
    })
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
