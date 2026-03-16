use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::resolver::{PackageSource, ResolvedPackage};

pub mod claude;
pub mod codex;
pub mod opencode;

#[derive(Debug, Clone)]
pub struct ManagedFile {
    pub path: PathBuf,
    pub contents: Vec<u8>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Adapter {
    #[value(name = "claude")]
    Claude,
    #[value(name = "codex")]
    Codex,
    #[value(name = "opencode", alias = "open-code")]
    OpenCode,
}

impl Adapter {
    pub const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::OpenCode];

    const fn bit(self) -> u8 {
        match self {
            Self::Claude => 1 << 0,
            Self::Codex => 1 << 1,
            Self::OpenCode => 1 << 2,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Adapters(u8);

impl Adapters {
    #[allow(dead_code)]
    pub const NONE: Self = Self(0);
    pub const ALL: Self = Self(Self::CLAUDE.0 | Self::CODEX.0 | Self::OPENCODE.0);
    pub const CLAUDE: Self = Self(Adapter::Claude.bit());
    pub const CODEX: Self = Self(Adapter::Codex.bit());
    pub const OPENCODE: Self = Self(Adapter::OpenCode.bit());

    pub const fn contains(self, adapter: Adapter) -> bool {
        self.0 & adapter.bit() != 0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[allow(dead_code)]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn from_slice(adapters: &[Adapter]) -> Self {
        adapters
            .iter()
            .copied()
            .fold(Self::NONE, |selected, adapter| {
                selected.union(adapter.into())
            })
    }

    pub fn to_vec(self) -> Vec<Adapter> {
        self.iter().collect()
    }

    #[allow(dead_code)]
    pub fn iter(self) -> impl Iterator<Item = Adapter> {
        Adapter::ALL
            .into_iter()
            .filter(move |adapter| self.contains(*adapter))
    }
}

impl From<Adapter> for Adapters {
    fn from(value: Adapter) -> Self {
        Self(value.bit())
    }
}

impl std::fmt::Display for Adapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    Skill,
    Agent,
    Rule,
    Command,
}

impl ArtifactKind {
    pub const fn supported_adapters(self) -> Adapters {
        match self {
            Self::Skill => Adapters::CLAUDE
                .union(Adapters::CODEX)
                .union(Adapters::OPENCODE),
            Self::Agent => Adapters::CLAUDE.union(Adapters::OPENCODE),
            Self::Rule => Adapters::CLAUDE
                .union(Adapters::CODEX)
                .union(Adapters::OPENCODE),
            Self::Command => Adapters::CLAUDE.union(Adapters::OPENCODE),
        }
    }

    pub const fn plural_name(self) -> &'static str {
        match self {
            Self::Skill => "skills",
            Self::Agent => "agents",
            Self::Rule => "rules",
            Self::Command => "commands",
        }
    }
}

#[derive(Debug, Default)]
pub struct OutputPlan {
    pub files: Vec<ManagedFile>,
    pub managed_files: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default)]
struct OutputAccumulator {
    files: BTreeMap<PathBuf, Vec<u8>>,
    managed_files: BTreeSet<String>,
    warnings: Vec<String>,
    opencode_skill_owners: BTreeMap<String, String>,
}

pub fn namespaced_skill_id(package: &ResolvedPackage, skill_id: &str) -> String {
    format!("{skill_id}_{}", package_short_id(package))
}

pub fn package_short_id(package: &ResolvedPackage) -> String {
    match &package.source {
        PackageSource::Git { rev, .. } => short_source_id(rev),
        PackageSource::Path { .. } | PackageSource::Root => short_source_id(
            package
                .digest
                .strip_prefix("sha256:")
                .unwrap_or(&package.digest),
        ),
    }
}

pub fn short_source_id(value: &str) -> String {
    let short = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(6)
        .collect::<String>()
        .to_ascii_lowercase();

    if short.is_empty() {
        "local0".into()
    } else {
        short
    }
}

pub fn build_output_plan(
    project_root: &Path,
    packages: &[(ResolvedPackage, PathBuf)],
    selected_adapters: Adapters,
) -> Result<OutputPlan> {
    let mut plan = OutputAccumulator::default();

    for (package, snapshot_root) in packages {
        warn_if_unsupported(
            &mut plan.warnings,
            package,
            ArtifactKind::Skill,
            package.manifest.discovered.skills.len(),
        );
        warn_if_unsupported(
            &mut plan.warnings,
            package,
            ArtifactKind::Agent,
            package.manifest.discovered.agents.len(),
        );
        warn_if_unsupported(
            &mut plan.warnings,
            package,
            ArtifactKind::Rule,
            package.manifest.discovered.rules.len(),
        );
        warn_if_unsupported(
            &mut plan.warnings,
            package,
            ArtifactKind::Command,
            package.manifest.discovered.commands.len(),
        );

        for skill in &package.manifest.discovered.skills {
            if selected_adapters.contains(Adapter::Claude)
                && ArtifactKind::Skill
                    .supported_adapters()
                    .contains(Adapter::Claude)
            {
                merge_files(
                    &mut plan.files,
                    claude::skill_files(project_root, package, snapshot_root, skill)?,
                )?;
                plan.managed_files
                    .insert(format!(".claude/skills/{}", skill.id));
            }

            if selected_adapters.contains(Adapter::Codex)
                && ArtifactKind::Skill
                    .supported_adapters()
                    .contains(Adapter::Codex)
            {
                merge_files(
                    &mut plan.files,
                    codex::skill_files(project_root, package, snapshot_root, skill)?,
                )?;
                plan.managed_files
                    .insert(format!(".codex/skills/{}", skill.id));
            }

            if selected_adapters.contains(Adapter::OpenCode)
                && ArtifactKind::Skill
                    .supported_adapters()
                    .contains(Adapter::OpenCode)
            {
                claim_opencode_skill_id(&mut plan.opencode_skill_owners, package, &skill.id)?;
                merge_files(
                    &mut plan.files,
                    opencode::skill_files(project_root, snapshot_root, skill)?,
                )?;
                plan.managed_files
                    .insert(format!(".opencode/skills/{}", skill.id));
            }
        }

        for agent in &package.manifest.discovered.agents {
            if selected_adapters.contains(Adapter::Claude)
                && ArtifactKind::Agent
                    .supported_adapters()
                    .contains(Adapter::Claude)
            {
                merge_file(
                    &mut plan.files,
                    claude::agent_file(project_root, snapshot_root, agent)?,
                )?;
                plan.managed_files
                    .insert(format!(".claude/agents/{}.md", agent.id));
            }

            if selected_adapters.contains(Adapter::OpenCode)
                && ArtifactKind::Agent
                    .supported_adapters()
                    .contains(Adapter::OpenCode)
            {
                merge_file(
                    &mut plan.files,
                    opencode::agent_file(project_root, snapshot_root, agent)?,
                )?;
                plan.managed_files
                    .insert(format!(".opencode/agents/{}.md", agent.id));
            }
        }

        for rule in &package.manifest.discovered.rules {
            if selected_adapters.contains(Adapter::Claude)
                && ArtifactKind::Rule
                    .supported_adapters()
                    .contains(Adapter::Claude)
            {
                merge_file(
                    &mut plan.files,
                    claude::rule_file(project_root, snapshot_root, rule)?,
                )?;
                plan.managed_files
                    .insert(format!(".claude/rules/{}.md", rule.id));
            }

            if selected_adapters.contains(Adapter::Codex)
                && ArtifactKind::Rule
                    .supported_adapters()
                    .contains(Adapter::Codex)
            {
                merge_file(
                    &mut plan.files,
                    codex::rule_file(project_root, snapshot_root, rule)?,
                )?;
                plan.managed_files
                    .insert(format!(".codex/rules/{}.rules", rule.id));
            }

            if selected_adapters.contains(Adapter::OpenCode)
                && ArtifactKind::Rule
                    .supported_adapters()
                    .contains(Adapter::OpenCode)
            {
                merge_file(
                    &mut plan.files,
                    opencode::rule_file(project_root, snapshot_root, rule)?,
                )?;
                plan.managed_files
                    .insert(format!(".opencode/rules/{}.md", rule.id));
            }
        }

        for command in &package.manifest.discovered.commands {
            if selected_adapters.contains(Adapter::Claude)
                && ArtifactKind::Command
                    .supported_adapters()
                    .contains(Adapter::Claude)
            {
                merge_file(
                    &mut plan.files,
                    claude::command_file(project_root, snapshot_root, command)?,
                )?;
                plan.managed_files
                    .insert(format!(".claude/commands/{}.md", command.id));
            }

            if selected_adapters.contains(Adapter::OpenCode)
                && ArtifactKind::Command
                    .supported_adapters()
                    .contains(Adapter::OpenCode)
            {
                merge_file(
                    &mut plan.files,
                    opencode::command_file(project_root, snapshot_root, command)?,
                )?;
                plan.managed_files
                    .insert(format!(".opencode/commands/{}.md", command.id));
            }
        }
    }

    Ok(OutputPlan {
        files: plan
            .files
            .into_iter()
            .map(|(path, contents)| ManagedFile { path, contents })
            .collect(),
        managed_files: plan.managed_files.into_iter().collect(),
        warnings: plan.warnings,
    })
}

fn warn_if_unsupported(
    warnings: &mut Vec<String>,
    package: &ResolvedPackage,
    kind: ArtifactKind,
    count: usize,
) {
    if count == 0 || !kind.supported_adapters().is_empty() {
        return;
    }

    warnings.push(format!(
        "package `{}` discovered {} {} but no adapters support them",
        package.alias,
        count,
        kind.plural_name()
    ));
}

fn claim_opencode_skill_id(
    owners: &mut BTreeMap<String, String>,
    package: &ResolvedPackage,
    skill_id: &str,
) -> Result<()> {
    match owners.get(skill_id) {
        Some(existing) if existing != &package.alias => bail!(
            "multiple packages export OpenCode skill `{skill_id}` (`{existing}` and `{}`)",
            package.alias
        ),
        Some(_) => Ok(()),
        None => {
            owners.insert(skill_id.to_string(), package.alias.clone());
            Ok(())
        }
    }
}

fn merge_files(target: &mut BTreeMap<PathBuf, Vec<u8>>, files: Vec<ManagedFile>) -> Result<()> {
    for file in files {
        merge_file(target, file)?;
    }
    Ok(())
}

fn merge_file(target: &mut BTreeMap<PathBuf, Vec<u8>>, file: ManagedFile) -> Result<()> {
    match target.get(&file.path) {
        Some(existing) if existing != &file.contents => {
            bail!("multiple packages want to manage {}", file.path.display());
        }
        Some(_) => {}
        None => {
            target.insert(file.path, file.contents);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_kind_support_matrix_matches_supported_adapters() {
        let skill = ArtifactKind::Skill.supported_adapters();
        assert!(skill.intersects(Adapters::CLAUDE));
        assert!(skill.contains(Adapter::Claude));
        assert!(skill.contains(Adapter::Codex));
        assert!(skill.contains(Adapter::OpenCode));
        assert_eq!(skill.iter().count(), 3);

        let agent = ArtifactKind::Agent.supported_adapters();
        assert!(agent.contains(Adapter::Claude));
        assert!(!agent.contains(Adapter::Codex));
        assert!(agent.contains(Adapter::OpenCode));

        let rule = ArtifactKind::Rule.supported_adapters();
        assert!(rule.contains(Adapter::Claude));
        assert!(rule.contains(Adapter::Codex));
        assert!(rule.contains(Adapter::OpenCode));

        let command = ArtifactKind::Command.supported_adapters();
        assert!(command.contains(Adapter::Claude));
        assert!(!command.contains(Adapter::Codex));
        assert!(command.contains(Adapter::OpenCode));

        assert!(Adapters::NONE.is_empty());
    }
}
