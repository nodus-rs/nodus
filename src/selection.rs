use std::io::{self, IsTerminal, Write};
use std::path::Path;

use anstream::{AutoStream, ColorChoice};
use anstyle::{AnsiColor, Style};
use anyhow::{Result, bail};
use dialoguer::{Select, theme::ColorfulTheme};

use crate::adapters::{Adapter, Adapters};
use crate::manifest::Manifest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterSelectionSource {
    Cli,
    Manifest,
    Detected,
    Prompt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterSelection {
    pub adapters: Vec<Adapter>,
    pub source: AdapterSelectionSource,
    pub should_persist: bool,
}

pub fn resolve_adapter_selection(
    project_root: &Path,
    manifest: &Manifest,
    explicit: &[Adapter],
    allow_prompt: bool,
) -> Result<AdapterSelection> {
    if !explicit.is_empty() {
        let adapters = normalize_adapters(explicit);
        return Ok(AdapterSelection {
            adapters,
            source: AdapterSelectionSource::Cli,
            should_persist: true,
        });
    }

    if let Some(enabled) = manifest.enabled_adapters() {
        return Ok(AdapterSelection {
            adapters: normalize_adapters(enabled),
            source: AdapterSelectionSource::Manifest,
            should_persist: false,
        });
    }

    let detected = detect_repo_adapters(project_root);
    if !detected.is_empty() {
        return Ok(AdapterSelection {
            adapters: detected.to_vec(),
            source: AdapterSelectionSource::Detected,
            should_persist: true,
        });
    }

    if allow_prompt {
        return Ok(AdapterSelection {
            adapters: vec![prompt_for_adapter(project_root)?],
            source: AdapterSelectionSource::Prompt,
            should_persist: true,
        });
    }

    bail!(
        "no adapter configuration found in {}. Pass `--adapter <agents|claude|codex|cursor|opencode>` or configure `[adapters] enabled = [...]` in nodus.toml",
        project_root.display()
    );
}

pub fn detect_repo_adapters(project_root: &Path) -> Adapters {
    let mut detected = Adapters::NONE;

    if project_root.join(".claude").exists() {
        detected = detected.union(Adapters::CLAUDE);
    }
    if project_root.join(".codex").exists() {
        detected = detected.union(Adapters::CODEX);
    }
    if project_root.join(".agents").exists() {
        detected = detected.union(Adapters::AGENTS);
    }
    if project_root.join(".cursor").exists() {
        detected = detected.union(Adapters::CURSOR);
    }
    if project_root.join(".opencode").exists() || project_root.join("AGENTS.md").exists() {
        detected = detected.union(Adapters::OPENCODE);
    }

    detected
}

pub fn should_prompt_for_adapter() -> bool {
    !cfg!(test) && io::stdin().is_terminal() && io::stderr().is_terminal()
}

fn normalize_adapters(adapters: &[Adapter]) -> Vec<Adapter> {
    let mut adapters = adapters.to_vec();
    adapters.sort();
    adapters.dedup();
    adapters
}

fn prompt_for_adapter(project_root: &Path) -> Result<Adapter> {
    render_missing_adapter_notice(project_root)?;

    let selection = Select::with_theme(&adapter_prompt_theme())
        .with_prompt("Select an adapter to install")
        .items(adapter_prompt_items())
        .default(0)
        .interact_on_opt(&dialoguer::console::Term::stderr())?;

    let Some(index) = selection else {
        bail!("adapter selection cancelled");
    };

    Ok(Adapter::ALL[index])
}

fn render_missing_adapter_notice(project_root: &Path) -> Result<()> {
    let mut output = AutoStream::new(io::stderr().lock(), ColorChoice::Auto);
    render_missing_adapter_notice_to(project_root, output.current_choice(), &mut output)
}

fn render_missing_adapter_notice_to(
    project_root: &Path,
    color_choice: ColorChoice,
    output: &mut impl Write,
) -> Result<()> {
    writeln!(
        output,
        "{} no adapter configuration found in {}",
        paint("warning:", warning_style(), color_choice),
        project_root.display(),
    )?;
    writeln!(
        output,
        "{} use arrow keys to choose an adapter, then press Enter",
        paint("note:", note_style(), color_choice),
    )?;
    output.flush()?;
    Ok(())
}

fn adapter_prompt_items() -> &'static [&'static str; Adapter::ALL.len()] {
    &["agents", "claude", "codex", "cursor", "opencode"]
}

fn adapter_prompt_theme() -> ColorfulTheme {
    ColorfulTheme {
        active_item_style: dialoguer::console::Style::new().cyan().bold(),
        active_item_prefix: dialoguer::console::Style::new()
            .cyan()
            .bold()
            .apply_to(">".to_string()),
        checked_item_prefix: dialoguer::console::Style::new()
            .cyan()
            .bold()
            .apply_to(">".to_string()),
        prompt_style: dialoguer::console::Style::new().cyan().bold(),
        ..ColorfulTheme::default()
    }
}

fn paint(value: &str, style: Style, choice: ColorChoice) -> String {
    if matches!(choice, ColorChoice::Never) {
        value.to_string()
    } else {
        format!("{style}{value}{style:#}")
    }
}

fn warning_style() -> Style {
    Style::new().bold().fg_color(Some(AnsiColor::Yellow.into()))
}

fn note_style() -> Style {
    Style::new().bold().fg_color(Some(AnsiColor::Cyan.into()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn detects_existing_repo_adapter_roots() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join(".claude")).unwrap();
        fs::create_dir_all(temp.path().join(".cursor")).unwrap();
        fs::create_dir_all(temp.path().join(".opencode")).unwrap();

        let detected = detect_repo_adapters(temp.path());

        assert!(detected.contains(Adapter::Claude));
        assert!(detected.contains(Adapter::Cursor));
        assert!(detected.contains(Adapter::OpenCode));
        assert!(!detected.contains(Adapter::Codex));
        assert!(!detected.contains(Adapter::Agents));
    }

    #[test]
    fn resolves_detected_adapters_when_manifest_is_unset() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join(".codex")).unwrap();

        let selection =
            resolve_adapter_selection(temp.path(), &Manifest::default(), &[], false).unwrap();

        assert_eq!(selection.adapters, vec![Adapter::Codex]);
        assert_eq!(selection.source, AdapterSelectionSource::Detected);
        assert!(selection.should_persist);
    }

    #[test]
    fn rejects_noninteractive_repo_without_any_adapter_signal() {
        let temp = TempDir::new().unwrap();

        let error = resolve_adapter_selection(temp.path(), &Manifest::default(), &[], false)
            .unwrap_err()
            .to_string();

        assert!(error.contains("Pass `--adapter"));
    }

    #[test]
    fn adapter_prompt_items_follow_supported_adapter_order() {
        assert_eq!(
            adapter_prompt_items(),
            &["agents", "claude", "codex", "cursor", "opencode"]
        );
        assert_eq!(adapter_prompt_items().len(), Adapter::ALL.len());
    }

    #[test]
    fn missing_adapter_notice_mentions_project_root_and_guidance() {
        let temp = TempDir::new().unwrap();
        let mut output = Vec::new();

        render_missing_adapter_notice_to(temp.path(), ColorChoice::Never, &mut output).unwrap();

        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains(&format!(
            "warning: no adapter configuration found in {}",
            temp.path().display()
        )));
        assert!(rendered.contains("note: use arrow keys to choose an adapter, then press Enter"));
    }
}
