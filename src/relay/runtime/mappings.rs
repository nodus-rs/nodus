use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{DependencyContext, RelayFileMapping, RelayTransform};
use crate::adapters::{
    Adapter, Adapters, ArtifactKind, ManagedArtifactNames, managed_artifact_id,
    managed_artifact_path, managed_skill_root,
};
use crate::agent_format::{
    default_codex_agent_description, emitted_codex_agent_toml,
    emitted_codex_agent_toml_from_markdown, markdown_from_codex_agent_toml,
    source_toml_from_managed_codex, source_toml_from_managed_markdown,
};
use crate::manifest::SkillEntry;
use crate::paths::strip_path_prefix;
use crate::resolver::ResolvedPackage;

pub(super) fn build_mappings(
    names: &ManagedArtifactNames,
    _packages: &[ResolvedPackage],
    dependency: &DependencyContext,
    project_root: &Path,
    selected_adapters: Adapters,
    linked_repo: &Path,
) -> Result<Vec<RelayFileMapping>> {
    let mut mappings = Vec::new();
    let package = &dependency.package;
    let snapshot_root = &dependency.snapshot_root;

    for skill in &package.manifest.discovered.skills {
        if !package.selects_component(crate::manifest::DependencyComponent::Skills) {
            continue;
        }

        for adapter in [
            Adapter::Agents,
            Adapter::Claude,
            Adapter::Codex,
            Adapter::Copilot,
            Adapter::Cursor,
            Adapter::OpenCode,
        ] {
            if !selected_adapters.contains(adapter) {
                continue;
            }
            let source_root = snapshot_root.join(&skill.path);
            let managed_root = managed_skill_root(names, project_root, adapter, package, &skill.id);
            let target_root = linked_repo.join(&skill.path);
            mappings.extend(skill_mappings(
                names,
                adapter,
                package,
                skill,
                &source_root,
                &target_root,
                &managed_root,
            )?);
        }
    }

    if package.selects_component(crate::manifest::DependencyComponent::Agents) {
        for adapter in [
            Adapter::Claude,
            Adapter::Codex,
            Adapter::Copilot,
            Adapter::OpenCode,
        ] {
            if !selected_adapters.contains(adapter) {
                continue;
            }
            for agent in package.manifest.discovered.selected_agents(adapter) {
                if let Some(managed_path) = managed_artifact_path(
                    names,
                    project_root,
                    adapter,
                    ArtifactKind::Agent,
                    package,
                    &agent.id,
                ) {
                    mappings.push(file_mapping(
                        managed_path,
                        Some(snapshot_root.join(&agent.path)),
                        linked_repo.join(&agent.path),
                        agent.id.clone(),
                        agent_transform(names, adapter, package, agent),
                    ));
                }
            }
        }
    }

    for rule in &package.manifest.discovered.rules {
        if !package.selects_component(crate::manifest::DependencyComponent::Rules) {
            continue;
        }
        for (adapter, kind) in [
            (Adapter::Claude, ArtifactKind::Rule),
            (Adapter::Cursor, ArtifactKind::Rule),
            (Adapter::OpenCode, ArtifactKind::Rule),
        ] {
            if !selected_adapters.contains(adapter) {
                continue;
            }
            if let Some(managed_path) =
                managed_artifact_path(names, project_root, adapter, kind, package, &rule.id)
            {
                mappings.push(file_mapping(
                    managed_path,
                    Some(snapshot_root.join(&rule.path)),
                    linked_repo.join(&rule.path),
                    rule.id.clone(),
                    RelayTransform::None,
                ));
            }
        }
    }

    for command in &package.manifest.discovered.commands {
        if !package.selects_component(crate::manifest::DependencyComponent::Commands) {
            continue;
        }
        if selected_adapters.contains(Adapter::Codex) {
            let managed_skill_id =
                crate::adapters::codex::synthetic_command_skill_id(names, package, &command.id);
            mappings.push(file_mapping(
                managed_skill_root(
                    names,
                    project_root,
                    Adapter::Codex,
                    package,
                    &managed_skill_id,
                )
                .join("SKILL.md"),
                Some(snapshot_root.join(&command.path)),
                linked_repo.join(&command.path),
                command.id.clone(),
                RelayTransform::CodexCommandSkill {
                    managed_skill_id,
                    source_command_id: command.id.clone(),
                },
            ));
        }
        for (adapter, kind) in [
            (Adapter::Agents, ArtifactKind::Command),
            (Adapter::Claude, ArtifactKind::Command),
            (Adapter::Cursor, ArtifactKind::Command),
            (Adapter::OpenCode, ArtifactKind::Command),
        ] {
            if !selected_adapters.contains(adapter) {
                continue;
            }
            if let Some(managed_path) =
                managed_artifact_path(names, project_root, adapter, kind, package, &command.id)
            {
                mappings.push(file_mapping(
                    managed_path,
                    Some(snapshot_root.join(&command.path)),
                    linked_repo.join(&command.path),
                    command.id.clone(),
                    RelayTransform::None,
                ));
            }
        }
    }

    for mapping in package.managed_paths() {
        let known_targets = mapping
            .files
            .iter()
            .map(|file| file.target_relative.clone())
            .collect::<BTreeSet<_>>();
        for file in &mapping.files {
            mappings.push(file_mapping(
                project_root.join(&file.target_relative),
                Some(snapshot_root.join(&file.source_relative)),
                linked_repo.join(&file.source_relative),
                file.source_relative.to_string_lossy().into_owned(),
                RelayTransform::None,
            ));
        }
        let is_directory = mapping.files.is_empty()
            || mapping
                .files
                .iter()
                .any(|file| file.target_relative != mapping.target_root);
        if is_directory {
            let managed_root = project_root.join(&mapping.target_root);
            if managed_root.is_dir() {
                for entry in walkdir::WalkDir::new(&managed_root) {
                    let entry = entry?;
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    let relative =
                        strip_path_prefix(entry.path(), &managed_root).with_context(|| {
                            format!("failed to make {} relative", entry.path().display())
                        })?;
                    let target_relative = mapping.target_root.join(relative);
                    if known_targets.contains(&target_relative) {
                        continue;
                    }
                    let source_relative = mapping.source_root.join(relative);
                    mappings.push(file_mapping(
                        entry.path().to_path_buf(),
                        None,
                        linked_repo.join(&source_relative),
                        source_relative.to_string_lossy().into_owned(),
                        RelayTransform::None,
                    ));
                }
            }
        }
    }
    Ok(mappings)
}

pub(super) fn build_missing_mappings(
    names: &ManagedArtifactNames,
    packages: &[ResolvedPackage],
    dependency: &DependencyContext,
    project_root: &Path,
    selected_adapters: Adapters,
    preferred_adapter: Option<Adapter>,
    linked_repo: &Path,
) -> Result<Vec<RelayFileMapping>> {
    if let Some(adapter) = preferred_adapter {
        if !selected_adapters.contains(adapter) {
            bail!(
                "relay preferred adapter `{}` is not active in the current managed outputs",
                adapter
            );
        }
        return build_missing_mappings_for_adapter(
            names,
            packages,
            dependency,
            project_root,
            adapter,
            linked_repo,
        );
    }

    let mut candidates = Vec::new();
    for adapter in Adapter::ALL {
        if !selected_adapters.contains(adapter) {
            continue;
        }
        let mappings = build_missing_mappings_for_adapter(
            names,
            packages,
            dependency,
            project_root,
            adapter,
            linked_repo,
        )?;
        if !mappings.is_empty() {
            candidates.push((adapter, mappings));
        }
    }

    match candidates.len() {
        0 => Ok(Vec::new()),
        1 => Ok(candidates.remove(0).1),
        _ => bail!(
            "managed creation candidates for `{}` appear in multiple adapters; rerun with `--via <adapter>`",
            dependency.alias
        ),
    }
}

fn build_missing_mappings_for_adapter(
    names: &ManagedArtifactNames,
    packages: &[ResolvedPackage],
    dependency: &DependencyContext,
    project_root: &Path,
    adapter: Adapter,
    linked_repo: &Path,
) -> Result<Vec<RelayFileMapping>> {
    let mut mappings = Vec::new();
    let runtime_root = crate::adapters::runtime_root(project_root, adapter);

    if dependency
        .package
        .selects_component(crate::manifest::DependencyComponent::Skills)
        || (adapter == Adapter::Codex
            && dependency
                .package
                .selects_component(crate::manifest::DependencyComponent::Commands))
    {
        let known_skill_roots = packages
            .iter()
            .filter(|package| {
                package.selects_component(crate::manifest::DependencyComponent::Skills)
                    || (adapter == Adapter::Codex
                        && package
                            .selects_component(crate::manifest::DependencyComponent::Commands))
            })
            .flat_map(|package| {
                let mut roots = package
                    .manifest
                    .discovered
                    .skills
                    .iter()
                    .map(|skill| {
                        managed_skill_root(names, project_root, adapter, package, &skill.id)
                    })
                    .collect::<Vec<_>>();
                if adapter == Adapter::Codex
                    && package.selects_component(crate::manifest::DependencyComponent::Commands)
                {
                    roots.extend(package.manifest.discovered.commands.iter().map(|command| {
                        let managed_skill_id = crate::adapters::codex::synthetic_command_skill_id(
                            names,
                            package,
                            &command.id,
                        );
                        managed_skill_root(names, project_root, adapter, package, &managed_skill_id)
                    }));
                }
                roots
            })
            .collect::<std::collections::BTreeSet<_>>();
        let skills_root = runtime_root.join("skills");
        if skills_root.is_dir() {
            for entry in fs::read_dir(&skills_root)
                .with_context(|| format!("failed to read {}", skills_root.display()))?
            {
                let entry = entry?;
                let managed_root = entry.path();
                if !entry.file_type()?.is_dir()
                    || known_skill_roots.contains(&managed_root)
                    || !managed_root.join("SKILL.md").is_file()
                {
                    continue;
                }
                let skill_id = entry.file_name().to_string_lossy().into_owned();
                if adapter == Adapter::Codex
                    && skill_id.starts_with(crate::adapters::codex::SYNTHETIC_COMMAND_SKILL_PREFIX)
                {
                    if !dependency
                        .package
                        .selects_component(crate::manifest::DependencyComponent::Commands)
                    {
                        bail!(
                            "Codex synthetic command skill `{skill_id}` requires the `commands` component"
                        );
                    }
                    let command_id =
                        crate::adapters::codex::source_command_id_from_synthetic_skill_id(
                            &skill_id,
                        )?;
                    mappings.push(file_mapping(
                        managed_root.join("SKILL.md"),
                        None,
                        linked_repo
                            .join("commands")
                            .join(format!("{command_id}.md")),
                        command_id.to_string(),
                        RelayTransform::CodexCommandSkill {
                            managed_skill_id: skill_id.clone(),
                            source_command_id: command_id.to_string(),
                        },
                    ));
                    continue;
                }
                if adapter == Adapter::Codex
                    && !dependency
                        .package
                        .selects_component(crate::manifest::DependencyComponent::Skills)
                {
                    bail!(
                        "Codex managed skill `{skill_id}` requires the `skills` component or the reserved `{}` prefix",
                        crate::adapters::codex::SYNTHETIC_COMMAND_SKILL_PREFIX
                    );
                }
                mappings.extend(missing_skill_mappings(
                    names,
                    adapter,
                    &dependency.package,
                    &skill_id,
                    &managed_root,
                    &linked_repo.join("skills").join(&skill_id),
                )?);
            }
        }
    }

    if dependency
        .package
        .selects_component(crate::manifest::DependencyComponent::Agents)
        && matches!(
            adapter,
            Adapter::Claude | Adapter::Codex | Adapter::Copilot | Adapter::OpenCode
        )
    {
        let known_agent_paths = packages
            .iter()
            .filter(|package| {
                package.selects_component(crate::manifest::DependencyComponent::Agents)
            })
            .flat_map(|package| {
                package
                    .manifest
                    .discovered
                    .selected_agents(adapter)
                    .into_iter()
                    .filter_map(|agent| {
                        managed_artifact_path(
                            names,
                            project_root,
                            adapter,
                            ArtifactKind::Agent,
                            package,
                            &agent.id,
                        )
                    })
            })
            .collect::<std::collections::BTreeSet<_>>();
        let agents_root = runtime_root.join("agents");
        if agents_root.is_dir() {
            for entry in fs::read_dir(&agents_root)
                .with_context(|| format!("failed to read {}", agents_root.display()))?
            {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let managed_path = entry.path();
                if known_agent_paths.contains(&managed_path) {
                    continue;
                }
                let file_name = entry.file_name().to_string_lossy().into_owned();
                let Some(agent_id) = (match adapter {
                    Adapter::Codex => file_name.strip_suffix(".toml"),
                    Adapter::Copilot => file_name.strip_suffix(".agent.md"),
                    Adapter::Claude | Adapter::OpenCode => file_name.strip_suffix(".md"),
                    _ => None,
                }) else {
                    continue;
                };
                mappings.push(file_mapping(
                    managed_path,
                    None,
                    linked_repo.join("agents").join(match adapter {
                        Adapter::Codex => format!("{agent_id}.toml"),
                        Adapter::Claude | Adapter::Copilot | Adapter::OpenCode => {
                            format!("{agent_id}.md")
                        }
                        _ => unreachable!("unsupported agent relay adapter"),
                    }),
                    agent_id.to_string(),
                    RelayTransform::None,
                ));
            }
        }
    }

    Ok(mappings)
}

fn agent_transform(
    names: &ManagedArtifactNames,
    adapter: Adapter,
    package: &ResolvedPackage,
    agent: &crate::manifest::AgentEntry,
) -> RelayTransform {
    match adapter {
        Adapter::Codex if agent.is_toml() => {
            let managed_name = managed_artifact_id(names, package, ArtifactKind::Agent, &agent.id);
            RelayTransform::CodexAgentToml {
                rewritten_name: (managed_name != agent.id).then_some(managed_name),
            }
        }
        Adapter::Codex => RelayTransform::CodexAgentMarkdown {
            runtime_name: managed_artifact_id(names, package, ArtifactKind::Agent, &agent.id),
            description: default_codex_agent_description(&agent.id),
        },
        Adapter::Claude => agent
            .is_toml()
            .then_some(RelayTransform::MarkdownAgentToml {
                adapter_name: "Claude",
            })
            .unwrap_or(RelayTransform::None),
        Adapter::Copilot => agent
            .is_toml()
            .then_some(RelayTransform::MarkdownAgentToml {
                adapter_name: "GitHub Copilot",
            })
            .unwrap_or(RelayTransform::None),
        Adapter::OpenCode => agent
            .is_toml()
            .then_some(RelayTransform::MarkdownAgentToml {
                adapter_name: "OpenCode",
            })
            .unwrap_or(RelayTransform::None),
        Adapter::Agents | Adapter::Cursor => RelayTransform::None,
    }
}

fn skill_mappings(
    names: &ManagedArtifactNames,
    adapter: Adapter,
    package: &ResolvedPackage,
    skill: &SkillEntry,
    source_root: &Path,
    linked_root: &Path,
    managed_root: &Path,
) -> Result<Vec<RelayFileMapping>> {
    let mut mappings = Vec::new();
    for entry in walkdir::WalkDir::new(source_root) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = strip_path_prefix(entry.path(), source_root)
            .with_context(|| format!("failed to make {} relative", entry.path().display()))?;
        let transform = if relative == Path::new("SKILL.md") {
            match adapter {
                Adapter::OpenCode => RelayTransform::OpenCodeSkillName {
                    managed_skill_id: crate::adapters::managed_skill_id(names, package, &skill.id),
                },
                Adapter::Copilot => RelayTransform::CopilotSkillName {
                    managed_skill_id: crate::adapters::managed_skill_id(names, package, &skill.id),
                },
                _ => RelayTransform::None,
            }
        } else {
            RelayTransform::None
        };
        mappings.push(file_mapping(
            managed_root.join(relative),
            Some(source_root.join(relative)),
            linked_root.join(relative),
            skill.id.clone(),
            transform,
        ));
    }
    Ok(mappings)
}

fn missing_skill_mappings(
    names: &ManagedArtifactNames,
    adapter: Adapter,
    package: &ResolvedPackage,
    skill_id: &str,
    managed_root: &Path,
    linked_root: &Path,
) -> Result<Vec<RelayFileMapping>> {
    let mut mappings = Vec::new();
    for entry in walkdir::WalkDir::new(managed_root) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = strip_path_prefix(entry.path(), managed_root)
            .with_context(|| format!("failed to make {} relative", entry.path().display()))?;
        let transform = if relative == Path::new("SKILL.md") {
            match adapter {
                Adapter::OpenCode => RelayTransform::OpenCodeSkillName {
                    managed_skill_id: crate::adapters::managed_skill_id(names, package, skill_id),
                },
                Adapter::Copilot => RelayTransform::CopilotSkillName {
                    managed_skill_id: crate::adapters::managed_skill_id(names, package, skill_id),
                },
                _ => RelayTransform::None,
            }
        } else {
            RelayTransform::None
        };
        mappings.push(file_mapping(
            managed_root.join(relative),
            None,
            linked_root.join(relative),
            skill_id.to_string(),
            transform,
        ));
    }
    Ok(mappings)
}

fn file_mapping(
    managed_path: PathBuf,
    snapshot_path: Option<PathBuf>,
    linked_source_path: PathBuf,
    artifact_id: String,
    transform: RelayTransform,
) -> RelayFileMapping {
    RelayFileMapping {
        managed_path,
        snapshot_path,
        linked_source_path,
        artifact_id,
        transform,
    }
}

impl RelayTransform {
    pub(super) fn to_managed_bytes(&self, source: &[u8]) -> Result<Vec<u8>> {
        match self {
            Self::None => Ok(source.to_vec()),
            Self::OpenCodeSkillName { managed_skill_id } => {
                crate::adapters::opencode::rewrite_skill_name(source, managed_skill_id)
            }
            Self::CopilotSkillName { managed_skill_id } => {
                crate::adapters::copilot::rewrite_skill_name(source, managed_skill_id)
            }
            Self::CodexAgentToml { rewritten_name } => {
                emitted_codex_agent_toml(source, rewritten_name.as_deref(), "Codex agent source")
            }
            Self::CodexAgentMarkdown {
                runtime_name,
                description,
            } => emitted_codex_agent_toml_from_markdown(
                source,
                runtime_name,
                description,
                "Codex agent source",
            ),
            Self::CodexCommandSkill {
                managed_skill_id,
                source_command_id,
            } => crate::adapters::codex::emitted_command_skill_markdown(
                source,
                managed_skill_id,
                source_command_id,
                "Codex command source",
            ),
            Self::MarkdownAgentToml { adapter_name } => {
                markdown_from_codex_agent_toml(source, &format!("{adapter_name} agent source"))
            }
        }
    }

    pub(super) fn to_source_bytes(
        &self,
        managed: &[u8],
        baseline_source: Option<&[u8]>,
        artifact_id: &str,
    ) -> Result<Vec<u8>> {
        match self {
            Self::None => Ok(managed.to_vec()),
            Self::OpenCodeSkillName { managed_skill_id } => baseline_source
                .map(|baseline_source| {
                    restore_rewritten_skill_name(
                        managed,
                        baseline_source,
                        managed_skill_id,
                        artifact_id,
                        "OpenCode",
                    )
                })
                .unwrap_or_else(|| {
                    restore_skill_name_without_baseline(managed, artifact_id, "OpenCode")
                }),
            Self::CopilotSkillName { managed_skill_id } => baseline_source
                .map(|baseline_source| {
                    restore_rewritten_skill_name(
                        managed,
                        baseline_source,
                        managed_skill_id,
                        artifact_id,
                        "GitHub Copilot",
                    )
                })
                .unwrap_or_else(|| {
                    restore_skill_name_without_baseline(managed, artifact_id, "GitHub Copilot")
                }),
            Self::CodexAgentToml { rewritten_name } => {
                if let Some(rewritten_name) = rewritten_name.as_deref() {
                    source_toml_from_managed_codex(
                        managed,
                        baseline_source,
                        rewritten_name,
                        "Codex agent source",
                    )
                } else {
                    Ok(managed.to_vec())
                }
            }
            Self::CodexAgentMarkdown { .. } => {
                markdown_from_codex_agent_toml(managed, "Codex agent source")
            }
            Self::CodexCommandSkill {
                managed_skill_id, ..
            } => crate::adapters::codex::command_body_from_synthetic_skill(
                managed,
                managed_skill_id,
                "Codex command source",
            ),
            Self::MarkdownAgentToml { adapter_name } => baseline_source
                .map(|baseline_source| {
                    source_toml_from_managed_markdown(
                        managed,
                        baseline_source,
                        &format!("{adapter_name} agent source"),
                    )
                })
                .unwrap_or_else(|| {
                    bail!("{adapter_name} agent relay needs a TOML source baseline to write back")
                }),
        }
    }
}

pub(super) fn restore_rewritten_skill_name(
    managed: &[u8],
    baseline_source: &[u8],
    managed_skill_id: &str,
    artifact_id: &str,
    adapter_name: &str,
) -> Result<Vec<u8>> {
    let managed = String::from_utf8(managed.to_vec())
        .with_context(|| format!("{adapter_name} managed skills must be UTF-8"))?;
    let baseline_source = String::from_utf8(baseline_source.to_vec())
        .with_context(|| format!("{adapter_name} source skills must be UTF-8"))?;
    let restored_name = extract_frontmatter_name(&baseline_source, artifact_id, adapter_name)?;
    let mut lines = split_lines_preserving_endings(&managed);
    rewrite_or_insert_skill_name(
        &mut lines,
        &restored_name,
        Some(managed_skill_id),
        adapter_name,
    )?;
    Ok(lines.concat().into_bytes())
}

fn extract_frontmatter_name(
    contents: &str,
    fallback_name: &str,
    adapter_name: &str,
) -> Result<String> {
    let lines = contents.lines().collect::<Vec<_>>();
    if lines.first().copied() != Some("---") {
        bail!("{adapter_name} skill is missing YAML frontmatter");
    }
    let Some(frontmatter_end) = lines.iter().skip(1).position(|line| *line == "---") else {
        bail!("{adapter_name} skill is missing a closing frontmatter fence");
    };
    let frontmatter_end = frontmatter_end + 1;
    for line in lines.iter().take(frontmatter_end) {
        if let Some(value) = line.trim_start().strip_prefix("name:") {
            let value = value.trim();
            return Ok(if value.is_empty() {
                fallback_name.to_string()
            } else {
                value.to_string()
            });
        }
    }
    Ok(fallback_name.to_string())
}

fn restore_skill_name_without_baseline(
    managed: &[u8],
    artifact_id: &str,
    adapter_name: &str,
) -> Result<Vec<u8>> {
    let managed = String::from_utf8(managed.to_vec())
        .with_context(|| format!("{adapter_name} managed skills must be UTF-8"))?;
    let mut lines = split_lines_preserving_endings(&managed);
    rewrite_or_insert_skill_name(&mut lines, artifact_id, None, adapter_name)?;
    Ok(lines.concat().into_bytes())
}

fn split_lines_preserving_endings(contents: &str) -> Vec<String> {
    if contents.is_empty() {
        Vec::new()
    } else {
        contents.split_inclusive('\n').map(str::to_string).collect()
    }
}

fn trim_line_ending(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

fn rewrite_or_insert_skill_name(
    lines: &mut Vec<String>,
    name: &str,
    managed_skill_id: Option<&str>,
    adapter_name: &str,
) -> Result<()> {
    let frontmatter_end = frontmatter_end(lines, adapter_name)?;
    let name_index = managed_skill_id
        .and_then(|managed_skill_id| {
            lines.iter().take(frontmatter_end).position(|line| {
                trim_line_ending(line).trim_start() == format!("name: {managed_skill_id}")
            })
        })
        .or_else(|| {
            lines
                .iter()
                .take(frontmatter_end)
                .position(|line| trim_line_ending(line).trim_start().starts_with("name:"))
        });

    if let Some(index) = name_index {
        lines[index] = rewrite_frontmatter_name_line(&lines[index], name);
    } else {
        lines.insert(
            frontmatter_end,
            inserted_frontmatter_name_line(lines, frontmatter_end, name),
        );
    }

    Ok(())
}

fn frontmatter_end(lines: &[String], adapter_name: &str) -> Result<usize> {
    if lines.first().map(|line| trim_line_ending(line)) != Some("---") {
        bail!("{adapter_name} skill is missing YAML frontmatter");
    }
    let Some(frontmatter_end) = lines
        .iter()
        .skip(1)
        .position(|line| trim_line_ending(line) == "---")
    else {
        bail!("{adapter_name} skill is missing a closing frontmatter fence");
    };
    Ok(frontmatter_end + 1)
}

fn inserted_frontmatter_name_line(lines: &[String], frontmatter_end: usize, name: &str) -> String {
    format!(
        "name: {name}{}",
        preferred_line_ending(lines, frontmatter_end)
    )
}

fn preferred_line_ending(lines: &[String], anchor: usize) -> &str {
    line_ending(lines.get(anchor).map(String::as_str).unwrap_or_default())
        .or_else(|| {
            anchor
                .checked_sub(1)
                .and_then(|index| lines.get(index))
                .and_then(|line| line_ending(line))
        })
        .unwrap_or("\n")
}

fn line_ending(line: &str) -> Option<&str> {
    if line.ends_with("\r\n") {
        Some("\r\n")
    } else if line.ends_with('\n') {
        Some("\n")
    } else {
        None
    }
}

fn rewrite_frontmatter_name_line(line: &str, name: &str) -> String {
    let leading = line
        .chars()
        .take_while(|character| character.is_ascii_whitespace())
        .collect::<String>();
    let newline = if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    };

    format!("{leading}name: {name}{newline}")
}
