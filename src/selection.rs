use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use anyhow::{Result, bail};

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
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let stderr = io::stderr();
    let mut stderr = stderr.lock();
    prompt_for_adapter_from(project_root, &mut stdin, &mut stderr)
}

fn prompt_for_adapter_from(
    project_root: &Path,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Adapter> {
    writeln!(
        output,
        "No adapter configuration found in {}.",
        project_root.display()
    )?;
    writeln!(output, "Select an adapter to install:")?;
    writeln!(output, "  1. agents")?;
    writeln!(output, "  2. claude")?;
    writeln!(output, "  3. codex")?;
    writeln!(output, "  4. cursor")?;
    writeln!(output, "  5. opencode")?;
    write!(output, "> ")?;
    output.flush()?;

    let mut line = String::new();
    input.read_line(&mut line)?;
    parse_prompt_answer(&line)
}

fn parse_prompt_answer(answer: &str) -> Result<Adapter> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "1" | "agents" => Ok(Adapter::Agents),
        "2" | "claude" => Ok(Adapter::Claude),
        "3" | "codex" => Ok(Adapter::Codex),
        "4" | "cursor" => Ok(Adapter::Cursor),
        "5" | "opencode" | "open-code" => Ok(Adapter::OpenCode),
        other => bail!("invalid adapter selection `{other}`"),
    }
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
    fn prompt_parser_accepts_numeric_choices() {
        assert_eq!(parse_prompt_answer("3\n").unwrap(), Adapter::Codex);
        assert_eq!(parse_prompt_answer("cursor").unwrap(), Adapter::Cursor);
        assert_eq!(parse_prompt_answer("open-code").unwrap(), Adapter::OpenCode);
    }
}
