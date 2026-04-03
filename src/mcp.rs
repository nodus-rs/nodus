mod handlers;
mod server;
mod tools;

pub use tools::*;

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value as JsonValue;

pub fn tool_definitions() -> Vec<(&'static str, &'static str)> {
    vec![
        (TOOL_ADD, "Add a dependency to the project"),
        (TOOL_REMOVE, "Remove a dependency from the project"),
        (TOOL_SYNC, "Sync all dependencies"),
        (TOOL_LIST, "List installed packages"),
        (TOOL_RELAY, "Relay managed edits to linked source repos"),
        (TOOL_RELAY_STATUS, "Show pending relay edits and conflicts"),
        (TOOL_INFO, "Show project or package info"),
        (TOOL_CHECK_UPDATES, "Check for available package updates"),
    ]
}

pub fn dispatch_tool(
    tool_name: &str,
    args: &JsonValue,
    cwd: &Path,
    cache_root: &Path,
) -> Result<String> {
    handlers::dispatch_tool(tool_name, args, cwd, cache_root)
}

pub async fn start_server(cwd: PathBuf, cache_root: PathBuf) -> Result<()> {
    server::run(cwd, cache_root).await
}
