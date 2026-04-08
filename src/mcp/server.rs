use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use anyhow::Result;
use rust_mcp_sdk::mcp_server::enforce_compatible_protocol_version;
use rust_mcp_sdk::schema::schema_utils::{
    CallToolError, ClientJsonrpcNotification, ClientJsonrpcRequest, ClientMessage, ClientMessages,
    FromMessage, MessageFromServer, ResultFromServer, ServerMessage, ServerMessages,
};
use rust_mcp_sdk::schema::{
    CallToolRequestParams, CallToolResult, Implementation, InitializeResult, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, RequestId, Result as EmptyResult, RpcError,
    ServerCapabilities, ServerCapabilitiesTools, TextContent, Tool, ToolInputSchema,
};

use super::handlers;

/// Minimal MCP stdio server for local agent clients.
///
/// We intentionally own the stdio loop here instead of delegating to the upstream
/// SDK runtime because the SDK server transport fails to answer the initial
/// `initialize` request in our CLI integration test.
struct NodusServer {
    cwd: PathBuf,
    cache_root: PathBuf,
    initialized: bool,
    tools: Vec<Tool>,
    trace: bool,
}

impl NodusServer {
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
            initialized: false,
            tools,
            trace: std::env::var_os("NODUS_MCP_TRACE").is_some(),
        }
    }

    fn run(&mut self) -> Result<()> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut reader = stdin.lock();
        let mut writer = stdout.lock();
        let mut line = String::new();

        loop {
            line.clear();
            let read = reader.read_line(&mut line)?;
            if read == 0 {
                return Ok(());
            }

            let payload = line.trim();
            if payload.is_empty() {
                continue;
            }

            let responses = match serde_json::from_str::<ClientMessages>(payload) {
                Ok(messages) => {
                    if self.trace {
                        eprintln!("[nodus-mcp receive] {messages:?}");
                    }
                    self.handle_messages(messages)?
                }
                Err(error) => Some(ServerMessages::Single(self.error_response(
                    None,
                    RpcError::parse_error().with_message(error.to_string()),
                )?)),
            };

            if let Some(responses) = responses {
                if self.trace {
                    eprintln!("[nodus-mcp send] {responses:?}");
                }
                serde_json::to_writer(&mut writer, &responses)?;
                writer.write_all(b"\n")?;
                writer.flush()?;
            }
        }
    }

    fn handle_messages(&mut self, messages: ClientMessages) -> Result<Option<ServerMessages>> {
        match messages {
            ClientMessages::Single(message) => self
                .handle_message(message)
                .map(|response| response.map(ServerMessages::Single)),
            ClientMessages::Batch(messages) => {
                let mut responses = Vec::new();
                for message in messages {
                    if let Some(response) = self.handle_message(message)? {
                        responses.push(response);
                    }
                }
                if responses.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(ServerMessages::Batch(responses)))
                }
            }
        }
    }

    fn handle_message(&mut self, message: ClientMessage) -> Result<Option<ServerMessage>> {
        match message {
            ClientMessage::Request(request) => self.handle_request(request).map(Some),
            ClientMessage::Notification(notification) => {
                self.handle_notification(notification);
                Ok(None)
            }
            ClientMessage::Response(_) | ClientMessage::Error(_) => Ok(None),
        }
    }

    fn handle_request(&mut self, request: ClientJsonrpcRequest) -> Result<ServerMessage> {
        let request_id = request.request_id().clone();
        match request {
            ClientJsonrpcRequest::InitializeRequest(request) => {
                self.initialize_response(request_id, request.params.protocol_version.as_str())
            }
            ClientJsonrpcRequest::PingRequest(_) => {
                self.success_response(request_id, ResultFromServer::Result(EmptyResult::default()))
            }
            ClientJsonrpcRequest::ListToolsRequest(request) => match self.require_initialized() {
                Ok(()) => {
                    let result = self.list_tools(request.params);
                    self.success_response(request_id, ResultFromServer::ListToolsResult(result))
                }
                Err(error) => self.error_response(Some(request_id), error),
            },
            ClientJsonrpcRequest::CallToolRequest(request) => match self.require_initialized() {
                Ok(()) => {
                    let result = self.call_tool(request.params);
                    self.success_response(request_id, ResultFromServer::CallToolResult(result))
                }
                Err(error) => self.error_response(Some(request_id), error),
            },
            _ => self.error_response(
                Some(request_id),
                RpcError::method_not_found()
                    .with_message(format!("Unsupported MCP method: {}", request.method())),
            ),
        }
    }

    fn handle_notification(&mut self, notification: ClientJsonrpcNotification) {
        if let ClientJsonrpcNotification::InitializedNotification(_) = notification {
            self.initialized = true;
        }
    }

    fn initialize_response(
        &mut self,
        request_id: RequestId,
        client_version: &str,
    ) -> Result<ServerMessage> {
        let mut info = server_info();
        if let Some(updated_protocol_version) =
            enforce_compatible_protocol_version(client_version, &info.protocol_version)
                .map_err(|error| RpcError::internal_error().with_message(error.to_string()))?
        {
            info.protocol_version = updated_protocol_version;
        }
        self.initialized = true;
        self.success_response(request_id, ResultFromServer::InitializeResult(info))
    }

    fn list_tools(&self, _params: Option<PaginatedRequestParams>) -> ListToolsResult {
        ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: self.tools.clone(),
        }
    }

    fn call_tool(&self, params: CallToolRequestParams) -> CallToolResult {
        let args_value = params
            .arguments
            .map(serde_json::Value::Object)
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

        match handlers::dispatch_tool(&params.name, &args_value, &self.cwd, &self.cache_root) {
            Ok(result) => CallToolResult::text_content(vec![TextContent::from(result)]),
            Err(error) => CallToolError::from_message(error.to_string()).into(),
        }
    }

    fn require_initialized(&self) -> std::result::Result<(), RpcError> {
        if self.initialized {
            Ok(())
        } else {
            Err(RpcError::invalid_request()
                .with_message("MCP server has not been initialized yet".to_string()))
        }
    }

    fn success_response(
        &self,
        request_id: RequestId,
        result: ResultFromServer,
    ) -> Result<ServerMessage> {
        Ok(ServerMessage::from_message(
            MessageFromServer::ResultFromServer(result),
            Some(request_id),
        )?)
    }

    fn error_response(
        &self,
        request_id: Option<RequestId>,
        error: RpcError,
    ) -> Result<ServerMessage> {
        Ok(ServerMessage::from_message(
            MessageFromServer::Error(error),
            request_id,
        )?)
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
    let mut server = NodusServer::new(cwd, cache_root);
    server.run()
}
