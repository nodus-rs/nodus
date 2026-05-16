use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anstyle::{AnsiColor, Effects, Style};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::adapters::{Adapter, effective_session_start_sources, hook_supported_by_adapter};
use crate::domain::dependency_query::{
    ResolvedInspectionSource, load_manifest_for_inspection, resolve_inspection_target,
};
use crate::git::{ensure_git_dependency, normalize_alias_from_url, normalize_git_url};
use crate::install_paths::codex_user_config_writes_enabled;
use crate::manifest::{
    DependencyComponent, DependencySpec, LoadedManifest, ManagedPlacement, PackageRole,
    RequestedGitRef as ManifestRequestedGitRef, normalize_dependency_alias,
};
use crate::paths::display_path;
use crate::report::Reporter;

#[derive(Debug, Clone, Serialize)]
pub struct PackageInfo {
    alias: String,
    enabled: bool,
    name: String,
    version: Option<String>,
    version_requirement: Option<String>,
    description: Option<String>,
    license: Option<String>,
    rust_version: Option<String>,
    documentation: Option<String>,
    homepage: Option<String>,
    repository: Option<String>,
    keywords: Vec<String>,
    features: BTreeMap<String, Vec<String>>,
    api_version: Option<String>,
    root: PathBuf,
    source: PackageInfoSource,
    selected_components: Option<Vec<DependencyComponent>>,
    workspace_members: Vec<PackageWorkspaceMember>,
    adapters: Vec<Adapter>,
    skills: Vec<String>,
    agents: Vec<String>,
    rules: Vec<String>,
    commands: Vec<String>,
    mcp_servers: Vec<String>,
    managed_exports: Vec<PackageManagedExport>,
    dependencies: Vec<String>,
    dev_dependencies: Vec<String>,
    capabilities: Vec<PackageCapability>,
    hook_adapter_support: Vec<PackageHookAdapterSupport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_integration: Option<PackageNativeIntegration>,
    warnings: Vec<String>,
    #[serde(skip)]
    show_dev_dependencies: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum PackageInfoSource {
    Path {
        path: PathBuf,
        tag: Option<String>,
    },
    Git {
        url: String,
        subpath: Option<PathBuf>,
        tag: Option<String>,
        branch: Option<String>,
        rev: String,
    },
}

#[derive(Debug, Clone, Serialize)]
struct PackageNativeIntegration {
    adapters: Vec<Adapter>,
    marketplaces: Vec<NativeMarketplaceInfo>,
    plugins: Vec<NativePluginInfo>,
    hooks: Vec<NativeHookLocation>,
    codex: CodexNativeState,
    claude: ClaudeNativeState,
}

#[derive(Debug, Clone, Serialize)]
struct NativeMarketplaceInfo {
    adapter: Adapter,
    path: String,
    exists: bool,
    name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct NativePluginInfo {
    adapter: Adapter,
    key: String,
    root: String,
    hooks: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct NativeHookLocation {
    adapter: Adapter,
    scope: String,
    path: String,
}

#[derive(Debug, Clone, Serialize)]
struct CodexNativeState {
    project_config: String,
    hooks: Option<bool>,
    plugin_hooks: Option<bool>,
    plugin_hooks_required: bool,
    user_config: String,
}

#[derive(Debug, Clone, Serialize)]
struct ClaudeNativeState {
    settings: String,
    extra_known_marketplaces: Vec<String>,
    enabled_plugins: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PackageCapability {
    id: String,
    sensitivity: String,
    justification: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PackageManagedExport {
    source: String,
    target: String,
    placement: String,
    resolved_root: String,
}

#[derive(Debug, Clone, Serialize)]
struct PackageWorkspaceMember {
    id: String,
    enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
struct PackageHookAdapterSupport {
    adapter: Adapter,
    supported_events: Vec<PackageHookEventSupport>,
}

#[derive(Debug, Clone, Serialize)]
struct PackageHookEventSupport {
    event: String,
    session_start_sources: Vec<String>,
}

struct PackageInfoContext {
    enabled: bool,
    selected_components: Option<Vec<DependencyComponent>>,
    selected_workspace_members: Option<Vec<String>>,
    version_requirement: Option<String>,
    role: PackageRole,
}

#[derive(Debug, Default, Deserialize)]
struct CargoManifest {
    #[serde(default)]
    package: Option<CargoPackageSection>,
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct CargoPackageSection {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default, rename = "rust-version")]
    rust_version: Option<String>,
    #[serde(default)]
    documentation: Option<String>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
}

#[derive(Debug, Default)]
struct CargoMetadata {
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    license: Option<String>,
    rust_version: Option<String>,
    documentation: Option<String>,
    homepage: Option<String>,
    repository: Option<String>,
    keywords: Vec<String>,
    features: BTreeMap<String, Vec<String>>,
}

pub fn describe_package_in_dir(
    cwd: &Path,
    cache_root: &Path,
    package: &str,
    tag: Option<&str>,
    branch: Option<&str>,
    reporter: &Reporter,
) -> Result<()> {
    let info = load_package_info(cwd, cache_root, package, tag, branch, reporter)?;
    for line in info.render_lines(reporter) {
        reporter.line(line)?;
    }
    Ok(())
}

pub fn describe_package_json_in_dir(
    cwd: &Path,
    cache_root: &Path,
    package: &str,
    tag: Option<&str>,
    branch: Option<&str>,
) -> Result<PackageInfo> {
    load_package_info(cwd, cache_root, package, tag, branch, &Reporter::silent())
}

fn load_package_info(
    cwd: &Path,
    cache_root: &Path,
    package: &str,
    tag: Option<&str>,
    branch: Option<&str>,
    reporter: &Reporter,
) -> Result<PackageInfo> {
    let trimmed = package.trim();
    if trimmed.is_empty() {
        bail!("package must not be empty");
    }

    if let Some((alias, dependency, root_manifest)) = resolve_direct_dependency(cwd, trimmed)? {
        if tag.is_some() || branch.is_some() {
            bail!(
                "`--tag` and `--branch` can only be used when inspecting a direct package reference"
            );
        }
        return load_from_dependency_spec(
            &alias,
            &dependency,
            &root_manifest,
            cache_root,
            reporter,
        );
    }

    if tag.is_none()
        && branch.is_none()
        && let Some(package_root) = resolve_local_package_path(cwd, trimmed)?
    {
        let (manifest, role) = load_package_manifest_for_inspection(&package_root)?;
        let alias = alias_from_loaded_manifest(&manifest)?;
        return Ok(package_info_from_loaded(
            alias,
            manifest,
            PackageInfoSource::Path {
                path: package_root,
                tag: None,
            },
            PackageInfoContext {
                enabled: true,
                selected_components: None,
                selected_workspace_members: None,
                version_requirement: None,
                role,
            },
        ));
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
    let (manifest, role) = load_package_manifest_for_inspection(&checkout.path)
        .with_context(|| format!("dependency `{alias}` does not match the Nodus package layout"))?;

    Ok(package_info_from_loaded(
        alias,
        manifest,
        PackageInfoSource::Git {
            url: checkout.url,
            subpath: None,
            tag: checkout.tag,
            branch: checkout.branch,
            rev: checkout.rev,
        },
        PackageInfoContext {
            enabled: true,
            selected_components: None,
            selected_workspace_members: None,
            version_requirement: None,
            role,
        },
    ))
}

fn resolve_direct_dependency(
    cwd: &Path,
    package: &str,
) -> Result<Option<(String, DependencySpec, LoadedManifest)>> {
    crate::domain::dependency_query::resolve_direct_dependency(cwd, package)
}

fn resolve_local_package_path(cwd: &Path, package: &str) -> Result<Option<PathBuf>> {
    crate::domain::dependency_query::resolve_local_package_path(cwd, package)
}

fn load_from_dependency_spec(
    alias: &str,
    dependency: &DependencySpec,
    root_manifest: &LoadedManifest,
    cache_root: &Path,
    reporter: &Reporter,
) -> Result<PackageInfo> {
    let target = resolve_inspection_target(alias, dependency, root_manifest, cache_root, reporter)?;
    let source = match target.source {
        ResolvedInspectionSource::Path {
            declared_path,
            resolved_root,
            tag,
        } => PackageInfoSource::Path {
            path: declared_path.unwrap_or(resolved_root),
            tag,
        },
        ResolvedInspectionSource::Git {
            url,
            subpath,
            tag,
            branch,
            rev,
        } => PackageInfoSource::Git {
            url,
            subpath,
            tag,
            branch,
            rev,
        },
    };
    Ok(package_info_from_loaded(
        target.alias,
        target.manifest,
        source,
        PackageInfoContext {
            enabled: target.enabled,
            selected_components: target.selected_components,
            selected_workspace_members: target.selected_workspace_members,
            version_requirement: target.version_requirement,
            role: target.role,
        },
    ))
}

fn load_package_manifest_for_inspection(root: &Path) -> Result<(LoadedManifest, PackageRole)> {
    load_manifest_for_inspection(root)
}

fn package_info_from_loaded(
    alias: String,
    manifest: LoadedManifest,
    source: PackageInfoSource,
    context: PackageInfoContext,
) -> PackageInfo {
    let PackageInfoContext {
        enabled,
        selected_components,
        selected_workspace_members,
        version_requirement,
        role,
    } = context;
    let mut warnings = manifest.warnings.clone();
    let cargo_metadata = load_cargo_metadata(&manifest.root, &mut warnings);
    let mut adapters = manifest
        .manifest
        .enabled_adapters()
        .map(|enabled| enabled.to_vec())
        .unwrap_or_default();
    adapters.sort();

    let mut dependencies = manifest
        .manifest
        .dependencies
        .iter()
        .map(render_dependency_name)
        .collect::<Vec<_>>();
    dependencies.sort();
    let mut dev_dependencies = if role == PackageRole::Root {
        manifest
            .manifest
            .dev_dependencies
            .iter()
            .map(render_dependency_name)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    dev_dependencies.sort();
    let selected_workspace_members = selected_workspace_members
        .unwrap_or_else(|| {
            if role == PackageRole::Root {
                manifest
                    .resolved_workspace_members()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|member| member.id)
                    .collect()
            } else {
                Vec::new()
            }
        })
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let hooks = if role == PackageRole::Root {
        manifest.manifest.effective_hooks()
    } else {
        manifest.manifest.hooks.clone()
    };
    let native_integration = (role == PackageRole::Root)
        .then(|| build_native_integration_info(&manifest.root, &adapters, &mut warnings))
        .flatten();

    PackageInfo {
        alias,
        enabled,
        name: manifest
            .manifest
            .name
            .clone()
            .or_else(|| cargo_metadata.name.clone())
            .unwrap_or_else(|| manifest.effective_name()),
        version: manifest
            .effective_version()
            .map(|version| version.to_string())
            .or_else(|| cargo_metadata.version.clone()),
        version_requirement,
        description: cargo_metadata.description,
        license: cargo_metadata.license,
        rust_version: cargo_metadata.rust_version,
        documentation: cargo_metadata.documentation,
        homepage: cargo_metadata.homepage,
        repository: cargo_metadata.repository,
        keywords: cargo_metadata.keywords,
        features: cargo_metadata.features,
        api_version: manifest.manifest.api_version.clone(),
        root: manifest.root.clone(),
        source,
        selected_components,
        workspace_members: manifest
            .resolved_workspace_members()
            .unwrap_or_default()
            .into_iter()
            .map(|member| PackageWorkspaceMember {
                enabled: selected_workspace_members.contains(&member.id),
                id: member.id,
            })
            .collect(),
        adapters,
        skills: manifest
            .discovered
            .skills
            .iter()
            .map(|entry| entry.id.clone())
            .collect(),
        agents: manifest
            .discovered
            .agents
            .iter()
            .map(|entry| entry.id.clone())
            .collect(),
        rules: manifest
            .discovered
            .rules
            .iter()
            .map(|entry| entry.id.clone())
            .collect(),
        commands: manifest
            .discovered
            .commands
            .iter()
            .map(|entry| entry.id.clone())
            .collect(),
        mcp_servers: manifest.manifest.mcp_servers.keys().cloned().collect(),
        managed_exports: manifest
            .manifest
            .managed_exports
            .iter()
            .map(|managed_export| PackageManagedExport {
                source: display_path(&managed_export.source),
                target: display_path(&managed_export.target),
                placement: match managed_export.placement {
                    ManagedPlacement::Package => "package".into(),
                    ManagedPlacement::Project => "project".into(),
                },
                resolved_root: match managed_export.placement {
                    ManagedPlacement::Package => {
                        format!(".nodus/packages/{}", manifest.effective_name())
                    }
                    ManagedPlacement::Project => ".".into(),
                },
            })
            .collect(),
        dependencies,
        dev_dependencies,
        capabilities: manifest
            .manifest
            .capabilities
            .iter()
            .map(|capability| PackageCapability {
                id: capability.id.clone(),
                sensitivity: capability.sensitivity.clone(),
                justification: capability.justification.clone(),
            })
            .collect(),
        hook_adapter_support: build_hook_adapter_support(&hooks),
        native_integration,
        warnings,
        show_dev_dependencies: role == PackageRole::Root,
    }
}

fn load_cargo_metadata(root: &Path, warnings: &mut Vec<String>) -> CargoMetadata {
    let manifest_path = root.join("Cargo.toml");
    let Ok(contents) = fs::read_to_string(&manifest_path) else {
        return CargoMetadata::default();
    };

    match toml::from_str::<CargoManifest>(&contents) {
        Ok(cargo_manifest) => {
            let package = cargo_manifest.package.unwrap_or_default();
            CargoMetadata {
                name: package.name,
                version: package.version,
                description: package.description,
                license: package.license,
                rust_version: package.rust_version,
                documentation: package.documentation,
                homepage: package.homepage,
                repository: package.repository,
                keywords: package.keywords,
                features: cargo_manifest.features,
            }
        }
        Err(error) => {
            warnings.push(format!(
                "failed to parse Cargo metadata in {}: {error}",
                manifest_path.display()
            ));
            CargoMetadata::default()
        }
    }
}

fn build_native_integration_info(
    project_root: &Path,
    manifest_adapters: &[Adapter],
    warnings: &mut Vec<String>,
) -> Option<PackageNativeIntegration> {
    let mut marketplaces = Vec::new();
    let mut plugins = Vec::new();
    for adapter in [Adapter::Claude, Adapter::Codex] {
        let Some((marketplace, mut marketplace_plugins)) =
            inspect_native_marketplace(project_root, adapter, warnings)
        else {
            continue;
        };
        marketplaces.push(marketplace);
        plugins.append(&mut marketplace_plugins);
    }

    let hooks = inspect_native_hook_locations(project_root, &plugins);
    let codex = inspect_codex_native_state(
        project_root,
        plugins
            .iter()
            .any(|plugin| plugin.adapter == Adapter::Codex && plugin.hooks.is_some()),
        warnings,
    );
    let claude = inspect_claude_native_state(project_root, warnings);

    let mut adapters = manifest_adapters
        .iter()
        .copied()
        .filter(|adapter| matches!(adapter, Adapter::Claude | Adapter::Codex))
        .collect::<BTreeSet<_>>();
    for marketplace in &marketplaces {
        if marketplace.exists {
            adapters.insert(marketplace.adapter);
        }
    }
    if codex.hooks.is_some() || codex.plugin_hooks.is_some() {
        adapters.insert(Adapter::Codex);
    }
    if !claude.extra_known_marketplaces.is_empty() || !claude.enabled_plugins.is_empty() {
        adapters.insert(Adapter::Claude);
    }

    let has_state = !adapters.is_empty()
        || marketplaces.iter().any(|marketplace| marketplace.exists)
        || !plugins.is_empty()
        || !hooks.is_empty()
        || codex.hooks.is_some()
        || codex.plugin_hooks.is_some()
        || !claude.extra_known_marketplaces.is_empty()
        || !claude.enabled_plugins.is_empty();
    has_state.then(|| PackageNativeIntegration {
        adapters: adapters.into_iter().collect(),
        marketplaces,
        plugins,
        hooks,
        codex,
        claude,
    })
}

fn inspect_native_marketplace(
    project_root: &Path,
    adapter: Adapter,
    warnings: &mut Vec<String>,
) -> Option<(NativeMarketplaceInfo, Vec<NativePluginInfo>)> {
    let path = crate::adapters::native_marketplace_path(project_root, adapter)?;
    let display = display_project_path(project_root, &path);
    if !path.exists() {
        return Some((
            NativeMarketplaceInfo {
                adapter,
                path: display,
                exists: false,
                name: None,
            },
            Vec::new(),
        ));
    }

    let Some(json) = read_json_file(&path, warnings) else {
        return Some((
            NativeMarketplaceInfo {
                adapter,
                path: display,
                exists: true,
                name: None,
            },
            Vec::new(),
        ));
    };
    let name = json
        .get("name")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let plugins = json
        .get("plugins")
        .and_then(JsonValue::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    native_plugin_info(project_root, adapter, name.as_deref(), entry)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Some((
        NativeMarketplaceInfo {
            adapter,
            path: display,
            exists: true,
            name,
        },
        plugins,
    ))
}

fn native_plugin_info(
    project_root: &Path,
    adapter: Adapter,
    marketplace_name: Option<&str>,
    entry: &JsonValue,
) -> Option<NativePluginInfo> {
    let plugin_name = entry.get("name").and_then(JsonValue::as_str)?;
    let root = native_plugin_source_root(project_root, adapter, entry)?;
    let key = marketplace_name
        .map(|marketplace| format!("{plugin_name}@{marketplace}"))
        .unwrap_or_else(|| plugin_name.to_string());
    let hooks_path = root.join("hooks").join("hooks.json");
    Some(NativePluginInfo {
        adapter,
        key,
        root: display_project_path(project_root, &root),
        hooks: hooks_path
            .exists()
            .then(|| display_project_path(project_root, &hooks_path)),
    })
}

fn native_plugin_source_root(
    project_root: &Path,
    adapter: Adapter,
    entry: &JsonValue,
) -> Option<PathBuf> {
    let source = match adapter {
        Adapter::Claude => entry.get("source").and_then(JsonValue::as_str),
        Adapter::Codex => entry
            .get("source")
            .and_then(|source| source.get("path"))
            .and_then(JsonValue::as_str)
            .or_else(|| entry.get("source").and_then(JsonValue::as_str)),
        Adapter::Agents | Adapter::Copilot | Adapter::Cursor | Adapter::OpenCode => None,
    }?;
    let source = source.strip_prefix("./").unwrap_or(source);
    let path = Path::new(source);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        Some(project_root.join(".nodus").join(path))
    }
}

fn inspect_native_hook_locations(
    project_root: &Path,
    plugins: &[NativePluginInfo],
) -> Vec<NativeHookLocation> {
    let mut hooks = Vec::new();
    for (adapter, path) in [
        (
            Adapter::Claude,
            project_root.join(".claude").join("settings.json"),
        ),
        (
            Adapter::Codex,
            project_root.join(".codex").join("hooks.json"),
        ),
    ] {
        if path.exists() {
            hooks.push(NativeHookLocation {
                adapter,
                scope: "workspace".into(),
                path: display_project_path(project_root, &path),
            });
        }
    }

    for plugin in plugins {
        if let Some(path) = &plugin.hooks {
            hooks.push(NativeHookLocation {
                adapter: plugin.adapter,
                scope: format!("plugin:{}", plugin.key),
                path: path.clone(),
            });
        }
    }
    hooks
}

fn inspect_codex_native_state(
    project_root: &Path,
    plugin_hooks_required: bool,
    warnings: &mut Vec<String>,
) -> CodexNativeState {
    let path = project_root.join(".codex").join("config.toml");
    let (hooks, plugin_hooks) = if path.exists() {
        match fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))
            .and_then(|contents| {
                contents
                    .parse::<toml_edit::DocumentMut>()
                    .with_context(|| format!("failed to parse TOML in {}", path.display()))
            }) {
            Ok(value) => {
                let features = value.get("features");
                (
                    features
                        .and_then(|features| features.get("hooks"))
                        .and_then(toml_edit::Item::as_bool),
                    features
                        .and_then(|features| features.get("plugin_hooks"))
                        .and_then(toml_edit::Item::as_bool),
                )
            }
            Err(error) => {
                warnings.push(error.to_string());
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    CodexNativeState {
        project_config: if path.exists() {
            format!("{} (present)", display_project_path(project_root, &path))
        } else {
            format!("{} (missing)", display_project_path(project_root, &path))
        },
        hooks,
        plugin_hooks,
        plugin_hooks_required,
        user_config: if codex_user_config_writes_enabled() {
            "auto".into()
        } else {
            "disabled".into()
        },
    }
}

fn inspect_claude_native_state(
    project_root: &Path,
    warnings: &mut Vec<String>,
) -> ClaudeNativeState {
    let path = project_root.join(".claude").join("settings.json");
    let mut extra_known_marketplaces = Vec::new();
    let mut enabled_plugins = Vec::new();
    if path.exists()
        && let Some(json) = read_json_file(&path, warnings)
    {
        extra_known_marketplaces = json
            .get("extraKnownMarketplaces")
            .and_then(JsonValue::as_object)
            .map(|entries| entries.keys().cloned().collect())
            .unwrap_or_default();
        enabled_plugins = json
            .get("enabledPlugins")
            .and_then(JsonValue::as_object)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|(key, value)| {
                        value.as_bool().unwrap_or(false).then_some(key.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
    }
    extra_known_marketplaces.sort();
    enabled_plugins.sort();

    ClaudeNativeState {
        settings: if path.exists() {
            format!("{} (present)", display_project_path(project_root, &path))
        } else {
            format!("{} (missing)", display_project_path(project_root, &path))
        },
        extra_known_marketplaces,
        enabled_plugins,
    }
}

fn read_json_file(path: &Path, warnings: &mut Vec<String>) -> Option<JsonValue> {
    match fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))
        .and_then(|contents| {
            serde_json::from_str::<JsonValue>(&contents)
                .with_context(|| format!("failed to parse JSON in {}", path.display()))
        }) {
        Ok(value) => Some(value),
        Err(error) => {
            warnings.push(error.to_string());
            None
        }
    }
}

fn display_project_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(display_path)
        .unwrap_or_else(|| display_path(path))
}

fn alias_from_loaded_manifest(manifest: &LoadedManifest) -> Result<String> {
    normalize_dependency_alias(&manifest.effective_name())
}

impl PackageInfo {
    fn render_lines(&self, reporter: &Reporter) -> Vec<String> {
        let mut lines = Vec::new();

        lines.push(self.header_line(reporter));
        if let Some(description) = &self.description {
            lines.push(description.clone());
        }

        self.push_optional_field(&mut lines, reporter, "version", self.version.as_deref());
        self.push_optional_field(
            &mut lines,
            reporter,
            "version-requirement",
            self.version_requirement.as_deref(),
        );
        self.push_optional_field(&mut lines, reporter, "license", self.license.as_deref());
        self.push_optional_field(
            &mut lines,
            reporter,
            "rust-version",
            self.rust_version.as_deref(),
        );
        self.push_optional_field(
            &mut lines,
            reporter,
            "documentation",
            self.documentation.as_deref(),
        );
        self.push_optional_field(&mut lines, reporter, "homepage", self.homepage.as_deref());
        self.push_optional_field(
            &mut lines,
            reporter,
            "repository",
            self.repository.as_deref(),
        );
        lines.push(format!(
            "{} {}",
            paint_label(reporter, "source:"),
            self.source_display()
        ));
        lines.push(format!(
            "{} {}",
            paint_label(reporter, "package-root:"),
            display_path(&self.root)
        ));
        lines.push(format!(
            "{} {}",
            paint_label(reporter, "alias:"),
            self.alias
        ));
        if !self.enabled {
            lines.push(format!("{} disabled", paint_label(reporter, "status:")));
        }
        if let Some(api_version) = &self.api_version {
            lines.push(format!(
                "{} {api_version}",
                paint_label(reporter, "api-version:")
            ));
        }
        lines.push(format!(
            "{} {}",
            paint_label(reporter, "components:"),
            render_components(self.selected_components.as_deref())
        ));
        lines.push(format!(
            "{} {}",
            paint_label(reporter, "adapters:"),
            render_adapters(&self.adapters)
        ));
        lines.push(format!(
            "{} {}",
            paint_label(reporter, "dependencies:"),
            render_items(&self.dependencies)
        ));
        if self.show_dev_dependencies {
            lines.push(format!(
                "{} {}",
                paint_label(reporter, "dev-dependencies:"),
                render_items(&self.dev_dependencies)
            ));
        }

        let artifacts = [
            ("skills", &self.skills),
            ("agents", &self.agents),
            ("rules", &self.rules),
            ("commands", &self.commands),
        ];
        if artifacts.iter().any(|(_, items)| !items.is_empty()) {
            lines.push(paint_label(reporter, "artifacts:"));
            lines.extend(render_named_lists(reporter, &artifacts));
        }

        if !self.capabilities.is_empty() {
            lines.push(paint_label(reporter, "capabilities:"));
            lines.extend(render_capability_lines(reporter, &self.capabilities));
        }

        if !self.hook_adapter_support.is_empty() {
            lines.push(paint_label(reporter, "hook-adapter-support:"));
            lines.extend(render_hook_adapter_support_lines(
                reporter,
                &self.hook_adapter_support,
            ));
        }

        if let Some(native_integration) = &self.native_integration {
            lines.push(paint_label(reporter, "native-integration:"));
            lines.extend(render_native_integration_lines(
                reporter,
                native_integration,
            ));
        }

        if !self.managed_exports.is_empty() {
            lines.push(paint_label(reporter, "managed-exports:"));
            lines.extend(render_managed_export_lines(reporter, &self.managed_exports));
        }

        if !self.workspace_members.is_empty() {
            lines.push(paint_label(reporter, "workspace-members:"));
            lines.extend(render_workspace_member_lines(
                reporter,
                &self.workspace_members,
            ));
        }

        if !self.mcp_servers.is_empty() {
            lines.push(format!(
                "{} {}",
                paint_label(reporter, "mcp-servers:"),
                render_items(&self.mcp_servers)
            ));
        }

        if !self.features.is_empty() {
            lines.push(paint_label(reporter, "features:"));
            lines.extend(render_feature_lines(reporter, &self.features));
        }

        if !self.warnings.is_empty() {
            lines.push(paint_label(reporter, "warnings:"));
            lines.extend(self.warnings.iter().map(|warning| format!("  {warning}")));
        }

        lines
    }

    fn header_line(&self, reporter: &Reporter) -> String {
        let name = reporter.paint(&self.name, title_style());
        if self.keywords.is_empty() {
            name
        } else {
            format!(
                "{} {}",
                name,
                self.keywords
                    .iter()
                    .map(|keyword| reporter.paint(&format!("#{keyword}"), keyword_style()))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        }
    }

    fn push_optional_field(
        &self,
        lines: &mut Vec<String>,
        reporter: &Reporter,
        label: &str,
        value: Option<&str>,
    ) {
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            lines.push(format!(
                "{} {value}",
                paint_label(reporter, &format!("{label}:"))
            ));
        }
    }

    fn source_display(&self) -> String {
        match &self.source {
            PackageInfoSource::Path { path, tag } => match tag {
                Some(tag) => format!("path {} (tag {tag})", display_path(path)),
                None => format!("path {}", display_path(path)),
            },
            PackageInfoSource::Git {
                url,
                subpath,
                tag,
                branch,
                rev,
            } => {
                let mut details = Vec::new();
                if let Some(subpath) = subpath {
                    details.push(format!("subpath {}", display_path(subpath)));
                }
                if let Some(tag) = tag {
                    details.push(format!("tag {tag}"));
                }
                if let Some(branch) = branch {
                    details.push(format!("branch {branch}"));
                }
                details.push(format!("rev {}", short_rev(rev)));

                format!("git {url} ({})", details.join(", "))
            }
        }
    }
}

fn render_workspace_member_lines(
    reporter: &Reporter,
    members: &[PackageWorkspaceMember],
) -> Vec<String> {
    members
        .iter()
        .map(|member| {
            let status = if member.enabled {
                "enabled"
            } else {
                "disabled"
            };
            let status = if member.enabled {
                reporter.paint(status, label_style())
            } else {
                reporter.paint(status, dim_style())
            };
            format!("  {} ({status})", member.id)
        })
        .collect()
}

fn render_named_lists(reporter: &Reporter, items: &[(&str, &Vec<String>)]) -> Vec<String> {
    let width = items
        .iter()
        .filter(|(_, values)| !values.is_empty())
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);
    items
        .iter()
        .filter(|(_, values)| !values.is_empty())
        .map(|(name, values)| {
            let padded = format!("{name:width$}", width = width);
            let label = if reporter.color_enabled() {
                reporter.paint(&padded, dim_style())
            } else {
                padded
            };
            format!("  {label} = [{}]", values.join(", "))
        })
        .collect()
}

fn render_capability_lines(reporter: &Reporter, capabilities: &[PackageCapability]) -> Vec<String> {
    let width = capabilities
        .iter()
        .map(|capability| capability.id.len())
        .max()
        .unwrap_or(0);
    capabilities
        .iter()
        .map(|capability| {
            let padded = format!("{:width$}", capability.id, width = width);
            let id = if reporter.color_enabled() {
                reporter.paint(&padded, dim_style())
            } else {
                padded
            };
            let justification = capability
                .justification
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!(" ({value})"))
                .unwrap_or_default();
            format!(
                "  {id} = {sensitivity}{justification}",
                sensitivity = capability.sensitivity,
            )
        })
        .collect()
}

fn build_hook_adapter_support(
    hooks: &[crate::manifest::HookSpec],
) -> Vec<PackageHookAdapterSupport> {
    if hooks.is_empty() {
        return Vec::new();
    }

    Adapter::ALL
        .into_iter()
        .map(|adapter| PackageHookAdapterSupport {
            adapter,
            supported_events: hooks
                .iter()
                .filter(|hook| hook_supported_by_adapter(hook, adapter))
                .map(|hook| PackageHookEventSupport {
                    event: hook.event.as_str().to_string(),
                    session_start_sources: effective_session_start_sources(hook, adapter)
                        .into_iter()
                        .map(|source| source.as_str().to_string())
                        .collect(),
                })
                .collect(),
        })
        .collect()
}

fn render_hook_adapter_support_lines(
    reporter: &Reporter,
    support: &[PackageHookAdapterSupport],
) -> Vec<String> {
    let width = support
        .iter()
        .map(|entry| entry.adapter.as_str().len())
        .max()
        .unwrap_or(0);

    support
        .iter()
        .map(|entry| {
            let padded = format!("{:width$}", entry.adapter.as_str(), width = width);
            let label = if reporter.color_enabled() {
                reporter.paint(&padded, dim_style())
            } else {
                padded
            };
            let summary = if entry.supported_events.is_empty() {
                "none".to_string()
            } else {
                entry
                    .supported_events
                    .iter()
                    .map(|event| {
                        if event.session_start_sources.is_empty() {
                            event.event.clone()
                        } else {
                            format!("{}({})", event.event, event.session_start_sources.join(","))
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!("  {label} = {summary}")
        })
        .collect()
}

fn render_native_integration_lines(
    reporter: &Reporter,
    native: &PackageNativeIntegration,
) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!(
        "  adapters = [{}]",
        native
            .adapters
            .iter()
            .map(|adapter| adapter.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));

    if !native.marketplaces.is_empty() {
        lines.push(format!("  {}", paint_label(reporter, "marketplaces:")));
        for marketplace in &native.marketplaces {
            let state = if marketplace.exists {
                marketplace
                    .name
                    .as_deref()
                    .map(|name| format!("present, {name}"))
                    .unwrap_or_else(|| "present".into())
            } else {
                "missing".into()
            };
            lines.push(format!(
                "    {} {} ({state})",
                marketplace.adapter.as_str(),
                marketplace.path
            ));
        }
    }

    if !native.plugins.is_empty() {
        lines.push(format!("  {}", paint_label(reporter, "plugins:")));
        for plugin in &native.plugins {
            let hooks = plugin
                .hooks
                .as_deref()
                .map(|hooks| format!(" hooks={hooks}"))
                .unwrap_or_default();
            lines.push(format!(
                "    {} {} -> {}{}",
                plugin.adapter.as_str(),
                plugin.key,
                plugin.root,
                hooks
            ));
        }
    }

    if !native.hooks.is_empty() {
        lines.push(format!("  {}", paint_label(reporter, "hooks:")));
        for hook in &native.hooks {
            lines.push(format!(
                "    {} {} {}",
                hook.adapter.as_str(),
                hook.scope,
                hook.path
            ));
        }
    }

    lines.push(format!(
        "  codex = project-config:{} hooks={} plugin_hooks={} plugin_hooks_required={} user-config={}",
        native.codex.project_config,
        render_optional_bool(native.codex.hooks),
        render_optional_bool(native.codex.plugin_hooks),
        native.codex.plugin_hooks_required,
        native.codex.user_config
    ));
    lines.push(format!(
        "  claude = settings:{} marketplaces=[{}] enabled=[{}]",
        native.claude.settings,
        native.claude.extra_known_marketplaces.join(", "),
        native.claude.enabled_plugins.join(", ")
    ));

    lines
}

fn render_optional_bool(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    }
}

fn render_managed_export_lines(
    reporter: &Reporter,
    managed_exports: &[PackageManagedExport],
) -> Vec<String> {
    let width = managed_exports
        .iter()
        .map(|managed_export| managed_export.source.len())
        .max()
        .unwrap_or(0);
    managed_exports
        .iter()
        .map(|managed_export| {
            let padded = format!("{:width$}", managed_export.source, width = width);
            let source = if reporter.color_enabled() {
                reporter.paint(&padded, dim_style())
            } else {
                padded
            };
            format!(
                "  {source} -> {target} ({placement}, root {resolved_root})",
                target = managed_export.target,
                placement = managed_export.placement,
                resolved_root = managed_export.resolved_root,
            )
        })
        .collect()
}

fn render_feature_lines(
    reporter: &Reporter,
    features: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    let ordered = ordered_features(features);
    let width = ordered
        .iter()
        .map(|(name, _)| name.len() + usize::from(name == "default"))
        .max()
        .unwrap_or(0);

    ordered
        .into_iter()
        .map(|(name, members)| {
            let label = if name == "default" {
                format!(
                    "{}{name:width$}",
                    reporter.paint("+", label_style()),
                    width = width
                )
            } else {
                let padded = format!("{name:width$}", width = width);
                format!(" {}", reporter.paint(&padded, dim_style()))
            };
            let members = members
                .iter()
                .map(|member| {
                    if reporter.color_enabled() {
                        reporter.paint(member, dim_style())
                    } else {
                        member.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!(" {label} = [{members}]",)
        })
        .collect()
}

fn ordered_features(features: &BTreeMap<String, Vec<String>>) -> Vec<(String, Vec<String>)> {
    let mut ordered = Vec::new();
    if let Some(default) = features.get("default") {
        ordered.push(("default".to_string(), default.clone()));
    }
    ordered.extend(
        features
            .iter()
            .filter(|(name, _)| name.as_str() != "default")
            .map(|(name, members)| (name.clone(), members.clone())),
    );
    ordered
}

fn render_components(components: Option<&[DependencyComponent]>) -> String {
    match components {
        Some(components) => components
            .iter()
            .map(|component| component.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        None => "all".to_string(),
    }
}

fn render_adapters(adapters: &[Adapter]) -> String {
    if adapters.is_empty() {
        "none".to_string()
    } else {
        adapters
            .iter()
            .map(|adapter| adapter.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn render_items(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        items.join(", ")
    }
}

fn render_dependency_name((alias, dependency): (&String, &DependencySpec)) -> String {
    if dependency.is_enabled() {
        alias.clone()
    } else {
        format!("{alias} (disabled)")
    }
}

fn short_rev(rev: &str) -> String {
    rev.chars().take(12).collect()
}

fn paint_label(reporter: &Reporter, label: &str) -> String {
    reporter.paint(label, label_style())
}

fn title_style() -> Style {
    Style::new()
        .bold()
        .fg_color(Some(AnsiColor::BrightGreen.into()))
}

fn label_style() -> Style {
    Style::new()
        .bold()
        .fg_color(Some(AnsiColor::BrightGreen.into()))
}

fn keyword_style() -> Style {
    Style::new()
        .bold()
        .fg_color(Some(AnsiColor::BrightBlue.into()))
}

fn dim_style() -> Style {
    Style::new() | Effects::DIMMED
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;
    use crate::report::ColorMode;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::File::create(path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }

    fn write_skill(path: &Path, name: &str) {
        write_file(
            &path.join("skills/review/SKILL.md"),
            &format!("---\nname: {name}\ndescription: Example skill.\n---\n# {name}\n"),
        );
    }

    fn init_git_repo(path: &Path) {
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };

        run(&["init"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test User"]);
        run(&["add", "."]);
        run(&["commit", "-m", "initial"]);
    }

    fn write_workspace_package(path: &Path) {
        write_file(
            &path.join("nodus.toml"),
            r#"
[workspace]
members = ["plugins/axiom", "plugins/firebase"]

[workspace.package.axiom]
path = "plugins/axiom"
name = "Axiom"

[workspace.package.firebase]
path = "plugins/firebase"
name = "Firebase"
"#,
        );
        write_skill(&path.join("plugins/axiom"), "Axiom");
        write_skill(&path.join("plugins/firebase"), "Firebase");
    }

    fn capture_info_output(
        cwd: &Path,
        cache_root: &Path,
        package: &str,
        tag: Option<&str>,
        branch: Option<&str>,
    ) -> String {
        capture_info_output_with_mode(cwd, cache_root, package, tag, branch, ColorMode::Never)
    }

    fn capture_info_output_with_mode(
        cwd: &Path,
        cache_root: &Path,
        package: &str,
        tag: Option<&str>,
        branch: Option<&str>,
        color_mode: ColorMode,
    ) -> String {
        let buffer = Vec::<u8>::new();
        let output = std::sync::Arc::new(std::sync::Mutex::new(buffer));

        #[derive(Clone)]
        struct SharedWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

        impl Write for SharedWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let reporter = Reporter::sink(color_mode, SharedWriter(output.clone()));
        describe_package_in_dir(cwd, cache_root, package, tag, branch, &reporter).unwrap();
        String::from_utf8(output.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn info_reads_a_local_package_directory() {
        let package = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();

        write_file(
            &package.path().join("Cargo.toml"),
            r#"
[package]
name = "playbook-ios"
version = "0.1.0"
description = "A package for review workflows"
license = "MIT"
rust-version = "1.85"
documentation = "https://docs.rs/playbook-ios"
homepage = "https://example.com/playbook-ios"
repository = "https://github.com/example/playbook-ios"
keywords = ["agents", "review"]

[features]
default = []
test-utils = []
"#,
        );
        write_file(
            &package.path().join("nodus.toml"),
            r#"
name = "playbook-ios"
version = "0.1.0"
api_version = "1"
"#,
        );
        write_skill(package.path(), "Review");

        let output = capture_info_output(package.path(), cache.path(), ".", None, None);

        assert!(output.contains("playbook-ios #agents #review"));
        assert!(output.contains("A package for review workflows"));
        assert!(output.contains("version: 0.1.0"));
        assert!(output.contains("license: MIT"));
        assert!(output.contains("rust-version: 1.85"));
        assert!(output.contains("documentation: https://docs.rs/playbook-ios"));
        assert!(output.contains("homepage: https://example.com/playbook-ios"));
        assert!(output.contains("repository: https://github.com/example/playbook-ios"));
        assert!(output.contains("alias: playbook_ios"));
        assert!(output.contains("source: path"));
        assert!(output.contains("package-root:"));
        assert!(output.contains("api-version: 1"));
        assert!(output.contains("artifacts:\n  skills = [review]"));
        assert!(output.contains("features:\n +default"));
        assert!(output.contains("  test-utils = []"));
    }

    #[test]
    fn info_reports_hook_adapter_support_matrix() {
        let package = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();

        write_file(
            &package.path().join("nodus.toml"),
            r#"
name = "fuli"

[[hooks]]
id = "fuli.claude.session-start"
event = "session_start"

[hooks.matcher]
sources = ["startup", "resume", "clear", "compact"]

[hooks.handler]
type = "command"
command = "fuli integration claude hook session-start"

[[hooks]]
id = "fuli.claude.user-prompt-submit"
event = "user_prompt_submit"

[hooks.handler]
type = "command"
command = "fuli integration claude hook user-prompt-submit"

[[hooks]]
id = "fuli.claude.post-tool-use"
event = "post_tool_use"

[hooks.handler]
type = "command"
command = "fuli integration claude hook post-tool-use"

[[hooks]]
id = "fuli.claude.stop"
event = "stop"

[hooks.handler]
type = "command"
command = "fuli integration claude hook stop"

[[hooks]]
id = "fuli.claude.subagent-stop"
event = "subagent_stop"

[hooks.handler]
type = "command"
command = "fuli integration claude hook subagent-stop"

[[hooks]]
id = "fuli.claude.session-end"
event = "session_end"

[hooks.handler]
type = "command"
command = "fuli integration claude hook session-end"
"#,
        );
        write_skill(package.path(), "Fuli Memory");

        let info = describe_package_json_in_dir(
            package.path(),
            cache.path(),
            package.path().to_str().unwrap(),
            None,
            None,
        )
        .unwrap();

        let claude = info
            .hook_adapter_support
            .iter()
            .find(|entry| entry.adapter == Adapter::Claude)
            .unwrap();
        assert_eq!(claude.supported_events.len(), 6);
        assert_eq!(
            claude.supported_events[0].session_start_sources,
            vec!["startup", "resume", "clear", "compact"]
        );

        let codex = info
            .hook_adapter_support
            .iter()
            .find(|entry| entry.adapter == Adapter::Codex)
            .unwrap();
        assert_eq!(
            codex
                .supported_events
                .iter()
                .map(|event| event.event.as_str())
                .collect::<Vec<_>>(),
            vec![
                "session_start",
                "user_prompt_submit",
                "post_tool_use",
                "stop"
            ]
        );
        assert_eq!(
            codex.supported_events[0].session_start_sources,
            vec!["startup", "resume", "clear"]
        );

        let opencode = info
            .hook_adapter_support
            .iter()
            .find(|entry| entry.adapter == Adapter::OpenCode)
            .unwrap();
        assert_eq!(
            opencode
                .supported_events
                .iter()
                .map(|event| event.event.as_str())
                .collect::<Vec<_>>(),
            vec!["session_start", "post_tool_use", "stop"]
        );
        assert_eq!(
            opencode.supported_events[0].session_start_sources,
            vec!["startup"]
        );

        let copilot = info
            .hook_adapter_support
            .iter()
            .find(|entry| entry.adapter == Adapter::Copilot)
            .unwrap();
        assert_eq!(
            copilot
                .supported_events
                .iter()
                .map(|event| event.event.as_str())
                .collect::<Vec<_>>(),
            vec![
                "session_start",
                "user_prompt_submit",
                "post_tool_use",
                "stop",
                "subagent_stop",
                "session_end"
            ]
        );
        assert_eq!(
            copilot.supported_events[0].session_start_sources,
            vec!["startup", "resume"]
        );

        let output = capture_info_output(
            package.path(),
            cache.path(),
            package.path().to_str().unwrap(),
            None,
            None,
        );
        assert!(output.contains("hook-adapter-support:"));
        assert!(output.contains("claude   = session_start(startup,resume,clear,compact)"));
        assert!(output.contains("user_prompt_submit"));
        assert!(output.contains("session_end"));
        assert!(output.contains("subagent_stop"));
        assert!(output.contains(
            "codex    = session_start(startup,resume,clear), user_prompt_submit, post_tool_use, stop"
        ));
        assert!(output.contains("opencode = session_start(startup), post_tool_use, stop"));
        assert!(output.contains("agents   = none"));
        assert!(output.contains(
            "copilot  = session_start(startup,resume), user_prompt_submit, post_tool_use, stop, subagent_stop, session_end"
        ));
    }

    #[test]
    fn info_shows_workspace_members_for_workspace_root() {
        let package = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        write_workspace_package(package.path());

        let output = capture_info_output(package.path(), cache.path(), ".", None, None);

        assert!(output.contains("workspace-members:"));
        assert!(output.contains("axiom (enabled)"));
        assert!(output.contains("firebase (enabled)"));
    }

    #[test]
    fn info_shows_disabled_workspace_members_when_dependency_members_are_omitted() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        write_workspace_package(&project.path().join("vendor/wrapper"));
        write_file(
            &project.path().join("nodus.toml"),
            r#"
[dependencies.wrapper]
path = "vendor/wrapper"
"#,
        );

        let output = capture_info_output(project.path(), cache.path(), "wrapper", None, None);

        assert!(output.contains("workspace-members:"));
        assert!(output.contains("axiom (disabled)"));
        assert!(output.contains("firebase (disabled)"));
    }

    #[test]
    fn info_lists_mcp_servers() {
        let package = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();

        write_file(
            &package.path().join("nodus.toml"),
            r#"
[mcp_servers.firebase]
command = "npx"
args = ["-y", "firebase-tools", "mcp", "--dir", "."]
"#,
        );
        write_skill(package.path(), "Review");

        let output = capture_info_output(package.path(), cache.path(), ".", None, None);

        assert!(output.contains("mcp-servers: firebase"));
    }

    #[test]
    fn info_reports_native_integration_state() {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        write_file(
            &project.path().join("nodus.toml"),
            r#"
[adapters]
enabled = ["claude", "codex"]

[dependencies]
shared = { path = "vendor/shared" }
"#,
        );
        write_file(
            &project.path().join("vendor/shared/nodus.toml"),
            r#"
name = "Shared Tools"

[activation]
always_context = ["prompts/context.md"]
"#,
        );
        write_skill(&project.path().join("vendor/shared"), "Review");
        write_file(
            &project.path().join("vendor/shared/prompts/context.md"),
            "Use the shared context.\n",
        );

        crate::resolver::sync_in_dir_with_adapters(
            project.path(),
            cache.path(),
            false,
            false,
            false,
            &[Adapter::Claude, Adapter::Codex],
            false,
            &Reporter::silent(),
        )
        .unwrap();

        let output = capture_info_output(project.path(), cache.path(), ".", None, None);

        assert!(output.contains("native-integration:"));
        assert!(output.contains("adapters = [claude, codex]"));
        assert!(output.contains(".nodus/.claude-plugin/marketplace.json (present"));
        assert!(output.contains(".nodus/.agents/plugins/marketplace.json (present"));
        assert!(output.contains("claude shared@"));
        assert!(output.contains(".nodus/packages/shared/claude-plugin"));
        assert!(output.contains("codex shared@"));
        assert!(output.contains(".nodus/packages/shared/codex-plugin"));
        assert!(output.contains("plugin_hooks=true plugin_hooks_required=true"));
        assert!(output.contains("user-config=auto"));

        let info =
            describe_package_json_in_dir(project.path(), cache.path(), ".", None, None).unwrap();
        let native = info.native_integration.unwrap();
        assert_eq!(native.adapters, vec![Adapter::Claude, Adapter::Codex]);
        assert_eq!(native.codex.hooks, Some(true));
        assert_eq!(native.codex.plugin_hooks, Some(true));
        assert!(native.codex.plugin_hooks_required);
        assert!(
            native
                .plugins
                .iter()
                .any(|plugin| plugin.adapter == Adapter::Codex && plugin.hooks.is_some())
        );
        assert!(
            native
                .claude
                .enabled_plugins
                .iter()
                .any(|plugin| plugin.starts_with("shared@"))
        );
    }

    #[test]
    fn info_lists_managed_exports() {
        let package = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();

        write_file(
            &package.path().join("nodus.toml"),
            r#"
name = "playbook-ios"

[[managed_exports]]
source = "learnings"
target = "learnings"

[[managed_exports]]
source = "prompts/review.md"
target = "docs/review.md"
placement = "project"
"#,
        );
        write_skill(package.path(), "Review");

        let output = capture_info_output(package.path(), cache.path(), ".", None, None);

        assert!(output.contains("managed-exports:"));
        assert!(output.contains(".nodus/packages/playbook-ios"));
        assert!(output.contains("learnings"));
        assert!(output.contains("prompts/review.md -> docs/review.md (project, root .)"));
    }

    #[test]
    fn info_lists_managed_exports_for_export_only_package() {
        let package = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();

        write_file(
            &package.path().join("nodus.toml"),
            r#"
name = "wenext-local-metrics"

[[managed_exports]]
source = "plugins/metrics-collector.js"
target = ".opencode/plugins/metrics-collector.js"
placement = "project"
"#,
        );
        write_file(
            &package.path().join("plugins/metrics-collector.js"),
            "export default function plugin() {}\n",
        );

        let output = capture_info_output(package.path(), cache.path(), ".", None, None);

        assert!(output.contains("managed-exports:"));
        assert!(output.contains("plugins/metrics-collector.js -> .opencode/plugins/metrics-collector.js (project, root .)"));
    }

    #[test]
    fn info_shows_dev_dependencies_for_local_package_inspection() {
        let package = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();

        write_file(
            &package.path().join("nodus.toml"),
            r#"
[dependencies]
shared = { path = "vendor/shared" }

[dev-dependencies]
tooling = { path = "vendor/tooling" }
"#,
        );
        write_skill(package.path(), "Review");
        write_skill(&package.path().join("vendor/shared"), "Shared");
        write_skill(&package.path().join("vendor/tooling"), "Tooling");

        let output = capture_info_output(package.path(), cache.path(), ".", None, None);

        assert!(output.contains("dependencies: shared"));
        assert!(output.contains("dev-dependencies: tooling"));
    }

    #[test]
    fn info_reads_a_direct_dependency_alias_from_the_root_manifest() {
        let root = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let dependency = root.path().join("vendor/playbook-ios");

        write_file(
            &root.path().join("nodus.toml"),
            r#"
[dependencies.playbook_ios]
path = "vendor/playbook-ios"
components = ["skills", "rules"]
"#,
        );
        write_file(
            &dependency.join("nodus.toml"),
            r#"
name = "playbook-ios"
version = "0.2.0"
[dev-dependencies]
tooling = { path = "vendor/tooling" }
[adapters]
enabled = ["codex"]
"#,
        );
        write_skill(&dependency, "Review");
        write_skill(&dependency.join("vendor/tooling"), "Tooling");
        write_file(&dependency.join("rules/safe.md"), "# safe\n");

        let output = capture_info_output(root.path(), cache.path(), "playbook_ios", None, None);

        assert!(output.contains("version: 0.2.0"));
        assert!(output.contains("alias: playbook_ios"));
        assert!(output.contains("components: skills, rules"));
        assert!(output.contains("adapters: codex"));
        assert!(output.contains("artifacts:"));
        assert!(output.contains("rules  = [safe]"));
        assert!(!output.contains("dev-dependencies:"));
    }

    #[test]
    fn info_marks_disabled_direct_dependencies() {
        let root = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let dependency = root.path().join("vendor/playbook-ios");

        write_file(
            &root.path().join("nodus.toml"),
            r#"
[dependencies.playbook_ios]
path = "vendor/playbook-ios"
enabled = false
"#,
        );
        write_file(&dependency.join("nodus.toml"), "name = \"playbook-ios\"\n");
        write_skill(&dependency, "Review");

        let output = capture_info_output(root.path(), cache.path(), "playbook_ios", None, None);

        assert!(output.contains("status: disabled"));
    }

    #[test]
    fn info_reads_a_git_package_reference() {
        let repo = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();

        write_file(
            &repo.path().join("nodus.toml"),
            r#"
name = "playbook-ios"
version = "0.3.0"
"#,
        );
        write_skill(repo.path(), "Review");
        init_git_repo(repo.path());

        let output = Command::new("git")
            .args(["tag", "v0.3.0"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let output = capture_info_output(
            repo.path(),
            cache.path(),
            &repo.path().to_string_lossy(),
            Some("v0.3.0"),
            None,
        );

        assert!(output.contains("version: 0.3.0"));
        assert!(output.contains("source: git"));
        assert!(output.contains("tag v0.3.0"));
    }

    #[test]
    fn info_uses_color_when_forced() {
        let package = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();

        write_file(
            &package.path().join("Cargo.toml"),
            r#"
[package]
name = "playbook-ios"
version = "0.1.0"
keywords = ["agents"]

[features]
default = []
"#,
        );

        let output = capture_info_output_with_mode(
            package.path(),
            cache.path(),
            ".",
            None,
            None,
            ColorMode::Always,
        );

        assert!(output.contains("\u{1b}["));
        assert!(output.contains("playbook-ios"));
        assert!(output.contains("#agents"));
        assert!(output.contains("version:"));
        assert!(output.contains("features:"));
    }
}
