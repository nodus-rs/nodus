# Nodus MCP Server Design

## Summary

Add an MCP (Model Context Protocol) server to the nodus CLI, exposing package management, relay, and project info operations as MCP tools over stdio. The server auto-registers itself in managed configs during `nodus sync` so AI tools discover and connect to it automatically.

## Motivation

AI tools (Claude, Cursor, Codex, OpenCode) can interact with nodus through MCP, enabling them to manage dependencies, relay edits, and inspect project state directly — without the user switching to a terminal.

## CLI Interface

New subcommand:

```
nodus mcp serve
```

Starts the MCP server on stdio (stdin/stdout JSON-RPC). Runs until the client disconnects or the process is terminated.

Implemented as `Command::Mcp` with a nested enum for future MCP subcommands (e.g., `nodus mcp test`).

## Transport

Stdio only (JSON-RPC over stdin/stdout). Uses `rust-mcp-sdk` with feature flags trimmed to `server` + `stdio` (no HTTP/SSE/auth). Runs on a tokio async runtime.

## MCP Tool Surface

Eight tools, each a thin adapter over existing CLI handler logic:

| Tool | Description | Parameters | Returns |
|------|-------------|------------|---------|
| `nodus_add` | Add a dependency | `package` (required), `global`, `dev`, `tag`, `branch`, `version`, `revision`, `adapter[]`, `component[]`, `exclude_component[]`, `sync_on_launch`, `accept_all_dependencies`, `dry_run` | Success message + installed package info |
| `nodus_remove` | Remove a dependency | `package` (required) | Success message |
| `nodus_sync` | Sync all dependencies | (none) | Summary of synced packages |
| `nodus_list` | List installed packages | (none) | JSON array of packages with name, version, source, adapters |
| `nodus_relay` | Relay managed edits to linked source | `package` (optional, defaults to all) | Summary of relayed files |
| `nodus_relay_status` | Show pending relay edits and conflicts | `package` (optional, defaults to all) | JSON with pending edits, conflicts per package |
| `nodus_info` | Show project or package info | `package` (optional, defaults to project) | JSON with manifest, packages, adapters, MCP servers |
| `nodus_check_updates` | Check for available updates | (none) | JSON array of packages with current vs latest version |

No business logic lives in the MCP layer — it's purely a bridge to existing handler functions.

## Architecture and Module Structure

### New files

| File | Responsibility |
|------|----------------|
| `src/mcp.rs` | Module root, MCP server setup and initialization |
| `src/mcp/tools.rs` | Tool definitions (name, description, input JSON schema) |
| `src/mcp/handlers.rs` | Tool dispatch — maps tool name to handler, calls existing CLI logic |
| `src/cli/args.rs` | New `Command::Mcp` variant |
| `src/cli/handlers/mcp.rs` | CLI handler for `nodus mcp serve` — starts the MCP server |

### Integration with existing code

MCP handlers call the same functions that CLI handlers call. For example, `nodus_list` calls the same `list_in_dir` function that `nodus list --json` uses.

**Reporter adaptation:** MCP handlers use `Reporter::sink()` (already exists) to capture terminal output into a buffer, then return the buffer content as the MCP tool result text. For tools that already support JSON output (`list`, `info`), the JSON path is used directly.

### Auto-registration

During `nodus sync`, nodus injects its own MCP server entry into managed configs alongside package-declared servers:

```json
{
  "nodus": {
    "command": "nodus",
    "args": ["mcp", "serve"]
  }
}
```

This entry is emitted to `.mcp.json`, `.codex/config.toml`, and `opencode.json` using the existing adapter output pipeline. The server name is `"nodus"` (no package alias prefix, since it's the tool itself).

The injection happens in the existing `emit_managed_mcp_config` function in `src/adapters/output.rs` — a small addition, not a new code path.

## Dependencies

Add to `Cargo.toml`:

```toml
[target.'cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))'.dependencies]
rust-mcp-sdk = { version = "0.9", default-features = false, features = ["server", "stdio", "macros"] }
```

Platform-gated like other async dependencies (tokio, mentra, notify).

## Error Handling

- Tool failures return MCP error responses with the `anyhow` error message as the text content. `rust-mcp-sdk` handles JSON-RPC error wrapping.
- Missing `nodus.toml` produces a clear error: "no nodus.toml found in current directory".
- Network errors (git fetch failures) propagate through existing error handling.

## Testing

- **Unit tests** for each tool handler: call the handler with known inputs against a tempdir fixture, assert the JSON result structure.
- **Integration tests** for MCP protocol round-trip: spawn `nodus mcp serve` as a subprocess, send JSON-RPC tool calls via stdin, assert JSON-RPC responses on stdout.
- **Auto-registration test:** Run `nodus sync` on a fixture project, verify the emitted `.mcp.json` contains the `"nodus"` server entry.

## Files Modified

| File | Change |
|------|--------|
| `Cargo.toml` | Add `rust-mcp-sdk` dependency |
| `src/lib.rs` | Register `mod mcp` |
| `src/mcp.rs` | **NEW** — MCP server setup and initialization |
| `src/mcp/tools.rs` | **NEW** — Tool definitions and schemas |
| `src/mcp/handlers.rs` | **NEW** — Tool dispatch bridging to CLI handlers |
| `src/cli/args.rs` | Add `Command::Mcp` variant |
| `src/cli/help.rs` | Add MCP help text |
| `src/cli/router.rs` | Route `Command::Mcp` to handler |
| `src/cli/handlers/mcp.rs` | **NEW** — CLI handler for `nodus mcp serve` |
| `src/adapters/output.rs` | Inject nodus MCP server entry during config emission |
