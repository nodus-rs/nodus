use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use rust_mcp_sdk::mcp_server::{McpServerOptions, ServerHandler, ServerRuntime, server_runtime};
use rust_mcp_sdk::schema::{
    CallToolRequestParams, CallToolResult, Implementation, InitializeResult, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, RpcError, ServerCapabilities, ServerCapabilitiesTools,
    TextContent, Tool, ToolInputSchema, schema_utils::CallToolError,
};
use rust_mcp_sdk::{McpServer, StdioTransport, ToMcpServerHandler, TransportOptions};

use super::handlers;

/// MCP server handler that delegates tool calls to nodus operations.
struct NodusHandler {
    cwd: PathBuf,
    cache_root: PathBuf,
    tools: Vec<Tool>,
}

impl NodusHandler {
    fn new(cwd: PathBuf, cache_root: PathBuf) -> Self {
        let tools = super::tool_definitions()
            .into_iter()
            .map(|(name, description, schema)| {
                let (required, properties) = extract_schema_parts(&schema);
                Tool {
                    name: name.to_string(),
                    description: Some(description.to_string()),
                    input_schema: ToolInputSchema::new(required, properties, None),
                    annotations: None,
                    execution: None,
                    icons: vec![],
                    meta: None,
                    output_schema: None,
                    title: None,
                }
            })
            .collect();

        Self {
            cwd,
            cache_root,
            tools,
        }
    }
}

#[async_trait]
impl ServerHandler for NodusHandler {
    async fn handle_list_tools_request(
        &self,
        _params: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<ListToolsResult, RpcError> {
        Ok(ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: self.tools.clone(),
        })
    }

    async fn handle_call_tool_request(
        &self,
        params: CallToolRequestParams,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<CallToolResult, CallToolError> {
        let args_value = params
            .arguments
            .map(serde_json::Value::Object)
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

        let result =
            handlers::dispatch_tool(&params.name, &args_value, &self.cwd, &self.cache_root)
                .map_err(|err| CallToolError::from_message(err.to_string()))?;

        Ok(CallToolResult::text_content(vec![TextContent::from(
            result,
        )]))
    }
}

type SchemaProperties =
    Option<std::collections::BTreeMap<String, serde_json::Map<String, serde_json::Value>>>;

/// Extract `required` and `properties` from a JSON schema value into the types
/// expected by `ToolInputSchema::new`.
fn extract_schema_parts(schema: &serde_json::Value) -> (Vec<String>, SchemaProperties) {
    let required = schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let properties = schema
        .get("properties")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(key, val)| {
                    let prop_map = val.as_object().cloned().unwrap_or_default();
                    (key.clone(), prop_map)
                })
                .collect()
        });

    (required, properties)
}

/// Build the server info advertised during MCP initialization.
fn server_info() -> InitializeResult {
    InitializeResult {
        server_info: Implementation {
            name: "nodus".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: Some("nodus MCP server".into()),
            description: Some("Local-first CLI for managing project-scoped agent packages.".into()),
            icons: vec![],
            website_url: None,
        },
        capabilities: ServerCapabilities {
            tools: Some(ServerCapabilitiesTools { list_changed: None }),
            ..Default::default()
        },
        meta: None,
        instructions: Some(
            "Use the nodus_* tools to manage project dependencies for agent packages.".into(),
        ),
        protocol_version: ProtocolVersion::V2025_11_25.into(),
    }
}

/// Start the MCP server on stdio and block until the client disconnects.
pub async fn run(cwd: PathBuf, cache_root: PathBuf) -> Result<()> {
    let transport = StdioTransport::new(TransportOptions::default())
        .map_err(|err| anyhow::anyhow!("failed to create transport: {err}"))?;

    let handler = NodusHandler::new(cwd, cache_root);

    let server: Arc<ServerRuntime> = server_runtime::create_server(McpServerOptions {
        server_details: server_info(),
        transport,
        handler: handler.to_mcp_server_handler(),
        task_store: None,
        client_task_store: None,
        message_observer: None,
    });

    server.start().await.map_err(|err| {
        let message = err
            .rpc_error_message()
            .map(String::from)
            .unwrap_or_else(|| err.to_string());
        anyhow::anyhow!("MCP server error: {}", message)
    })
}
