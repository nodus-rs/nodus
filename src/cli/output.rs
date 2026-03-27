use serde::Serialize;

use super::args::Command;
use crate::adapters::Adapter;
use crate::manifest::DependencyKind;
use crate::report::Reporter;

pub(super) fn uses_json_output(command: &Command) -> bool {
    match command {
        Command::List { json }
        | Command::Info { json, .. }
        | Command::Outdated { json }
        | Command::Doctor { json } => *json,
        _ => false,
    }
}

pub(super) fn should_auto_check_for_updates(
    command: &Command,
    stderr_is_terminal: bool,
    update_check_disabled: bool,
) -> bool {
    stderr_is_terminal
        && !update_check_disabled
        && !uses_json_output(command)
        && !matches!(
            command,
            Command::Completion { .. } | Command::Upgrade { .. }
        )
}

pub(super) fn write_json<T: Serialize>(reporter: &Reporter, value: &T) -> anyhow::Result<()> {
    reporter.line(serde_json::to_string_pretty(value)?)
}

pub(super) fn format_adapters(adapters: &[Adapter]) -> String {
    adapters
        .iter()
        .map(|adapter| adapter.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn display_dependency(kind: DependencyKind, alias: &str) -> String {
    if kind.is_dev() {
        format!("{alias} [dev]")
    } else {
        alias.to_string()
    }
}
