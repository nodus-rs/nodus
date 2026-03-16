use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
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
}

pub fn namespaced_skill_id(package: &ResolvedPackage, skill_id: &str) -> String {
    namespaced_artifact_id(package, skill_id)
}

pub fn namespaced_artifact_id(package: &ResolvedPackage, artifact_id: &str) -> String {
    format!("{artifact_id}_{}", package_short_id(package))
}

pub fn namespaced_file_name(
    package: &ResolvedPackage,
    artifact_id: &str,
    extension: &str,
) -> String {
    format!(
        "{}.{}",
        namespaced_artifact_id(package, artifact_id),
        extension.trim_start_matches('.')
    )
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
        if matches!(package.source, PackageSource::Root) {
            continue;
        }

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
                merge_files(
                    &mut plan.files,
                    opencode::skill_files(project_root, package, snapshot_root, skill)?,
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
                    claude::agent_file(project_root, package, snapshot_root, agent)?,
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
                    opencode::agent_file(project_root, package, snapshot_root, agent)?,
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
                    claude::rule_file(project_root, package, snapshot_root, rule)?,
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
                    codex::rule_file(project_root, package, snapshot_root, rule)?,
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
                    opencode::rule_file(project_root, package, snapshot_root, rule)?,
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
                    claude::command_file(project_root, package, snapshot_root, command)?,
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
                    opencode::command_file(project_root, package, snapshot_root, command)?,
                )?;
                plan.managed_files
                    .insert(format!(".opencode/commands/{}.md", command.id));
            }
        }
    }

    for file in gitignore_files(project_root, &plan.files)? {
        plan.managed_files
            .insert(display_relative(project_root, &file.path));
        merge_file(&mut plan.files, file)?;
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

fn gitignore_files(
    project_root: &Path,
    files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<Vec<ManagedFile>> {
    let mut entries = BTreeMap::<PathBuf, BTreeSet<String>>::new();

    for path in files.keys() {
        let Some((root, pattern)) = gitignore_entry(project_root, path)? else {
            continue;
        };
        entries.entry(root).or_default().insert(pattern);
    }

    Ok(entries
        .into_iter()
        .map(|(root, patterns)| ManagedFile {
            path: root.join(".gitignore"),
            contents: render_gitignore(&patterns).into_bytes(),
        })
        .collect())
}

fn gitignore_entry(project_root: &Path, path: &Path) -> Result<Option<(PathBuf, String)>> {
    let relative = path
        .strip_prefix(project_root)
        .with_context(|| format!("failed to make {} relative", path.display()))?;
    let components = relative
        .iter()
        .map(|component| component.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    let [runtime, rest @ ..] = components.as_slice() else {
        return Ok(None);
    };
    if !matches!(runtime.as_str(), ".claude" | ".codex" | ".opencode") {
        return Ok(None);
    }
    if rest.is_empty() {
        return Ok(None);
    }
    if rest == [".gitignore"] {
        return Ok(None);
    }

    let pattern = if rest.len() >= 2 {
        managed_artifact_gitignore_pattern(runtime, &rest[0], &rest[1])
    } else {
        rest.join("/")
    };

    Ok(Some((project_root.join(runtime), pattern)))
}

fn managed_artifact_gitignore_pattern(
    runtime: &str,
    artifact_dir: &str,
    artifact_name: &str,
) -> String {
    if artifact_dir == "skills"
        && matches!(runtime, ".claude" | ".codex" | ".opencode")
        && let Some((_, suffix)) = artifact_name.rsplit_once('_')
        && !suffix.is_empty()
    {
        return format!("skills/*_{suffix}/");
    }

    if matches!(runtime, ".claude" | ".codex" | ".opencode")
        && matches!(artifact_dir, "agents" | "commands" | "rules")
        && let Some((stem, extension)) = artifact_name.rsplit_once('.')
        && let Some((_, suffix)) = stem.rsplit_once('_')
        && !suffix.is_empty()
    {
        return format!("{artifact_dir}/*_{suffix}.{extension}");
    }

    format!("{artifact_dir}/{artifact_name}")
}

fn render_gitignore(patterns: &BTreeSet<String>) -> String {
    let mut output = String::from("# Managed by nodus\n.gitignore\n");
    for pattern in patterns {
        output.push_str(pattern);
        output.push('\n');
    }
    output
}

fn display_relative(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
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
