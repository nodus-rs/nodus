# Nodus MCP Server Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an MCP server to the nodus CLI (`nodus mcp serve`) that exposes package management, relay, and project info as MCP tools over stdio, and auto-registers itself in managed configs during sync.

**Architecture:** The MCP server is a thin bridge layer. Each MCP tool delegates to the existing handler functions that power the CLI. `rust-mcp-sdk` handles the MCP protocol (JSON-RPC, capabilities, tool schemas). During `nodus sync`, the adapter output pipeline injects a `"nodus"` server entry into `.mcp.json`, `.codex/config.toml`, and `opencode.json`.

**Tech Stack:** Rust, rust-mcp-sdk 0.9 (server + stdio + macros), tokio, serde_json

---

## File Structure

| File | Responsibility |
|------|----------------|
| `Cargo.toml` | Add `rust-mcp-sdk` dependency |
| `src/lib.rs` | Register `mod mcp` |
| `src/mcp.rs` | Module root — re-exports, `start_server` entry point |
| `src/mcp/tools.rs` | Tool name constants and JSON schema definitions |
| `src/mcp/handlers.rs` | Tool dispatch — routes tool name to handler, calls existing CLI logic, returns results |
| `src/cli/args.rs` | Add `Command::Mcp { command: McpCommand }` variant |
| `src/cli/help.rs` | Add `MCP_SERVE_ABOUT` help text |
| `src/cli/router.rs` | Route `Command::Mcp` to handler |
| `src/cli/handlers/mod.rs` | Add `pub(super) mod mcp;` |
| `src/cli/handlers/mcp.rs` | **NEW** — CLI handler for `nodus mcp serve` |
| `src/adapters/output.rs` | Inject nodus MCP server entry in `mcp_config_file`, `codex_mcp_config_file`, `opencode_mcp_config_file` |

---

## Task 1: Add rust-mcp-sdk dependency and create empty MCP module

**Files:**
- Modify: `Cargo.toml`
- Create: `src/mcp.rs`
- Create: `src/mcp/tools.rs`
- Create: `src/mcp/handlers.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Add rust-mcp-sdk to Cargo.toml**

In the platform-gated dependencies section, add:

```toml
rust-mcp-sdk = { version = "0.9", default-features = false, features = ["server", "stdio", "macros"] }
```

So the section becomes:

```toml
[target.'cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))'.dependencies]
async-trait = "0.1.89"
mentra = "0.5.0"
notify = "8"
rust-mcp-sdk = { version = "0.9", default-features = false, features = ["server", "stdio", "macros"] }
tokio = { version = "1.50.0", features = ["macros", "rt-multi-thread", "time", "sync", "signal"] }
```

- [ ] **Step 2: Create src/mcp/tools.rs**

```rust
pub const TOOL_ADD: &str = "nodus_add";
pub const TOOL_REMOVE: &str = "nodus_remove";
pub const TOOL_SYNC: &str = "nodus_sync";
pub const TOOL_LIST: &str = "nodus_list";
pub const TOOL_RELAY: &str = "nodus_relay";
pub const TOOL_RELAY_STATUS: &str = "nodus_relay_status";
pub const TOOL_INFO: &str = "nodus_info";
pub const TOOL_CHECK_UPDATES: &str = "nodus_check_updates";
```

- [ ] **Step 3: Create src/mcp/handlers.rs**

```rust
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
```

- [ ] **Step 4: Create src/mcp.rs**

```rust
mod handlers;
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
```

- [ ] **Step 5: Register the module in lib.rs**

Add `pub(crate) mod mcp;` after the `manifest` line in `src/lib.rs`:

```rust
pub(crate) mod manifest;
pub(crate) mod mcp;
```

- [ ] **Step 6: Verify it compiles**

Run: `cargo check`
Expected: compiles (with dead code warnings for the unused module — expected).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/mcp.rs src/mcp/tools.rs src/mcp/handlers.rs
git commit -m "feat(mcp): add rust-mcp-sdk dependency and create empty MCP module"
```

---

## Task 2: Add CLI subcommand `nodus mcp serve`

**Files:**
- Modify: `src/cli/args.rs`
- Modify: `src/cli/help.rs`
- Modify: `src/cli/router.rs`
- Modify: `src/cli/handlers/mod.rs`
- Create: `src/cli/handlers/mcp.rs`

- [ ] **Step 1: Add help text in help.rs**

Add at the end of `src/cli/help.rs`:

```rust
pub(super) const MCP_ABOUT: &str = "MCP server for AI tool integration";

pub(super) const MCP_LONG_ABOUT: &str = r#"Model Context Protocol (MCP) integration.

Exposes nodus operations as MCP tools so AI agents can manage packages, relay edits, and inspect project state."#;

pub(super) const MCP_SERVE_ABOUT: &str = "Start the MCP server on stdio";

pub(super) const MCP_SERVE_LONG_ABOUT: &str = r#"Start a Model Context Protocol server that communicates via stdin/stdout.

AI tools like Claude, Cursor, and Codex connect to this server to access nodus operations as MCP tools. The server runs until the client disconnects or the process is terminated.

Example MCP config entry:
  {
    "nodus": {
      "command": "nodus",
      "args": ["mcp", "serve"]
    }
  }"#;
```

- [ ] **Step 2: Add Command::Mcp variant in args.rs**

Add the imports for the new help constants at the top of `src/cli/args.rs` (in the existing `use crate::cli::help::{...}` block):

```rust
    MCP_ABOUT, MCP_LONG_ABOUT, MCP_SERVE_ABOUT, MCP_SERVE_LONG_ABOUT,
```

Add the new variant at the end of the `Command` enum (before the closing `}`):

```rust
    #[command(
        about = MCP_ABOUT,
        long_about = MCP_LONG_ABOUT,
    )]
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
```

Add the `McpCommand` enum after the `Command` enum:

```rust
#[derive(Debug, Subcommand)]
pub(super) enum McpCommand {
    #[command(
        about = MCP_SERVE_ABOUT,
        long_about = MCP_SERVE_LONG_ABOUT,
    )]
    Serve,
}
```

- [ ] **Step 3: Create src/cli/handlers/mcp.rs**

```rust
use anyhow::Context;

use crate::cli::handlers::CommandContext;

pub(crate) fn handle_mcp_serve(context: &CommandContext<'_>) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()
        .context("failed to create async runtime for MCP server")?;
    rt.block_on(async {
        context
            .reporter
            .note("nodus MCP server starting on stdio")?;
        // TODO: Wire up rust-mcp-sdk server in Task 3
        Ok(())
    })
}
```

- [ ] **Step 4: Register the handler module in mod.rs**

In `src/cli/handlers/mod.rs`, add:

```rust
pub(super) mod mcp;
```

- [ ] **Step 5: Route the command in router.rs**

In `src/cli/router.rs`, add the import for `mcp` handlers in the `use` statement at line 4:

```rust
use super::handlers::{CommandContext, dependency, mcp, project, query, system};
```

Add the match arm before the closing `}` of the `match command` block:

```rust
        Command::Mcp { command } => match command {
            crate::cli::args::McpCommand::Serve => mcp::handle_mcp_serve(&context),
        },
```

Add the import for `McpCommand` — you'll need to import it either via the match pattern or add it to the existing imports.

- [ ] **Step 6: Verify it compiles and the command is reachable**

Run: `cargo check`
Then: `cargo run -- mcp serve --help`
Expected: shows the MCP serve help text.

- [ ] **Step 7: Commit**

```bash
git add src/cli/args.rs src/cli/help.rs src/cli/router.rs src/cli/handlers/mod.rs src/cli/handlers/mcp.rs
git commit -m "feat(mcp): add nodus mcp serve CLI subcommand"
```

---

## Task 3: Implement MCP server startup with rust-mcp-sdk

**Files:**
- Modify: `src/mcp.rs`
- Modify: `src/cli/handlers/mcp.rs`

This task wires up `rust-mcp-sdk` to create a working MCP server that responds to `tools/list` but doesn't implement any tool handlers yet.

- [ ] **Step 1: Read the rust-mcp-sdk documentation**

Run: `cargo doc --open -p rust-mcp-sdk` or read the crate docs to understand the server API. The key types are:
- `ServerHandler` trait — implement this to handle MCP requests
- `SdkRunner` or similar — the main server runner
- `StdioTransport` — stdio JSON-RPC transport

Examine the actual API before writing code — the crate's exact API may differ from what the plan anticipates. The goal is:
1. Create a struct implementing `ServerHandler`
2. Register 8 tools with names and descriptions from `src/mcp/tools.rs`
3. Start the server on stdio
4. Return tool results from the `call_tool` handler

- [ ] **Step 2: Update src/mcp.rs with server startup**

Based on the actual `rust-mcp-sdk` API discovered in step 1, implement `start_server` as an async function:

```rust
/// Start the MCP server on stdio. Blocks until the client disconnects.
pub async fn start_server(cwd: PathBuf, cache_root: PathBuf) -> Result<()> {
    // 1. Create the server handler (a struct implementing ServerHandler)
    // 2. Register all 8 tools with their names, descriptions, and JSON schemas
    // 3. Create a StdioTransport
    // 4. Run the server
}
```

The server handler struct should hold `cwd: PathBuf` and `cache_root: PathBuf` so tool handlers can access the project context.

The `call_tool` method in the handler should delegate to `handlers::dispatch_tool()`.

- [ ] **Step 3: Update src/cli/handlers/mcp.rs to call start_server**

```rust
use anyhow::Context;

use crate::cli::handlers::CommandContext;

pub(crate) fn handle_mcp_serve(context: &CommandContext<'_>) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()
        .context("failed to create async runtime for MCP server")?;
    let cwd = context.cwd.to_path_buf();
    let cache_root = context.cache_root.to_path_buf();
    rt.block_on(crate::mcp::start_server(cwd, cache_root))
}
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check`
Expected: compiles with no errors.

- [ ] **Step 5: Smoke test**

Run: `echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}' | cargo run -- mcp serve`

Expected: a JSON-RPC response with server info and capabilities including the 8 tools.

Note: The exact smoke test may vary depending on the rust-mcp-sdk API. Adjust based on what you discover in step 1.

- [ ] **Step 6: Commit**

```bash
git add src/mcp.rs src/mcp/tools.rs src/cli/handlers/mcp.rs
git commit -m "feat(mcp): implement MCP server startup with rust-mcp-sdk"
```

---

## Task 4: Implement nodus_list and nodus_info tool handlers

**Files:**
- Modify: `src/mcp/handlers.rs`
- Modify: `src/mcp/tools.rs`

These are the simplest tools — read-only, with existing JSON output functions.

- [ ] **Step 1: Add JSON input schemas to tools.rs**

Add schema definitions for each tool. These are used by the MCP protocol to tell clients what parameters each tool accepts:

```rust
use serde_json::{json, Value as JsonValue};

pub const TOOL_ADD: &str = "nodus_add";
pub const TOOL_REMOVE: &str = "nodus_remove";
pub const TOOL_SYNC: &str = "nodus_sync";
pub const TOOL_LIST: &str = "nodus_list";
pub const TOOL_RELAY: &str = "nodus_relay";
pub const TOOL_RELAY_STATUS: &str = "nodus_relay_status";
pub const TOOL_INFO: &str = "nodus_info";
pub const TOOL_CHECK_UPDATES: &str = "nodus_check_updates";

pub fn list_input_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {},
        "required": []
    })
}

pub fn info_input_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "package": {
                "type": "string",
                "description": "Dependency alias, local path, Git URL, or GitHub shortcut. Defaults to current project."
            }
        },
        "required": []
    })
}
```

- [ ] **Step 2: Implement list and info handlers**

In `src/mcp/handlers.rs`:

```rust
use std::path::Path;

use anyhow::{Result, bail};
use serde_json::Value as JsonValue;

use super::tools::*;

pub fn dispatch_tool(
    tool_name: &str,
    args: &JsonValue,
    cwd: &Path,
    cache_root: &Path,
) -> Result<String> {
    match tool_name {
        TOOL_LIST => handle_list(cwd),
        TOOL_INFO => handle_info(args, cwd, cache_root),
        _ => bail!("unknown tool: {tool_name}"),
    }
}

fn handle_list(cwd: &Path) -> Result<String> {
    let list = crate::list::list_dependencies_json_in_dir(cwd)?;
    Ok(serde_json::to_string_pretty(&list)?)
}

fn handle_info(args: &JsonValue, cwd: &Path, cache_root: &Path) -> Result<String> {
    let package = args
        .get("package")
        .and_then(|v| v.as_str())
        .unwrap_or(".");
    let info = crate::info::describe_package_json_in_dir(cwd, cache_root, package, None, None)?;
    Ok(serde_json::to_string_pretty(&info)?)
}
```

- [ ] **Step 3: Update mcp.rs dispatch to not pass reporter**

Update `src/mcp.rs` to call `handlers::dispatch_tool` without reporter (handlers that need terminal output will create their own sink reporter):

```rust
pub fn dispatch_tool(
    tool_name: &str,
    args: &serde_json::Value,
    cwd: &Path,
    cache_root: &Path,
) -> Result<String> {
    handlers::dispatch_tool(tool_name, args, cwd, cache_root)
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib list`
Run: `cargo test --lib info`
Expected: existing tests still pass.

- [ ] **Step 5: Commit**

```bash
git add src/mcp/handlers.rs src/mcp/tools.rs src/mcp.rs
git commit -m "feat(mcp): implement nodus_list and nodus_info tool handlers"
```

---

## Task 5: Implement nodus_sync, nodus_add, and nodus_remove tool handlers

**Files:**
- Modify: `src/mcp/handlers.rs`
- Modify: `src/mcp/tools.rs`

These are the write operations — they modify project state.

- [ ] **Step 1: Add input schemas to tools.rs**

```rust
pub fn sync_input_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {},
        "required": []
    })
}

pub fn add_input_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "package": {
                "type": "string",
                "description": "Git URL, local path, or GitHub shortcut like owner/repo"
            },
            "tag": {
                "type": "string",
                "description": "Pin a specific Git tag"
            },
            "branch": {
                "type": "string",
                "description": "Track a specific Git branch"
            },
            "version": {
                "type": "string",
                "description": "Semver version requirement like ^1.2.0"
            },
            "adapter": {
                "type": "array",
                "items": { "type": "string", "enum": ["agents", "claude", "codex", "copilot", "cursor", "opencode"] },
                "description": "Adapters to enable"
            },
            "component": {
                "type": "array",
                "items": { "type": "string", "enum": ["skills", "agents", "rules", "commands"] },
                "description": "Components to install"
            }
        },
        "required": ["package"]
    })
}

pub fn remove_input_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "package": {
                "type": "string",
                "description": "Dependency alias or repository reference to remove"
            }
        },
        "required": ["package"]
    })
}
```

- [ ] **Step 2: Implement sync, add, and remove handlers**

Add to `src/mcp/handlers.rs`:

```rust
use crate::report::{ColorMode, Reporter};

// Add to the match in dispatch_tool:
//     TOOL_SYNC => handle_sync(cwd, cache_root),
//     TOOL_ADD => handle_add(args, cwd, cache_root),
//     TOOL_REMOVE => handle_remove(args, cwd, cache_root),

fn handle_sync(cwd: &Path, cache_root: &Path) -> Result<String> {
    let output = capture_output(|reporter| {
        crate::resolver::sync_in_dir_with_adapters(cwd, cache_root, &[], false, reporter)
    })?;
    Ok(output)
}

fn handle_add(args: &JsonValue, cwd: &Path, cache_root: &Path) -> Result<String> {
    let package = args
        .get("package")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: package"))?;
    let tag = args.get("tag").and_then(|v| v.as_str());
    let branch = args.get("branch").and_then(|v| v.as_str());
    let version = args.get("version").and_then(|v| v.as_str());
    let adapters = parse_adapter_array(args.get("adapter"));
    let components = parse_component_array(args.get("component"));

    let git_ref = if let Some(tag) = tag {
        Some(crate::manifest::RequestedGitRef::Tag(tag.to_string()))
    } else if let Some(branch) = branch {
        Some(crate::manifest::RequestedGitRef::Branch(branch.to_string()))
    } else {
        None
    };

    let version_req = version.map(|v| v.to_string());

    let output = capture_output(|reporter| {
        crate::resolver::add_dependency_in_dir_with_adapters(
            cwd,
            cache_root,
            package,
            crate::resolver::AddDependencyOptions {
                git_ref,
                version_req,
                kind: crate::manifest::DependencyKind::Dependency,
                adapters: &adapters,
                components: &components,
                sync_on_launch: false,
                accept_all_dependencies: false,
            },
            reporter,
        )
    })?;
    Ok(output)
}

fn handle_remove(args: &JsonValue, cwd: &Path, cache_root: &Path) -> Result<String> {
    let package = args
        .get("package")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: package"))?;

    let output = capture_output(|reporter| {
        crate::resolver::remove_dependency_in_dir(cwd, cache_root, package, false, reporter)
    })?;
    Ok(output)
}

/// Run a function that writes to a Reporter and capture its output as a String.
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
struct SharedOutputBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl SharedOutputBuffer {
    fn into_string(self) -> String {
        let bytes = self.0.lock().unwrap().clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl std::io::Write for SharedOutputBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn parse_adapter_array(value: Option<&JsonValue>) -> Vec<crate::adapters::Adapter> {
    value
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter_map(|s| s.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn parse_component_array(value: Option<&JsonValue>) -> Vec<crate::manifest::DependencyComponent> {
    value
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter_map(|s| s.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}
```

Note: Check that `Adapter` and `DependencyComponent` implement `FromStr` (they likely do since they use `value_enum` with clap). If not, implement parsing manually.

Also check the exact signature of `sync_in_dir_with_adapters` and `add_dependency_in_dir_with_adapters` and `remove_dependency_in_dir` — the parameter names and types above are based on exploration but may need adjustment. Read the actual function signatures before implementing.

- [ ] **Step 3: Verify it compiles**

Run: `cargo check`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add src/mcp/handlers.rs src/mcp/tools.rs
git commit -m "feat(mcp): implement nodus_sync, nodus_add, and nodus_remove tool handlers"
```

---

## Task 6: Implement nodus_relay, nodus_relay_status, and nodus_check_updates tool handlers

**Files:**
- Modify: `src/mcp/handlers.rs`
- Modify: `src/mcp/tools.rs`

- [ ] **Step 1: Add input schemas to tools.rs**

```rust
pub fn relay_input_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "package": {
                "type": "string",
                "description": "Dependency alias to relay. If omitted, relays all linked dependencies."
            }
        },
        "required": []
    })
}

pub fn relay_status_input_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "package": {
                "type": "string",
                "description": "Dependency alias to check. If omitted, checks all linked dependencies."
            }
        },
        "required": []
    })
}

pub fn check_updates_input_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {},
        "required": []
    })
}
```

- [ ] **Step 2: Implement relay, relay_status, and check_updates handlers**

Add to `src/mcp/handlers.rs` (and add to the match in `dispatch_tool`):

```rust
//     TOOL_RELAY => handle_relay(args, cwd, cache_root),
//     TOOL_RELAY_STATUS => handle_relay_status(args, cwd, cache_root),
//     TOOL_CHECK_UPDATES => handle_check_updates(cwd, cache_root),

fn handle_relay(args: &JsonValue, cwd: &Path, cache_root: &Path) -> Result<String> {
    let packages = if let Some(package) = args.get("package").and_then(|v| v.as_str()) {
        vec![package.to_string()]
    } else {
        // Get all linked packages from local config
        let config = crate::local_config::LocalConfig::load_in_dir(cwd)?;
        config.relay.keys().cloned().collect()
    };

    if packages.is_empty() {
        return Ok("no linked dependencies to relay".to_string());
    }

    let output = capture_output(|reporter| {
        let summaries = crate::relay::relay_dependencies_in_dir(
            cwd,
            cache_root,
            &packages,
            None,
            None,
            false,
            reporter,
        )?;
        let created: usize = summaries.iter().map(|s| s.created_file_count).sum();
        let updated: usize = summaries.iter().map(|s| s.updated_file_count).sum();
        reporter.finish(format!(
            "relayed {} dependencies; created {} and updated {} source files",
            summaries.len(),
            created,
            updated,
        ))?;
        Ok(())
    })?;
    Ok(output)
}

fn handle_relay_status(args: &JsonValue, cwd: &Path, cache_root: &Path) -> Result<String> {
    let packages = if let Some(package) = args.get("package").and_then(|v| v.as_str()) {
        vec![package.to_string()]
    } else {
        let config = crate::local_config::LocalConfig::load_in_dir(cwd)?;
        config.relay.keys().cloned().collect()
    };

    if packages.is_empty() {
        return Ok("no linked dependencies".to_string());
    }

    let output = capture_output(|reporter| {
        let summaries = crate::relay::relay_dependencies_in_dir_dry_run(
            cwd,
            cache_root,
            &packages,
            None,
            None,
            false,
            reporter,
        )?;
        for summary in &summaries {
            reporter.finish(format!(
                "{}: {} files to create, {} files to update",
                summary.alias, summary.created_file_count, summary.updated_file_count,
            ))?;
        }
        Ok(())
    })?;
    Ok(output)
}

fn handle_check_updates(cwd: &Path, cache_root: &Path) -> Result<String> {
    let result = crate::outdated::check_outdated_json_in_dir(cwd, cache_root)?;
    Ok(serde_json::to_string_pretty(&result)?)
}
```

Note: Check the exact signature of `relay_dependencies_in_dir` and `relay_dependencies_in_dir_dry_run`. The current signature takes `(project_root, cache_root, packages, repo_path_override, via_override, create_missing, reporter)`. Adjust as needed.

- [ ] **Step 3: Verify all 8 tools are wired up**

The `dispatch_tool` match should now have all 8 arms:

```rust
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
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check`
Expected: compiles.

- [ ] **Step 5: Commit**

```bash
git add src/mcp/handlers.rs src/mcp/tools.rs
git commit -m "feat(mcp): implement relay, relay_status, and check_updates tool handlers"
```

---

## Task 7: Auto-register nodus MCP server in managed configs

**Files:**
- Modify: `src/adapters/output.rs`

During `nodus sync`, inject a `"nodus"` server entry into `.mcp.json`, `.codex/config.toml`, and `opencode.json`.

- [ ] **Step 1: Read the current mcp_config_file, codex_mcp_config_file, and opencode_mcp_config_file functions**

Read `src/adapters/output.rs` to understand the exact insertion points.

- [ ] **Step 2: Inject nodus server into mcp_config_file**

In the `mcp_config_file` function (around line 628), after the loop that builds `desired_servers` from packages (after line 653), add:

```rust
    // Auto-register the nodus MCP server itself.
    desired_servers.insert(
        "nodus".to_string(),
        EmittedMcpServerConfig {
            transport_type: None,
            command: Some("nodus".to_string()),
            url: None,
            args: vec!["mcp".to_string(), "serve".to_string()],
            env: BTreeMap::new(),
            headers: BTreeMap::new(),
            cwd: None,
        },
    );
```

- [ ] **Step 3: Inject nodus server into codex_mcp_config_file**

Find the `codex_mcp_config_file` function and apply the same pattern. Read the Codex MCP config structure first — it uses a different format (`CodxMcpConfig` / `CodxMcpServer`). Add the nodus entry after the package loop builds the desired servers.

The Codex format for a command-based server looks like:

```toml
[mcp_servers.nodus]
command = "nodus"
args = ["mcp", "serve"]
```

Insert using whatever struct the Codex config uses.

- [ ] **Step 4: Inject nodus server into opencode_mcp_config_file**

Find the `opencode_mcp_config_file` function and apply the same pattern. OpenCode uses a JSON format with `"type": "local"` for command-based servers. Add the nodus entry after the package loop.

- [ ] **Step 5: Write an auto-registration test**

Add a test (in the existing test module in `src/adapters/output.rs` or in a new test):

```rust
#[test]
fn mcp_config_includes_nodus_server_entry() {
    // Create a minimal project fixture
    // Call mcp_config_file with empty packages
    // Assert the result contains a "nodus" key with command "nodus" and args ["mcp", "serve"]
}
```

Read the existing test patterns in `output.rs` to match the style.

- [ ] **Step 6: Run tests**

Run: `cargo test --lib adapters`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/adapters/output.rs
git commit -m "feat(mcp): auto-register nodus MCP server in managed configs during sync"
```

---

## Task 8: Integration tests for MCP server

**Files:**
- Modify: `src/cli/tests.rs` or create integration test

- [ ] **Step 1: Write a CLI test for `nodus mcp serve --help`**

Add to `src/cli/tests.rs` following the existing test patterns:

```rust
#[test]
fn mcp_serve_help_shows_description() {
    let output = run_help_output(&["mcp", "serve", "--help"]);
    assert!(output.contains("Start the MCP server on stdio"));
}
```

Check the existing test helpers (`run_help_output` or similar) and follow the pattern.

- [ ] **Step 2: Write a test for auto-registration in sync output**

```rust
#[test]
fn sync_emits_nodus_mcp_server_in_config() {
    // Set up a project fixture with a nodus.toml and at least one dependency
    // Run sync
    // Read .mcp.json
    // Assert it contains the "nodus" server entry
}
```

Follow existing sync test patterns in `src/cli/tests.rs`.

- [ ] **Step 3: Run full test suite**

Run: `cargo test`
Expected: ALL PASS.

- [ ] **Step 4: Run clippy**

Run: `cargo clippy -- -D warnings`
Expected: no warnings.

- [ ] **Step 5: Run cargo fmt**

Run: `cargo fmt`
Expected: no changes (or commit if needed).

- [ ] **Step 6: Commit**

```bash
git add src/cli/tests.rs
git commit -m "test(mcp): add integration tests for MCP server and auto-registration"
```
