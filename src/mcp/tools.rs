use serde_json::{Value as JsonValue, json};

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
                "description": "Package alias, path, URL, or GitHub shortcut. Defaults to the current project."
            }
        },
        "required": []
    })
}

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
                "description": "Package URL, path, or GitHub shortcut (e.g. \"owner/repo\")"
            },
            "tag": {
                "type": "string",
                "description": "Git tag to pin (e.g. \"v1.0.0\")"
            },
            "branch": {
                "type": "string",
                "description": "Git branch to track"
            },
            "version": {
                "type": "string",
                "description": "Semver version requirement (e.g. \"^1.0\")"
            },
            "adapter": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Adapters to emit configs for (agents, claude, codex, copilot, cursor, opencode)"
            },
            "component": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Components to install (skills, agents, rules, commands, mcp)"
            },
            "exclude_component": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Components to exclude (skills, agents, rules, commands, mcp)"
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
                "description": "Package alias to remove"
            }
        },
        "required": ["package"]
    })
}

pub fn relay_input_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "package": {
                "type": "string",
                "description": "Package alias to relay. If omitted, relays all linked packages."
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
                "description": "Package alias to check. If omitted, checks all linked packages."
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
