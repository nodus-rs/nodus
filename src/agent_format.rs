use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use toml::Value as TomlValue;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct RawCodexAgentConfig {
    name: String,
    description: String,
    developer_instructions: String,
    #[serde(flatten)]
    extra: BTreeMap<String, TomlValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct RawPartialCodexAgentConfig {
    name: String,
    description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    developer_instructions: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, TomlValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CodexAgentConfig {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) developer_instructions: String,
    pub(crate) extra: BTreeMap<String, TomlValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PartialCodexAgentConfig {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) developer_instructions: Option<String>,
    pub(crate) extra: BTreeMap<String, TomlValue>,
}

pub(crate) fn parse_codex_agent_config(bytes: &[u8], context: &str) -> Result<CodexAgentConfig> {
    let partial = parse_partial_codex_agent_config(bytes, context)?;
    let developer_instructions = partial
        .developer_instructions
        .ok_or_else(|| anyhow::anyhow!("{context} field `developer_instructions` is required"))?;
    Ok(CodexAgentConfig {
        name: partial.name,
        description: partial.description,
        developer_instructions,
        extra: partial.extra,
    })
}

pub(crate) fn parse_partial_codex_agent_config(
    bytes: &[u8],
    context: &str,
) -> Result<PartialCodexAgentConfig> {
    let contents = String::from_utf8(bytes.to_vec())
        .with_context(|| format!("{context} must be valid UTF-8"))?;
    let raw: RawPartialCodexAgentConfig =
        toml::from_str(&contents).with_context(|| format!("failed to parse {context} as TOML"))?;
    validate_partial_codex_agent_fields(&raw, context)?;
    Ok(PartialCodexAgentConfig {
        name: raw.name,
        description: raw.description,
        developer_instructions: raw.developer_instructions,
        extra: raw.extra,
    })
}

pub(crate) fn codex_agent_config_uses_markdown_body(bytes: &[u8], context: &str) -> Result<bool> {
    Ok(parse_partial_codex_agent_config(bytes, context)?
        .developer_instructions
        .is_none())
}

pub(crate) fn serialize_codex_agent_config(config: &CodexAgentConfig) -> Result<Vec<u8>> {
    let mut contents = toml::to_string_pretty(&RawCodexAgentConfig {
        name: config.name.clone(),
        description: config.description.clone(),
        developer_instructions: config.developer_instructions.clone(),
        extra: config.extra.clone(),
    })
    .context("failed to serialize Codex agent TOML")?;
    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    Ok(contents.into_bytes())
}

pub(crate) fn serialize_partial_codex_agent_config(
    config: &PartialCodexAgentConfig,
) -> Result<Vec<u8>> {
    let mut contents = toml::to_string_pretty(&RawPartialCodexAgentConfig {
        name: config.name.clone(),
        description: config.description.clone(),
        developer_instructions: config.developer_instructions.clone(),
        extra: config.extra.clone(),
    })
    .context("failed to serialize Codex agent TOML")?;
    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    Ok(contents.into_bytes())
}

pub(crate) fn emitted_codex_agent_toml(
    source_toml: &[u8],
    runtime_name: Option<&str>,
    context: &str,
) -> Result<Vec<u8>> {
    let mut config = parse_codex_agent_config(source_toml, context)?;
    if let Some(runtime_name) = runtime_name {
        config.name = runtime_name.to_string();
    }
    serialize_codex_agent_config(&config)
}

pub(crate) fn emitted_codex_agent_toml_from_toml_and_markdown(
    source_toml: &[u8],
    source_markdown: &[u8],
    runtime_name: Option<&str>,
    context: &str,
) -> Result<Vec<u8>> {
    let mut config = parse_partial_codex_agent_config(source_toml, context)?;
    if let Some(runtime_name) = runtime_name {
        config.name = runtime_name.to_string();
    }
    let developer_instructions = match config.developer_instructions {
        Some(developer_instructions) => developer_instructions,
        None => markdown_body_string(source_markdown, context)?,
    };
    serialize_codex_agent_config(&CodexAgentConfig {
        name: config.name,
        description: config.description,
        developer_instructions,
        extra: config.extra,
    })
}

pub(crate) fn emitted_codex_agent_toml_from_markdown(
    source_markdown: &[u8],
    runtime_name: &str,
    description: &str,
    context: &str,
) -> Result<Vec<u8>> {
    let developer_instructions = markdown_body_string(source_markdown, context)?;
    serialize_codex_agent_config(&CodexAgentConfig {
        name: runtime_name.to_string(),
        description: description.to_string(),
        developer_instructions,
        extra: BTreeMap::new(),
    })
}

pub(crate) fn markdown_from_codex_agent_toml(source_toml: &[u8], context: &str) -> Result<Vec<u8>> {
    Ok(parse_codex_agent_config(source_toml, context)?
        .developer_instructions
        .into_bytes())
}

pub(crate) fn source_toml_from_managed_markdown(
    managed_markdown: &[u8],
    baseline_toml: &[u8],
    context: &str,
) -> Result<Vec<u8>> {
    let mut config = parse_codex_agent_config(baseline_toml, context)?;
    config.developer_instructions = String::from_utf8(managed_markdown.to_vec())
        .with_context(|| format!("{context} developer instructions must be valid UTF-8"))?;
    serialize_codex_agent_config(&config)
}

pub(crate) fn source_toml_metadata_from_managed_codex(
    managed_toml: &[u8],
    baseline_toml: Option<&[u8]>,
    emitted_runtime_name: Option<&str>,
    context: &str,
) -> Result<Vec<u8>> {
    let managed = parse_codex_agent_config(managed_toml, context)?;
    let baseline = baseline_toml
        .map(|baseline_toml| parse_partial_codex_agent_config(baseline_toml, context))
        .transpose()?;
    let mut source_name = managed.name;
    if let (Some(baseline), Some(emitted_runtime_name)) = (baseline.as_ref(), emitted_runtime_name)
        && source_name == emitted_runtime_name
        && baseline.name != emitted_runtime_name
    {
        source_name = baseline.name.clone();
    }
    serialize_partial_codex_agent_config(&PartialCodexAgentConfig {
        name: source_name,
        description: managed.description,
        developer_instructions: None,
        extra: managed.extra,
    })
}

pub(crate) fn source_toml_from_managed_codex(
    managed_toml: &[u8],
    baseline_toml: Option<&[u8]>,
    emitted_runtime_name: &str,
    context: &str,
) -> Result<Vec<u8>> {
    let mut config = parse_codex_agent_config(managed_toml, context)?;
    if let Some(baseline_toml) = baseline_toml {
        let baseline = parse_codex_agent_config(baseline_toml, context)?;
        if config.name == emitted_runtime_name && baseline.name != emitted_runtime_name {
            config.name = baseline.name;
        }
    }
    serialize_codex_agent_config(&config)
}

pub(crate) fn source_markdown_from_managed_codex(
    managed_toml: &[u8],
    baseline_markdown: &[u8],
    context: &str,
) -> Result<Vec<u8>> {
    let config = parse_codex_agent_config(managed_toml, context)?;
    replace_markdown_body_preserving_frontmatter(
        baseline_markdown,
        &config.developer_instructions,
        context,
    )
}

pub(crate) fn default_codex_agent_description(agent_id: &str) -> String {
    format!("Instructions for the `{agent_id}` agent.")
}

fn validate_partial_codex_agent_fields(
    config: &RawPartialCodexAgentConfig,
    context: &str,
) -> Result<()> {
    if config.name.trim().is_empty() {
        bail!("{context} field `name` must not be empty");
    }
    if config.description.trim().is_empty() {
        bail!("{context} field `description` must not be empty");
    }
    if config
        .developer_instructions
        .as_ref()
        .is_some_and(|developer_instructions| developer_instructions.trim().is_empty())
    {
        bail!("{context} field `developer_instructions` must not be empty");
    }
    Ok(())
}

fn markdown_body_string(source_markdown: &[u8], context: &str) -> Result<String> {
    let contents = String::from_utf8(source_markdown.to_vec())
        .with_context(|| format!("{context} must be valid UTF-8"))?;
    Ok(markdown_body_without_yaml_frontmatter(&contents).to_string())
}

pub(crate) fn replace_markdown_body_preserving_frontmatter(
    source_markdown: &[u8],
    replacement_body: &str,
    context: &str,
) -> Result<Vec<u8>> {
    let contents = String::from_utf8(source_markdown.to_vec())
        .with_context(|| format!("{context} Markdown source must be valid UTF-8"))?;
    let body_start = yaml_frontmatter_body_start(&contents).unwrap_or(0);
    let mut replaced = String::with_capacity(body_start + replacement_body.len());
    replaced.push_str(&contents[..body_start]);
    replaced.push_str(replacement_body);
    Ok(replaced.into_bytes())
}

pub(crate) fn markdown_body_without_yaml_frontmatter(contents: &str) -> &str {
    yaml_frontmatter_body_start(contents)
        .map(|body_start| &contents[body_start..])
        .unwrap_or(contents)
}

fn yaml_frontmatter_body_start(contents: &str) -> Option<usize> {
    let mut lines = contents.split_inclusive('\n');
    let first = lines.next()?;
    if trim_line_ending(first) != "---" {
        return None;
    }

    let mut offset = first.len();
    for line in lines {
        offset += line.len();
        if trim_line_ending(line) == "---" {
            return Some(offset);
        }
    }

    None
}

fn trim_line_ending(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_serializes_codex_agent_toml() {
        let source = br#"name = "security"
description = "Review security-sensitive code."
developer_instructions = "Be careful."
model = "gpt-5"
"#;

        let config = parse_codex_agent_config(source, "agent").unwrap();
        assert_eq!(config.name, "security");
        assert_eq!(config.description, "Review security-sensitive code.");
        assert_eq!(config.developer_instructions, "Be careful.");
        assert_eq!(
            config.extra.get("model"),
            Some(&TomlValue::String("gpt-5".into()))
        );

        let serialized = String::from_utf8(serialize_codex_agent_config(&config).unwrap()).unwrap();
        assert!(serialized.contains("name = \"security\""));
        assert!(serialized.contains("model = \"gpt-5\""));
        assert!(serialized.ends_with('\n'));
    }

    #[test]
    fn parses_partial_codex_agent_toml_without_developer_instructions() {
        let source = br#"name = "security"
description = "Review security-sensitive code."
model = "gpt-5"
"#;

        let config = parse_partial_codex_agent_config(source, "agent").unwrap();
        assert_eq!(config.name, "security");
        assert_eq!(config.description, "Review security-sensitive code.");
        assert_eq!(config.developer_instructions, None);
        assert!(codex_agent_config_uses_markdown_body(source, "agent").unwrap());
        assert!(parse_codex_agent_config(source, "agent").is_err());
    }

    #[test]
    fn emits_codex_agent_toml_from_markdown_body_without_frontmatter() {
        let markdown = b"---\ntitle: Security\n---\n# Security\nReview carefully.\n";

        let emitted = String::from_utf8(
            emitted_codex_agent_toml_from_markdown(markdown, "security", "Security.", "agent")
                .unwrap(),
        )
        .unwrap();

        assert!(emitted.contains("name = \"security\""));
        assert!(emitted.contains("description = \"Security.\""));
        assert_eq!(
            parse_codex_agent_config(emitted.as_bytes(), "emitted")
                .unwrap()
                .developer_instructions,
            "# Security\nReview carefully.\n"
        );
        assert!(!emitted.contains("title: Security"));
    }

    #[test]
    fn emits_codex_agent_toml_from_metadata_toml_and_markdown_body() {
        let source_toml = br#"name = "Security reviewer"
description = "Codex metadata."
model = "gpt-5"
"#;
        let source_markdown = b"---\ntitle: Security\n---\n# Security\nReview carefully.\n";

        let emitted = String::from_utf8(
            emitted_codex_agent_toml_from_toml_and_markdown(
                source_toml,
                source_markdown,
                Some("security_abc123"),
                "agent",
            )
            .unwrap(),
        )
        .unwrap();

        assert!(emitted.contains("name = \"security_abc123\""));
        assert!(emitted.contains("description = \"Codex metadata.\""));
        assert!(emitted.contains("model = \"gpt-5\""));
        assert_eq!(
            parse_codex_agent_config(emitted.as_bytes(), "emitted")
                .unwrap()
                .developer_instructions,
            "# Security\nReview carefully.\n"
        );
        assert!(!emitted.contains("title: Security"));
    }

    #[test]
    fn restores_source_name_when_runtime_name_was_only_a_collision_rewrite() {
        let baseline = br#"name = "Security reviewer"
description = "Review security-sensitive code."
developer_instructions = "Be careful."
"#;
        let managed = br#"name = "security_abc123"
description = "Review security-sensitive code."
developer_instructions = "Be extra careful."
"#;

        let restored = String::from_utf8(
            source_toml_from_managed_codex(managed, Some(baseline), "security_abc123", "agent")
                .unwrap(),
        )
        .unwrap();

        assert!(restored.contains("name = \"Security reviewer\""));
        assert!(restored.contains("Be extra careful."));
    }

    #[test]
    fn restores_metadata_only_toml_without_developer_instructions() {
        let baseline = br#"name = "Security reviewer"
description = "Review security-sensitive code."
model = "gpt-5"
"#;
        let managed = br#"name = "security_abc123"
description = "Updated description."
developer_instructions = "Be extra careful."
model = "gpt-5.1"
sandbox_mode = "workspace-write"
"#;

        let restored = String::from_utf8(
            source_toml_metadata_from_managed_codex(
                managed,
                Some(baseline),
                Some("security_abc123"),
                "agent",
            )
            .unwrap(),
        )
        .unwrap();

        assert!(restored.contains("name = \"Security reviewer\""));
        assert!(restored.contains("description = \"Updated description.\""));
        assert!(restored.contains("model = \"gpt-5.1\""));
        assert!(restored.contains("sandbox_mode = \"workspace-write\""));
        assert!(!restored.contains("developer_instructions"));
    }

    #[test]
    fn replaces_markdown_body_preserving_frontmatter() {
        let source = b"---\ntitle: Security\n---\n# Security\nOld body.\n";

        let replaced = replace_markdown_body_preserving_frontmatter(
            source,
            "# Security\nNew body.\n",
            "agent",
        )
        .unwrap();

        assert_eq!(
            String::from_utf8(replaced).unwrap(),
            "---\ntitle: Security\n---\n# Security\nNew body.\n"
        );
    }
}
