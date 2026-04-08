use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use toml::Value as TomlValue;

use crate::paths::display_path;
use crate::report::Reporter;

const SERVER_NAME: &str = "nodus";
const EXPECTED_COMMAND: &str = "nodus";
const EXPECTED_ARGS: [&str; 2] = ["mcp", "serve"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpStatusState {
    Configured,
    NotFound,
    MissingServer,
    PathDependent,
    Misconfigured,
    ParseError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpOverallStatus {
    Healthy,
    NotConfigured,
    Broken,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpCommandStatus {
    pub command: String,
    pub found_on_path: bool,
    pub resolved_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpConfigStatus {
    pub runtime: String,
    pub path: String,
    pub exists: bool,
    pub state: McpStatusState,
    pub message: String,
    pub observed_command: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpStatusSummary {
    pub overall_status: McpOverallStatus,
    pub configured_count: usize,
    pub issue_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpStatusReport {
    pub project_root: String,
    pub manifest_exists: bool,
    pub lockfile_exists: bool,
    pub command: McpCommandStatus,
    pub configs: Vec<McpConfigStatus>,
    pub summary: McpStatusSummary,
}

#[derive(Debug, Default, Deserialize)]
struct ProjectMcpConfig {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: std::collections::BTreeMap<String, ProjectMcpServer>,
}

#[derive(Debug, Default, Deserialize)]
struct ProjectMcpServer {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ProjectCodexConfig {
    #[serde(default)]
    mcp_servers: std::collections::BTreeMap<String, TomlValue>,
}

#[derive(Debug, Default, Deserialize)]
struct ProjectOpenCodeConfig {
    #[serde(rename = "mcp", default)]
    mcp_servers: std::collections::BTreeMap<String, JsonValue>,
}

pub fn inspect_status_in_dir(project_root: &Path) -> Result<McpStatusReport> {
    let command = command_status();
    let configs = vec![
        inspect_project_json(project_root)?,
        inspect_codex_config(project_root)?,
        inspect_opencode_config(project_root)?,
    ];
    let configured_count = configs
        .iter()
        .filter(|status| status.state == McpStatusState::Configured)
        .count();
    let issue_count = configs
        .iter()
        .filter(|status| {
            matches!(
                status.state,
                McpStatusState::MissingServer
                    | McpStatusState::PathDependent
                    | McpStatusState::Misconfigured
                    | McpStatusState::ParseError
            )
        })
        .count();
    let overall_status = if issue_count > 0 {
        McpOverallStatus::Broken
    } else if configured_count == 0 {
        McpOverallStatus::NotConfigured
    } else {
        McpOverallStatus::Healthy
    };

    Ok(McpStatusReport {
        project_root: display_path(project_root),
        manifest_exists: project_root.join("nodus.toml").exists(),
        lockfile_exists: project_root.join("nodus.lock").exists(),
        command,
        configs,
        summary: McpStatusSummary {
            overall_status,
            configured_count,
            issue_count,
        },
    })
}

pub fn render_status(report: &McpStatusReport, reporter: &Reporter) -> Result<()> {
    reporter.line(format!("Project root: {}", report.project_root))?;
    reporter.line(format!(
        "Manifest: {}",
        if report.manifest_exists {
            "present"
        } else {
            "missing"
        }
    ))?;
    reporter.line(format!(
        "Lockfile: {}",
        if report.lockfile_exists {
            "present"
        } else {
            "missing"
        }
    ))?;

    let command_status = if let Some(path) = &report.command.resolved_path {
        format!("{} ({path})", report.command.command)
    } else {
        format!("{} (not found on PATH)", report.command.command)
    };
    reporter.line(format!("PATH command: {command_status}"))?;

    for config in &report.configs {
        reporter.line(format!("{}: {}", config.path, render_config_state(config)))?;
    }

    if report
        .configs
        .iter()
        .any(|status| status.state == McpStatusState::PathDependent)
    {
        reporter.note(
            "PATH-dependent `nodus` entries can fail in GUI clients; run `nodus sync` to refresh them to an absolute path",
        )?;
    } else if !report.command.found_on_path {
        reporter.note("the configured nodus command is not currently resolvable on PATH")?;
    }
    if report.summary.overall_status == McpOverallStatus::NotConfigured {
        if report.manifest_exists || report.lockfile_exists {
            reporter.note("no managed MCP config is present yet; run `nodus sync` to emit it")?;
        } else {
            reporter.note(
                "this directory does not look like a synced nodus project yet, so no MCP config is expected",
            )?;
        }
    }

    Ok(())
}

fn render_config_state(config: &McpConfigStatus) -> String {
    match &config.observed_command {
        Some(command) if !command.is_empty() => {
            format!("{} ({})", config.message, format_command(command))
        }
        _ => config.message.clone(),
    }
}

fn inspect_project_json(project_root: &Path) -> Result<McpConfigStatus> {
    let path = project_root.join(".mcp.json");
    let display = display_path(&path);
    if !path.exists() {
        return Ok(McpConfigStatus {
            runtime: "project".into(),
            path: display,
            exists: false,
            state: McpStatusState::NotFound,
            message: "not found".into(),
            observed_command: None,
        });
    }

    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let config: ProjectMcpConfig = match serde_json::from_str(&contents) {
        Ok(config) => config,
        Err(error) => {
            return Ok(McpConfigStatus {
                runtime: "project".into(),
                path: display,
                exists: true,
                state: McpStatusState::ParseError,
                message: format!("parse error: {error}"),
                observed_command: None,
            });
        }
    };

    let Some(server) = config.mcp_servers.get(SERVER_NAME) else {
        return Ok(McpConfigStatus {
            runtime: "project".into(),
            path: display,
            exists: true,
            state: McpStatusState::MissingServer,
            message: "missing `nodus` server entry".into(),
            observed_command: None,
        });
    };

    let observed = command_parts(server.command.as_deref(), &server.args);
    Ok(match observed.as_deref() {
        Some(command) if command_matches_project_command(command) => McpConfigStatus {
            runtime: "project".into(),
            path: display,
            exists: true,
            state: McpStatusState::Configured,
            message: "configured".into(),
            observed_command: Some(command.to_vec()),
        },
        Some(command) if command_is_path_dependent(command) => McpConfigStatus {
            runtime: "project".into(),
            path: display,
            exists: true,
            state: McpStatusState::PathDependent,
            message: "uses PATH-dependent `nodus`; run `nodus sync` to refresh".into(),
            observed_command: Some(command.to_vec()),
        },
        Some(command) => McpConfigStatus {
            runtime: "project".into(),
            path: display,
            exists: true,
            state: McpStatusState::Misconfigured,
            message: "expected `nodus mcp serve`".into(),
            observed_command: Some(command.to_vec()),
        },
        None => McpConfigStatus {
            runtime: "project".into(),
            path: display,
            exists: true,
            state: McpStatusState::Misconfigured,
            message: "expected `nodus mcp serve`".into(),
            observed_command: None,
        },
    })
}

fn inspect_codex_config(project_root: &Path) -> Result<McpConfigStatus> {
    let path = project_root.join(".codex/config.toml");
    let display = display_path(&path);
    if !path.exists() {
        return Ok(McpConfigStatus {
            runtime: "codex".into(),
            path: display,
            exists: false,
            state: McpStatusState::NotFound,
            message: "not found".into(),
            observed_command: None,
        });
    }

    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let config: ProjectCodexConfig = match toml::from_str(&contents) {
        Ok(config) => config,
        Err(error) => {
            return Ok(McpConfigStatus {
                runtime: "codex".into(),
                path: display,
                exists: true,
                state: McpStatusState::ParseError,
                message: format!("parse error: {error}"),
                observed_command: None,
            });
        }
    };

    let Some(server) = config.mcp_servers.get(SERVER_NAME) else {
        return Ok(McpConfigStatus {
            runtime: "codex".into(),
            path: display,
            exists: true,
            state: McpStatusState::MissingServer,
            message: "missing `nodus` server entry".into(),
            observed_command: None,
        });
    };

    let observed = codex_command(server);
    Ok(match observed.as_deref() {
        Some(command) if command_matches_project_command(command) => McpConfigStatus {
            runtime: "codex".into(),
            path: display,
            exists: true,
            state: McpStatusState::Configured,
            message: "configured".into(),
            observed_command: Some(command.to_vec()),
        },
        Some(command) if command_is_path_dependent(command) => McpConfigStatus {
            runtime: "codex".into(),
            path: display,
            exists: true,
            state: McpStatusState::PathDependent,
            message: "uses PATH-dependent `nodus`; run `nodus sync` to refresh".into(),
            observed_command: Some(command.to_vec()),
        },
        Some(command) => McpConfigStatus {
            runtime: "codex".into(),
            path: display,
            exists: true,
            state: McpStatusState::Misconfigured,
            message: "expected `nodus mcp serve`".into(),
            observed_command: Some(command.to_vec()),
        },
        None => McpConfigStatus {
            runtime: "codex".into(),
            path: display,
            exists: true,
            state: McpStatusState::Misconfigured,
            message: "expected `nodus mcp serve`".into(),
            observed_command: None,
        },
    })
}

fn inspect_opencode_config(project_root: &Path) -> Result<McpConfigStatus> {
    let path = project_root.join("opencode.json");
    let display = display_path(&path);
    if !path.exists() {
        return Ok(McpConfigStatus {
            runtime: "opencode".into(),
            path: display,
            exists: false,
            state: McpStatusState::NotFound,
            message: "not found".into(),
            observed_command: None,
        });
    }

    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let config: ProjectOpenCodeConfig = match serde_json::from_str(&contents) {
        Ok(config) => config,
        Err(error) => {
            return Ok(McpConfigStatus {
                runtime: "opencode".into(),
                path: display,
                exists: true,
                state: McpStatusState::ParseError,
                message: format!("parse error: {error}"),
                observed_command: None,
            });
        }
    };

    let Some(server) = config.mcp_servers.get(SERVER_NAME) else {
        return Ok(McpConfigStatus {
            runtime: "opencode".into(),
            path: display,
            exists: true,
            state: McpStatusState::MissingServer,
            message: "missing `nodus` server entry".into(),
            observed_command: None,
        });
    };

    let observed = opencode_command(server);
    Ok(match observed.as_deref() {
        Some(command) if command_matches_project_command(command) => McpConfigStatus {
            runtime: "opencode".into(),
            path: display,
            exists: true,
            state: McpStatusState::Configured,
            message: "configured".into(),
            observed_command: Some(command.to_vec()),
        },
        Some(command) if command_is_path_dependent(command) => McpConfigStatus {
            runtime: "opencode".into(),
            path: display,
            exists: true,
            state: McpStatusState::PathDependent,
            message: "uses PATH-dependent `nodus`; run `nodus sync` to refresh".into(),
            observed_command: Some(command.to_vec()),
        },
        Some(command) => McpConfigStatus {
            runtime: "opencode".into(),
            path: display,
            exists: true,
            state: McpStatusState::Misconfigured,
            message: "expected `nodus mcp serve`".into(),
            observed_command: Some(command.to_vec()),
        },
        None => McpConfigStatus {
            runtime: "opencode".into(),
            path: display,
            exists: true,
            state: McpStatusState::Misconfigured,
            message: "expected `nodus mcp serve`".into(),
            observed_command: None,
        },
    })
}

fn command_status() -> McpCommandStatus {
    let resolved = resolve_command_on_path(EXPECTED_COMMAND);
    McpCommandStatus {
        command: EXPECTED_COMMAND.into(),
        found_on_path: resolved.is_some(),
        resolved_path: resolved.as_deref().map(display_path),
    }
}

fn command_parts(command: Option<&str>, args: &[String]) -> Option<Vec<String>> {
    let command = command?.trim();
    if command.is_empty() {
        return None;
    }

    Some(
        std::iter::once(command.to_string())
            .chain(args.iter().cloned())
            .collect(),
    )
}

fn codex_command(value: &TomlValue) -> Option<Vec<String>> {
    let table = value.as_table()?;
    let command = table.get("command")?.as_str()?.trim();
    if command.is_empty() {
        return None;
    }

    let mut observed = vec![command.to_string()];
    let args = match table.get("args") {
        Some(value) => value
            .as_array()?
            .iter()
            .map(TomlValue::as_str)
            .collect::<Option<Vec<_>>>()?,
        None => Vec::new(),
    };
    observed.extend(args.into_iter().map(ToOwned::to_owned));
    Some(observed)
}

fn opencode_command(value: &JsonValue) -> Option<Vec<String>> {
    let object = value.as_object()?;
    if object.get("type").and_then(JsonValue::as_str) != Some("local") {
        return None;
    }
    object
        .get("command")?
        .as_array()?
        .iter()
        .map(JsonValue::as_str)
        .collect::<Option<Vec<_>>>()
        .map(|parts| parts.into_iter().map(ToOwned::to_owned).collect())
}

fn command_matches_project_command(command: &[String]) -> bool {
    let Some((binary, args)) = command.split_first() else {
        return false;
    };
    if !binary_looks_like_nodus(binary) || binary == EXPECTED_COMMAND {
        return false;
    }
    normalized_server_args(args)
        .is_some_and(|args| args.iter().copied().eq(EXPECTED_ARGS.iter().copied()))
}

fn command_is_path_dependent(command: &[String]) -> bool {
    let Some((binary, args)) = command.split_first() else {
        return false;
    };
    binary == EXPECTED_COMMAND
        && normalized_server_args(args)
            .is_some_and(|args| args.iter().copied().eq(EXPECTED_ARGS.iter().copied()))
}

fn binary_looks_like_nodus(command: &str) -> bool {
    Path::new(command)
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value == "nodus" || value.starts_with("nodus-"))
}

fn normalized_server_args<'a>(args: &'a [String]) -> Option<Vec<&'a str>> {
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    match args.as_slice() {
        ["--store-path", store_path, rest @ ..] if !store_path.is_empty() => Some(rest.to_vec()),
        rest => Some(rest.to_vec()),
    }
}

fn format_command(command: &[String]) -> String {
    command.join(" ")
}

fn resolve_command_on_path(command: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    for directory in env::split_paths(&path_var) {
        for candidate in executable_candidates(directory.join(command)) {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(not(windows))]
fn executable_candidates(path: PathBuf) -> Vec<PathBuf> {
    vec![path]
}

#[cfg(windows)]
fn executable_candidates(path: PathBuf) -> Vec<PathBuf> {
    use std::ffi::OsString;

    if path.extension().is_some() {
        return vec![path];
    }

    let pathext = env::var_os("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
    let extensions = pathext
        .to_string_lossy()
        .split(';')
        .filter_map(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.trim_start_matches('.').to_string())
            }
        })
        .collect::<Vec<_>>();

    let mut candidates = vec![path.clone()];
    for extension in extensions {
        candidates.push(path.with_extension(extension));
    }
    candidates
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn reports_configured_project_json_entry() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join(".mcp.json"),
            format!(
                r#"{{"mcpServers":{{"nodus":{{"command":"{}","args":["mcp","serve"]}}}}}}"#,
                display_path(&env::current_exe().unwrap())
            ),
        )
        .unwrap();

        let status = inspect_project_json(temp.path()).unwrap();
        assert_eq!(status.state, McpStatusState::Configured);
        assert!(status.observed_command.unwrap()[0].contains("nodus"));
    }

    #[test]
    fn reports_path_dependent_project_json_entry() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join(".mcp.json"),
            r#"{"mcpServers":{"nodus":{"command":"nodus","args":["mcp","serve"]}}}"#,
        )
        .unwrap();

        let status = inspect_project_json(temp.path()).unwrap();
        assert_eq!(status.state, McpStatusState::PathDependent);
    }

    #[test]
    fn reports_misconfigured_opencode_entry() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("opencode.json"),
            r#"{"mcp":{"nodus":{"type":"local","command":["cargo","run"]}}}"#,
        )
        .unwrap();

        let status = inspect_opencode_config(temp.path()).unwrap();
        assert_eq!(status.state, McpStatusState::Misconfigured);
        assert_eq!(
            status.observed_command,
            Some(vec!["cargo".into(), "run".into()])
        );
    }
}
