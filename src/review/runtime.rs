use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use clap::ValueEnum;
use mentra::agent::{
    AgentConfig, ContextCompactionConfig, MemoryConfig, TaskConfig, TeamConfig, ToolProfile,
    WorkspaceConfig,
};
use mentra::provider::{BuiltinProvider, ModelInfo, ModelSelector};
use mentra::runtime::{RuntimeError, RuntimePolicy, SqliteRuntimeStore};
use mentra::tool::{ToolAuthorizationDecision, ToolAuthorizationRequest, ToolAuthorizer};
use mentra::{ContentBlock, Runtime};
use tempfile::TempDir;

use crate::git::{ensure_git_dependency, normalize_alias_from_url, normalize_git_url};
use crate::manifest::{
    DependencyComponent, DependencyKind, DependencySourceKind, DependencySpec, LoadedManifest,
    PackageRole, RequestedGitRef as ManifestRequestedGitRef, load_dependency_from_dir,
    load_root_from_dir, normalize_dependency_alias,
};
use crate::report::Reporter;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReviewProvider {
    #[value(name = "openai")]
    Openai,
    #[value(name = "anthropic")]
    Anthropic,
    #[value(name = "gemini")]
    Gemini,
}

impl ReviewProvider {
    fn builtin(self) -> BuiltinProvider {
        match self {
            Self::Openai => BuiltinProvider::OpenAI,
            Self::Anthropic => BuiltinProvider::Anthropic,
            Self::Gemini => BuiltinProvider::Gemini,
        }
    }

    fn api_key_env(self) -> &'static str {
        match self {
            Self::Openai => "OPENAI_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::Gemini => "GEMINI_API_KEY",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Openai => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::Gemini => "Gemini",
        }
    }
}

impl std::fmt::Display for ReviewProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.display_name())
    }
}

#[derive(Debug, Clone)]
pub struct ReviewSummary {
    pub package_count: usize,
    pub provider: ReviewProvider,
}

#[derive(Debug, Clone)]
struct ReviewScope {
    target_index: usize,
    packages: Vec<ReviewPackage>,
}

#[derive(Debug, Clone)]
struct ReviewPackage {
    aliases: BTreeSet<String>,
    root: PathBuf,
    manifest: LoadedManifest,
    source: ReviewSource,
    selected_components: Option<Vec<DependencyComponent>>,
    dependencies: Vec<ReviewDependency>,
}

#[derive(Debug, Clone)]
struct ReviewDependency {
    alias: String,
    kind: DependencyKind,
    package_index: usize,
}

#[derive(Debug, Clone)]
enum ReviewSource {
    Path {
        path: PathBuf,
    },
    Git {
        url: String,
        tag: Option<String>,
        branch: Option<String>,
        rev: String,
    },
}

#[derive(Debug, Clone)]
struct ResolvedReviewTarget {
    alias: String,
    manifest: LoadedManifest,
    source: ReviewSource,
    selected_components: Option<Vec<DependencyComponent>>,
    role: PackageRole,
}

#[derive(Default)]
struct ReviewCollector {
    packages: Vec<ReviewPackage>,
    index_by_root: BTreeMap<PathBuf, usize>,
    active_roots: BTreeSet<PathBuf>,
}

struct ReviewSessionPaths {
    _temp: TempDir,
    store_path: PathBuf,
    team_dir: PathBuf,
    tasks_dir: PathBuf,
    transcript_dir: PathBuf,
}

struct ReadOnlyReviewAuthorizer;

pub struct ReviewRequest<'a> {
    pub package: &'a str,
    pub tag: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub provider: ReviewProvider,
    pub model: Option<&'a str>,
}

pub fn review_package_in_dir(
    cwd: &Path,
    cache_root: &Path,
    request: ReviewRequest<'_>,
    reporter: &Reporter,
) -> Result<ReviewSummary> {
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        bail!("`nodus review` is currently supported only on macOS and Linux");
    }

    reporter.status(
        "Collecting",
        format!("package graph for {}", request.package),
    )?;
    let scope = collect_review_scope(
        cwd,
        cache_root,
        request.package,
        request.tag,
        request.branch,
        reporter,
    )?;
    reporter.status(
        "Preparing",
        format!(
            "{} package{} in review scope",
            scope.packages.len(),
            if scope.packages.len() == 1 { "" } else { "s" }
        ),
    )?;
    let package_count = scope.packages.len();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start review runtime")?;
    runtime.block_on(run_review(scope, request.provider, request.model, reporter))?;

    Ok(ReviewSummary {
        package_count,
        provider: request.provider,
    })
}

async fn run_review(
    scope: ReviewScope,
    provider: ReviewProvider,
    model: Option<&str>,
    reporter: &Reporter,
) -> Result<()> {
    let session = ReviewSessionPaths::new().context("failed to prepare review session storage")?;
    let api_key = std::env::var(provider.api_key_env()).with_context(|| {
        format!(
            "{} is required to run `nodus review --provider {}`",
            provider.api_key_env(),
            provider.to_string().to_ascii_lowercase()
        )
    })?;
    let runtime = build_review_runtime(&scope, &session, provider, api_key)?;

    reporter.status("Resolving", format!("{provider} model"))?;
    let model = resolve_review_model(&runtime, provider, model).await?;
    reporter.status(
        "Reviewing",
        format!("{} with {}", scope.target_alias(), model.id),
    )?;
    let review = execute_review_with_runtime(&runtime, model, &scope, &session).await?;
    reporter.line(review)?;

    Ok(())
}

fn build_review_runtime(
    scope: &ReviewScope,
    session: &ReviewSessionPaths,
    provider: ReviewProvider,
    api_key: String,
) -> Result<Runtime> {
    let mut policy = RuntimePolicy::default();
    for root in scope.allowed_read_roots() {
        policy = policy.with_allowed_read_root(root);
    }

    Runtime::builder()
        .with_store(SqliteRuntimeStore::new(session.store_path.clone()))
        .with_provider(provider.builtin(), api_key)
        .with_policy(policy)
        .with_tool_authorizer(ReadOnlyReviewAuthorizer)
        .with_runtime_identifier(format!(
            "nodus-review-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
        .build()
        .map_err(Into::into)
}

async fn resolve_review_model(
    runtime: &Runtime,
    provider: ReviewProvider,
    model: Option<&str>,
) -> Result<ModelInfo> {
    let selector = model
        .map(|value| ModelSelector::Id(value.to_string()))
        .or_else(|| std::env::var("MENTRA_MODEL").ok().map(ModelSelector::Id))
        .unwrap_or(ModelSelector::NewestAvailable);
    runtime
        .resolve_model(provider.builtin(), selector)
        .await
        .map_err(Into::into)
}

async fn execute_review_with_runtime(
    runtime: &Runtime,
    model: ModelInfo,
    scope: &ReviewScope,
    session: &ReviewSessionPaths,
) -> Result<String> {
    let target = scope.target();
    let config = AgentConfig {
        system: Some(REVIEW_SYSTEM_PROMPT.to_string()),
        tool_profile: ToolProfile::only(["files"]),
        workspace: WorkspaceConfig {
            base_dir: target.root.clone(),
            auto_route_shell: false,
        },
        memory: MemoryConfig {
            auto_recall_enabled: false,
            write_tools_enabled: false,
            ..MemoryConfig::default()
        },
        team: TeamConfig {
            team_dir: session.team_dir.clone(),
            ..TeamConfig::default()
        },
        task: TaskConfig {
            tasks_dir: session.tasks_dir.clone(),
            ..TaskConfig::default()
        },
        context_compaction: ContextCompactionConfig {
            transcript_dir: session.transcript_dir.clone(),
            ..ContextCompactionConfig::default()
        },
        ..AgentConfig::default()
    };
    let mut agent = runtime
        .spawn_with_config("Nodus Review Agent", model, config)
        .map_err(Into::<anyhow::Error>::into)?;
    let response = agent
        .send(vec![ContentBlock::text(build_review_prompt(scope))])
        .await
        .map_err(Into::<anyhow::Error>::into)?;
    let text = response.text().trim().to_string();
    if text.is_empty() {
        bail!("review model returned an empty response");
    }
    Ok(text)
}

fn collect_review_scope(
    cwd: &Path,
    cache_root: &Path,
    package: &str,
    tag: Option<&str>,
    branch: Option<&str>,
    reporter: &Reporter,
) -> Result<ReviewScope> {
    let target = resolve_review_target(cwd, cache_root, package, tag, branch, reporter)?;
    let mut collector = ReviewCollector::default();
    let target_index = collector.collect(target, cache_root, reporter)?;
    Ok(ReviewScope {
        target_index,
        packages: collector.packages,
    })
}

fn resolve_review_target(
    cwd: &Path,
    cache_root: &Path,
    package: &str,
    tag: Option<&str>,
    branch: Option<&str>,
    reporter: &Reporter,
) -> Result<ResolvedReviewTarget> {
    let trimmed = package.trim();
    if trimmed.is_empty() {
        bail!("package must not be empty");
    }

    if let Some((alias, dependency, root_manifest)) = resolve_direct_dependency(cwd, trimmed)? {
        if tag.is_some() || branch.is_some() {
            bail!(
                "`--tag` and `--branch` can only be used when reviewing a direct package reference"
            );
        }
        return resolve_from_dependency_spec(
            &alias,
            &dependency,
            &root_manifest,
            cache_root,
            reporter,
        );
    }

    if let Some(package_root) = resolve_local_package_path(cwd, trimmed)? {
        if tag.is_some() || branch.is_some() {
            bail!("`--tag` and `--branch` cannot be used when reviewing a local package path");
        }
        let (manifest, role) = load_review_manifest_for_inspection(&package_root)?;
        return Ok(ResolvedReviewTarget {
            alias: normalize_dependency_alias(&manifest.effective_name())?,
            manifest,
            source: ReviewSource::Path { path: package_root },
            selected_components: None,
            role,
        });
    }

    let normalized_url = normalize_git_url(trimmed);
    let alias = normalize_alias_from_url(&normalized_url)?;
    let checkout = ensure_git_dependency(
        cache_root,
        &normalized_url,
        match (tag, branch) {
            (Some(tag), None) => Some(ManifestRequestedGitRef::Tag(tag)),
            (None, Some(branch)) => Some(ManifestRequestedGitRef::Branch(branch)),
            (None, None) => None,
            _ => bail!("git dependency must not request both `tag` and `branch`"),
        },
        true,
        reporter,
    )?;
    let (manifest, role) = load_review_manifest_for_inspection(&checkout.path)
        .with_context(|| format!("dependency `{alias}` does not match the Nodus package layout"))?;

    Ok(ResolvedReviewTarget {
        alias,
        manifest,
        source: ReviewSource::Git {
            url: checkout.url,
            tag: checkout.tag,
            branch: checkout.branch,
            rev: checkout.rev,
        },
        selected_components: None,
        role,
    })
}

fn resolve_direct_dependency(
    cwd: &Path,
    package: &str,
) -> Result<Option<(String, DependencySpec, LoadedManifest)>> {
    let root_manifest = load_root_from_dir(cwd)?;
    if let Some(entry) = root_manifest.manifest.get_dependency(package) {
        return Ok(Some((
            package.to_string(),
            entry.spec.clone(),
            root_manifest,
        )));
    }

    let normalized = match normalize_alias_from_url(package) {
        Ok(alias) => alias,
        Err(_) => return Ok(None),
    };
    let Some(entry) = root_manifest.manifest.get_dependency(&normalized) else {
        return Ok(None);
    };
    Ok(Some((normalized, entry.spec.clone(), root_manifest)))
}

fn resolve_local_package_path(cwd: &Path, package: &str) -> Result<Option<PathBuf>> {
    let candidate = Path::new(package);
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    };
    if !candidate.exists() {
        return Ok(None);
    }

    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("failed to access {}", candidate.display()))?;
    if !canonical.is_dir() {
        bail!("package path {} must be a directory", canonical.display());
    }
    Ok(Some(canonical))
}

fn load_review_manifest_for_inspection(root: &Path) -> Result<(LoadedManifest, PackageRole)> {
    match load_root_from_dir(root) {
        Ok(manifest) => Ok((manifest, PackageRole::Root)),
        Err(_) => {
            load_dependency_from_dir(root).map(|manifest| (manifest, PackageRole::Dependency))
        }
    }
}

fn resolve_from_dependency_spec(
    alias: &str,
    dependency: &DependencySpec,
    root_manifest: &LoadedManifest,
    cache_root: &Path,
    reporter: &Reporter,
) -> Result<ResolvedReviewTarget> {
    match dependency.source_kind()? {
        DependencySourceKind::Path => {
            let declared_path = dependency
                .path
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("dependency `{alias}` must declare `path`"))?;
            let package_root = root_manifest.resolve_path(declared_path)?;
            let manifest = load_dependency_from_dir(&package_root)?;
            Ok(ResolvedReviewTarget {
                alias: alias.to_string(),
                manifest,
                source: ReviewSource::Path { path: package_root },
                selected_components: dependency.effective_selected_components(),
                role: PackageRole::Dependency,
            })
        }
        DependencySourceKind::Git => {
            let url = dependency.resolved_git_url()?;
            let checkout = ensure_git_dependency(
                cache_root,
                &url,
                Some(dependency.requested_git_ref()?),
                true,
                reporter,
            )?;
            let manifest = load_dependency_from_dir(&checkout.path).with_context(|| {
                format!("dependency `{alias}` does not match the Nodus package layout")
            })?;
            Ok(ResolvedReviewTarget {
                alias: alias.to_string(),
                manifest,
                source: ReviewSource::Git {
                    url: checkout.url,
                    tag: checkout.tag,
                    branch: checkout.branch,
                    rev: checkout.rev,
                },
                selected_components: dependency.effective_selected_components(),
                role: PackageRole::Dependency,
            })
        }
    }
}

impl ReviewCollector {
    fn collect(
        &mut self,
        target: ResolvedReviewTarget,
        cache_root: &Path,
        reporter: &Reporter,
    ) -> Result<usize> {
        let root = target.manifest.root.clone();
        if self.active_roots.contains(&root) {
            bail!(
                "dependency cycle detected while reviewing {}",
                root.display()
            );
        }
        if let Some(index) = self.index_by_root.get(&root).copied() {
            self.packages[index].aliases.insert(target.alias);
            return Ok(index);
        }

        let index = self.packages.len();
        let mut aliases = BTreeSet::new();
        aliases.insert(target.alias.clone());
        self.index_by_root.insert(root.clone(), index);
        self.active_roots.insert(root.clone());
        self.packages.push(ReviewPackage {
            aliases,
            root: root.clone(),
            manifest: target.manifest,
            source: target.source,
            selected_components: target.selected_components,
            dependencies: Vec::new(),
        });

        let dependencies = self.packages[index]
            .manifest
            .manifest
            .active_dependency_entries_for_role(target.role)
            .into_iter()
            .map(|entry| (entry.alias.to_string(), entry.kind, entry.spec.clone()))
            .collect::<Vec<_>>();
        for (alias, kind, spec) in dependencies {
            let resolved = resolve_from_dependency_spec(
                &alias,
                &spec,
                &self.packages[index].manifest,
                cache_root,
                reporter,
            )?;
            let dependency_index = self.collect(resolved, cache_root, reporter)?;
            self.packages[index].dependencies.push(ReviewDependency {
                alias,
                kind,
                package_index: dependency_index,
            });
        }

        self.active_roots.remove(&root);
        Ok(index)
    }
}

impl ReviewScope {
    fn target(&self) -> &ReviewPackage {
        &self.packages[self.target_index]
    }

    fn target_alias(&self) -> String {
        self.target()
            .aliases
            .iter()
            .next()
            .cloned()
            .unwrap_or_else(|| "package".into())
    }

    fn allowed_read_roots(&self) -> Vec<PathBuf> {
        self.packages
            .iter()
            .map(|package| package.root.clone())
            .collect()
    }
}

impl ReviewPackage {
    fn primary_alias(&self) -> &str {
        self.aliases
            .iter()
            .next()
            .map(String::as_str)
            .unwrap_or("package")
    }
}

impl ReviewDependency {
    fn display_alias(&self) -> String {
        if self.kind.is_dev() {
            format!("{} (dev)", self.alias)
        } else {
            self.alias.clone()
        }
    }
}

impl ReviewSource {
    fn describe(&self) -> String {
        match self {
            Self::Path { path } => format!("path {}", path.display()),
            Self::Git {
                url,
                tag,
                branch,
                rev,
            } => {
                let reference = tag
                    .as_deref()
                    .map(|value| format!("tag {value}"))
                    .or_else(|| branch.as_deref().map(|value| format!("branch {value}")))
                    .unwrap_or_else(|| format!("rev {rev}"));
                format!("git {url} ({reference}, rev {rev})")
            }
        }
    }
}

impl ReviewSessionPaths {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir().context("failed to allocate temporary review directory")?;
        let store_path = temp.path().join("runtime.sqlite");
        let team_dir = temp.path().join("team");
        let tasks_dir = temp.path().join("tasks");
        let transcript_dir = temp.path().join("transcripts");

        std::fs::create_dir_all(&team_dir)
            .with_context(|| format!("failed to create {}", team_dir.display()))?;
        std::fs::create_dir_all(&tasks_dir)
            .with_context(|| format!("failed to create {}", tasks_dir.display()))?;
        std::fs::create_dir_all(&transcript_dir)
            .with_context(|| format!("failed to create {}", transcript_dir.display()))?;

        Ok(Self {
            _temp: temp,
            store_path,
            team_dir,
            tasks_dir,
            transcript_dir,
        })
    }
}

fn build_review_prompt(scope: &ReviewScope) -> String {
    let mut prompt = String::new();
    let target = scope.target();

    let _ = writeln!(
        prompt,
        "Review this Nodus package graph for safety before installation."
    );
    let _ = writeln!(prompt);
    let _ = writeln!(prompt, "Target package: {}", target.primary_alias());
    let _ = writeln!(prompt, "Target root: {}", target.root.display());
    let _ = writeln!(prompt, "Allowed review roots:");
    for root in scope.allowed_read_roots() {
        let _ = writeln!(prompt, "- {}", root.display());
    }

    let _ = writeln!(prompt);
    let _ = writeln!(prompt, "Review expectations:");
    let _ = writeln!(
        prompt,
        "- Use the `files` tool to inspect concrete evidence before you make claims."
    );
    let _ = writeln!(
        prompt,
        "- Focus on capabilities, instructions, commands, transitive dependencies, remote downloads, credential access, persistence, code execution, and surprising side effects."
    );
    let _ = writeln!(
        prompt,
        "- Treat ambiguous or missing justification for sensitive behavior as risk, not as proof of safety."
    );
    let _ = writeln!(
        prompt,
        "- Do not rely on reputation or external knowledge; reason only from the package graph and files you can inspect."
    );

    let _ = writeln!(prompt);
    let _ = writeln!(prompt, "Dependency graph:");
    render_dependency_tree(scope, scope.target_index, 0, &mut prompt);

    let _ = writeln!(prompt);
    let _ = writeln!(prompt, "Package summaries:");
    for (index, package) in scope.packages.iter().enumerate() {
        let _ = writeln!(prompt, "[package {}]", index + 1);
        let _ = writeln!(
            prompt,
            "aliases = {}",
            package
                .aliases
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
        let _ = writeln!(prompt, "root = {}", package.root.display());
        let _ = writeln!(prompt, "source = {}", package.source.describe());
        if let Some(components) = &package.selected_components {
            let _ = writeln!(
                prompt,
                "selected_components = {}",
                components
                    .iter()
                    .map(|component| component.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        } else {
            let _ = writeln!(prompt, "selected_components = all");
        }
        let _ = writeln!(
            prompt,
            "skills = {}",
            package
                .manifest
                .discovered
                .skills
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let _ = writeln!(
            prompt,
            "agents = {}",
            package
                .manifest
                .discovered
                .agents
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let _ = writeln!(
            prompt,
            "rules = {}",
            package
                .manifest
                .discovered
                .rules
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let _ = writeln!(
            prompt,
            "commands = {}",
            package
                .manifest
                .discovered
                .commands
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        if package.manifest.manifest.capabilities.is_empty() {
            let _ = writeln!(prompt, "capabilities = none");
        } else {
            let _ = writeln!(prompt, "capabilities:");
            for capability in &package.manifest.manifest.capabilities {
                let _ = writeln!(
                    prompt,
                    "- {} ({}){}",
                    capability.id,
                    capability.sensitivity,
                    capability
                        .justification
                        .as_deref()
                        .map(|value| format!(": {value}"))
                        .unwrap_or_default()
                );
            }
        }
        if package.dependencies.is_empty() {
            let _ = writeln!(prompt, "dependencies = none");
        } else {
            let _ = writeln!(
                prompt,
                "dependencies = {}",
                package
                    .dependencies
                    .iter()
                    .map(|dependency| dependency.display_alias())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let _ = writeln!(prompt);
    }

    let _ = writeln!(prompt, "Respond with these sections:");
    let _ = writeln!(prompt, "1. Verdict");
    let _ = writeln!(prompt, "2. Findings");
    let _ = writeln!(prompt, "3. Good Signs");
    let _ = writeln!(prompt, "4. Unknowns");
    let _ = writeln!(prompt, "5. Recommendation");

    prompt
}

fn render_dependency_tree(scope: &ReviewScope, index: usize, depth: usize, output: &mut String) {
    let package = &scope.packages[index];
    let indent = "  ".repeat(depth);
    let _ = writeln!(
        output,
        "{}- {} ({})",
        indent,
        package.primary_alias(),
        package.root.display()
    );
    for dependency in &package.dependencies {
        let child = &scope.packages[dependency.package_index];
        let child_indent = "  ".repeat(depth + 1);
        let _ = writeln!(
            output,
            "{}alias `{}` -> {}",
            child_indent,
            dependency.display_alias(),
            child.root.display()
        );
        render_dependency_tree(scope, dependency.package_index, depth + 2, output);
    }
}

fn authorize_review_request(request: &ToolAuthorizationRequest) -> ToolAuthorizationDecision {
    if request.tool_name != "files" {
        return ToolAuthorizationDecision::deny("review agent only allows the files tool");
    }

    let Some(operations) = request
        .preview
        .structured_input
        .get("operations")
        .and_then(|value| value.as_array())
    else {
        return ToolAuthorizationDecision::deny(
            "review agent requires explicit file operations in tool input",
        );
    };

    let mut disallowed = Vec::new();
    for operation in operations {
        match operation.get("op").and_then(|value| value.as_str()) {
            Some("read" | "list" | "search") => {}
            Some(other) => disallowed.push(other.to_string()),
            None => {
                return ToolAuthorizationDecision::deny(
                    "review agent requires each file operation to declare an `op`",
                );
            }
        }
    }

    if disallowed.is_empty() {
        ToolAuthorizationDecision::allow()
    } else {
        ToolAuthorizationDecision::deny(format!(
            "review agent is read-only; blocked file operations: {}",
            disallowed.join(", ")
        ))
    }
}

#[async_trait]
impl ToolAuthorizer for ReadOnlyReviewAuthorizer {
    async fn authorize(
        &self,
        request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, RuntimeError> {
        Ok(authorize_review_request(request))
    }
}

const REVIEW_SYSTEM_PROMPT: &str = r#"You are a package safety reviewer for Nodus packages.

Your job is to assess whether the target package and its transitive Nodus dependencies appear safe to install and use.

Constraints:
- You may only rely on the files available in the package roots provided by the user.
- Do not assume safety from popularity, branding, or repository location.
- Prefer direct evidence from files over speculation.
- If you cannot verify something, say so plainly.

Risk areas to inspect:
- declared capabilities, especially high-sensitivity ones
- instructions that ask the user to run shell commands, download binaries, or grant secrets
- commands, rules, agents, or skills that could lead to destructive or hidden side effects
- transitive dependencies that expand trust boundaries
- persistence, credential access, exfiltration, privilege escalation, and remote execution patterns

Be precise, skeptical, and evidence-based."#;

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use mentra::provider::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, ProviderId, Request,
        Response, Role, provider_event_stream_from_response,
    };
    use serde_json::json;

    use super::*;

    #[derive(Clone)]
    struct ScriptedProvider {
        kind: ProviderId,
        models: Vec<ModelInfo>,
        turns: Arc<Mutex<VecDeque<String>>>,
        requests: Arc<Mutex<Vec<Request<'static>>>>,
    }

    impl ScriptedProvider {
        fn new(model: ModelInfo, turns: Vec<String>) -> Self {
            Self {
                kind: model.provider.clone(),
                models: vec![model],
                turns: Arc::new(Mutex::new(turns.into())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn recorded_requests(&self) -> Vec<Request<'static>> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor::new(self.kind.clone())
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(self.models.clone())
        }

        async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
            self.requests.lock().unwrap().push(request.into_owned());
            let text = self
                .turns
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted provider missing response");
            Ok(provider_event_stream_from_response(Response {
                id: "review-response".into(),
                model: self.models[0].id.clone(),
                role: Role::Assistant,
                content: vec![ContentBlock::text(text)],
                stop_reason: None,
                usage: None,
            }))
        }
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn collects_nested_dependency_graph_for_review() {
        let temp = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let child = root.join("vendor/child");

        write_file(
            &root.join("nodus.toml"),
            "[dependencies]\nchild = { path = \"vendor/child\" }\n",
        );
        write_file(
            &root.join("skills/root/SKILL.md"),
            "---\nname: Root\ndescription: Root skill.\n---\n# Root\n",
        );
        write_file(
            &child.join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Review safely.\n---\n# Review\n",
        );
        write_file(
            &child.join("commands/check.md"),
            "# Check\nRun this review.\n",
        );

        let reporter = Reporter::silent();
        let scope = collect_review_scope(&root, cache.path(), ".", None, None, &reporter).unwrap();

        assert_eq!(scope.packages.len(), 2);
        assert_eq!(scope.target().primary_alias(), "root");
        assert_eq!(scope.packages[0].dependencies.len(), 1);
        assert_eq!(scope.packages[1].primary_alias(), "child");
    }

    #[test]
    fn review_marks_root_dev_dependencies_in_the_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");

        write_file(
            &root.join("nodus.toml"),
            "[dev-dependencies]\nchild = { path = \"vendor/child\" }\n",
        );
        write_file(
            &root.join("skills/root/SKILL.md"),
            "---\nname: Root\ndescription: Root skill.\n---\n# Root\n",
        );
        write_file(
            &root.join("vendor/child/skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Review safely.\n---\n# Review\n",
        );

        let reporter = Reporter::silent();
        let scope = collect_review_scope(&root, cache.path(), ".", None, None, &reporter).unwrap();
        let prompt = build_review_prompt(&scope);

        assert!(prompt.contains("dependencies = child (dev)"));
    }

    #[test]
    fn review_prompt_includes_dependency_graph_and_roots() {
        let temp = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");

        write_file(
            &root.join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Review safely.\n---\n# Review\n",
        );

        let reporter = Reporter::silent();
        let scope = collect_review_scope(&root, cache.path(), ".", None, None, &reporter).unwrap();
        let prompt = build_review_prompt(&scope);

        assert!(prompt.contains("Allowed review roots:"));
        assert!(prompt.contains(&root.display().to_string()));
        assert!(prompt.contains("Respond with these sections:"));
    }

    #[test]
    fn authorizer_blocks_mutating_file_operations() {
        let request = ToolAuthorizationRequest {
            agent_id: "agent-1".into(),
            agent_name: "review".into(),
            model: "mock-model".into(),
            history_len: 1,
            tool_call_id: "call-1".into(),
            tool_name: "files".into(),
            preview: mentra::tool::ToolAuthorizationPreview {
                working_directory: PathBuf::from("/tmp"),
                capabilities: Vec::new(),
                side_effect_level: mentra::tool::ToolSideEffectLevel::None,
                durability: mentra::tool::ToolDurability::Ephemeral,
                raw_input: json!({}),
                structured_input: json!({
                    "operations": [
                        { "op": "read", "path": "/tmp/a" },
                        { "op": "set", "path": "/tmp/a", "content": "x" }
                    ]
                }),
            },
        };

        let decision = authorize_review_request(&request);

        assert_eq!(
            decision.outcome,
            mentra::tool::ToolAuthorizationOutcome::Deny
        );
        assert!(
            decision
                .reason
                .unwrap()
                .contains("blocked file operations: set")
        );
    }

    #[tokio::test]
    async fn execute_review_uses_only_files_tool() {
        let temp = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        write_file(
            &root.join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Review safely.\n---\n# Review\n",
        );

        let reporter = Reporter::silent();
        let scope = collect_review_scope(&root, cache.path(), ".", None, None, &reporter).unwrap();
        let session = ReviewSessionPaths::new().unwrap();
        let model = ModelInfo::new("mock-model", BuiltinProvider::OpenAI);
        let provider = ScriptedProvider::new(model.clone(), vec!["Verdict: caution".into()]);
        let runtime = Runtime::builder()
            .with_store(SqliteRuntimeStore::new(session.store_path.clone()))
            .with_policy(RuntimePolicy::default().with_allowed_read_root(root.clone()))
            .with_tool_authorizer(ReadOnlyReviewAuthorizer)
            .with_provider_instance(provider.clone())
            .build()
            .unwrap();

        let review = execute_review_with_runtime(&runtime, model, &scope, &session)
            .await
            .unwrap();
        let requests = provider.recorded_requests();

        assert_eq!(review, "Verdict: caution");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].tools.len(), 1);
        assert_eq!(requests[0].tools[0].name, "files");
        assert_eq!(requests[0].system.as_deref(), Some(REVIEW_SYSTEM_PROMPT));
    }
}
