use std::path::Path;

use anyhow::Result;
use serde_json::Value as JsonValue;

pub fn dispatch_tool(
    tool_name: &str,
    _args: &JsonValue,
    _cwd: &Path,
    _cache_root: &Path,
) -> Result<String> {
    anyhow::bail!("unknown tool: {tool_name}")
}
