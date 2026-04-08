use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use tempfile::TempDir;

fn initialize_request() -> &'static str {
    r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{"sampling":{},"elicitation":{},"roots":{"listChanged":true}},"clientInfo":{"name":"nodus-test-client","version":"0.1.0"}}}"#
}

fn initialized_notification() -> &'static str {
    r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
}

fn list_tools_request() -> &'static str {
    r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#
}

struct SpawnedServer {
    child: Child,
    stdin: ChildStdin,
    stdout_rx: mpsc::Receiver<std::io::Result<String>>,
    stderr_rx: mpsc::Receiver<String>,
    _workspace: TempDir,
    _store: TempDir,
}

impl SpawnedServer {
    fn spawn() -> Self {
        let workspace = TempDir::new().expect("workspace tempdir");
        let store = TempDir::new().expect("store tempdir");

        let mut command = Command::new(env!("CARGO_BIN_EXE_nodus"));
        command
            .current_dir(workspace.path())
            .arg("--store-path")
            .arg(store.path())
            .arg("mcp")
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if std::env::var_os("NODUS_MCP_TRACE_TEST").is_some() {
            command.env("NODUS_MCP_TRACE", "1");
        }

        let mut child = command.spawn().expect("spawn nodus mcp serve");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        let stderr = child.stderr.take().expect("child stderr");

        let (stdout_tx, stdout_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if stdout_tx.send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = stdout_tx.send(Err(error));
                        break;
                    }
                }
            }
        });

        let (stderr_tx, stderr_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut stderr_text = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut stderr_text);
            let _ = stderr_tx.send(stderr_text);
        });

        Self {
            child,
            stdin,
            stdout_rx,
            stderr_rx,
            _workspace: workspace,
            _store: store,
        }
    }

    fn send_line(&mut self, line: &str) {
        self.stdin
            .write_all(line.as_bytes())
            .expect("write MCP message");
        self.stdin.write_all(b"\n").expect("terminate MCP message");
        self.stdin.flush().expect("flush MCP message");
    }

    fn recv_line(&mut self) -> String {
        match self.stdout_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(result) => result.expect("read MCP response"),
            Err(error) => {
                let stderr_output = self.shutdown();
                panic!("timed out waiting for MCP response: {error}; stderr: {stderr_output}");
            }
        }
    }

    fn shutdown(&mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.stderr_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap_or_default()
    }
}

#[test]
fn mcp_serve_responds_to_initialize_request() {
    let mut server = SpawnedServer::spawn();
    server.send_line(initialize_request());

    let response: serde_json::Value =
        serde_json::from_str(server.recv_line().trim_end()).expect("parse initialize response");

    assert_eq!(
        response.get("jsonrpc").and_then(|value| value.as_str()),
        Some("2.0")
    );
    assert_eq!(response.get("id").and_then(|value| value.as_i64()), Some(0));
    assert_eq!(
        response
            .get("result")
            .and_then(|value| value.get("protocolVersion"))
            .and_then(|value| value.as_str()),
        Some("2025-06-18")
    );

    let stderr_output = server.shutdown();
    assert!(
        stderr_output.is_empty(),
        "unexpected stderr during MCP initialize: {stderr_output}"
    );
}

#[test]
fn mcp_serve_lists_tools_after_initialize() {
    let mut server = SpawnedServer::spawn();
    server.send_line(initialize_request());
    let _ = server.recv_line();

    server.send_line(initialized_notification());
    server.send_line(list_tools_request());

    let response: serde_json::Value =
        serde_json::from_str(server.recv_line().trim_end()).expect("parse tools/list response");

    assert_eq!(response.get("id").and_then(|value| value.as_i64()), Some(1));
    let tools = response
        .get("result")
        .and_then(|value| value.get("tools"))
        .and_then(|value| value.as_array())
        .expect("tools/list result should include tools array");
    assert!(
        tools.iter().any(|tool| {
            tool.get("name").and_then(|value| value.as_str()) == Some("nodus_sync")
        }),
        "tools/list should advertise nodus_sync"
    );

    let stderr_output = server.shutdown();
    assert!(
        stderr_output.is_empty(),
        "unexpected stderr during tools/list: {stderr_output}"
    );
}
