use anyhow::Context;

use crate::cli::handlers::CommandContext;

pub(crate) fn handle_mcp_serve(context: &CommandContext<'_>) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()
        .context("failed to create async runtime for MCP server")?;
    let cwd = context.cwd.to_path_buf();
    let cache_root = context.cache_root.to_path_buf();
    rt.block_on(crate::mcp::start_server(cwd, cache_root))
}
