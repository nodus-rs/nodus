use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use semver::Version;
use serde_json::Value;
use toml::Table;

use super::types::{
    ClaudeMarketplace, ClaudeMarketplaceMcpServers, ClaudeMarketplaceRemoteSource,
    ClaudeMarketplaceSource, ClaudePluginCommandSpec, ClaudePluginExtras,
    ClaudePluginHookCompatSource, ClaudePluginMcpConfig, ClaudePluginMcpSource,
    ClaudePluginMetadata, CodexMarketplace, CodexMarketplacePlugin, CodexPluginMcpConfig,
    CodexPluginMetadata, SkillFrontmatter,
};
use super::*;
use crate::adapters::Adapter;
use crate::agent_format::parse_codex_agent_config;
use crate::git::github_slug_from_url;
use crate::paths::{canonicalize_path, display_path, path_is_dir};

pub(super) fn load_manifest_str(path: &Path, contents: &str) -> Result<(Manifest, Vec<String>)> {
    let raw_value: toml::Value = toml::from_str(contents)
        .with_context(|| format!("failed to parse TOML in {}", path.display()))?;
    let raw_table = raw_value
        .as_table()
        .cloned()
        .ok_or_else(|| anyhow!("manifest root must be a TOML table"))?;
    let manifest: Manifest = raw_value.try_into()?;
    Ok((manifest, collect_ignored_field_warnings(&raw_table)))
}

pub(super) fn should_try_plugin_wrapper_fallback(loaded: &LoadedManifest) -> bool {
    loaded.manifest.workspace.is_none()
        && loaded.discovered.is_empty()
        && loaded.manifest.dependencies.is_empty()
        && loaded.manifest.mcp_servers.is_empty()
}

fn local_source_contains_nodus_manageable_content(root: &Path) -> Result<bool> {
    if root.join(MANIFEST_FILE).exists() {
        return Ok(true);
    }

    for directory in ["agents", "commands", "rules", "skills"] {
        if path_points_to_directory(&root.join(directory)) {
            return Ok(true);
        }
    }

    if let Some(extras) = read_supported_claude_plugin_extras(root)?
        && extras.has_nodus_manageable_content()
    {
        return Ok(true);
    }

    Ok([
        root.join(".mcp.json"),
        root.join(".claude-plugin").join("marketplace.json"),
        root.join(".agents")
            .join("plugins")
            .join("marketplace.json"),
    ]
    .iter()
    .any(|path| path.exists()))
}

pub(super) fn load_claude_marketplace_wrapper(
    loaded: &LoadedManifest,
) -> Result<Option<LoadedManifest>> {
    let marketplace_path = loaded.root.join(".claude-plugin").join("marketplace.json");
    if !marketplace_path.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&marketplace_path)
        .with_context(|| format!("failed to read {}", marketplace_path.display()))?;
    let marketplace: ClaudeMarketplace = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse JSON in {}", marketplace_path.display()))?;
    if marketplace.plugins.is_empty() {
        bail!(
            "{} must declare at least one plugin",
            marketplace_path.display()
        );
    }

    let mut manifest = loaded.manifest.clone();
    let mut single_plugin_version = None;
    let mut aliases = HashSet::new();
    let mut warnings = loaded.warnings.clone();
    let plugin_count = marketplace.plugins.len();
    for plugin in marketplace.plugins {
        match load_claude_marketplace_plugin(
            loaded,
            &mut manifest,
            &marketplace_path,
            &mut aliases,
            plugin,
        ) {
            Ok(plugin_version) => {
                if plugin_count == 1 {
                    single_plugin_version = plugin_version.or(single_plugin_version);
                }
            }
            Err(error) => warnings.push(error.to_string()),
        }
    }

    if manifest.version.is_none() {
        manifest.version = single_plugin_version;
    }

    Ok(Some(LoadedManifest {
        root: loaded.root.clone(),
        manifest_path: loaded.manifest_path.clone(),
        manifest,
        discovered: PackageContents::default(),
        warnings,
        claude_plugin: None,
        extra_package_files: vec![marketplace_path],
        allows_empty_dependency_wrapper: true,
        allows_unpinned_git_dependencies: true,
        manifest_contents_override: None,
    }))
}

pub(super) fn load_codex_marketplace_wrapper(
    loaded: &LoadedManifest,
) -> Result<Option<LoadedManifest>> {
    let marketplace_path = loaded
        .root
        .join(".agents")
        .join("plugins")
        .join("marketplace.json");
    if !marketplace_path.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&marketplace_path)
        .with_context(|| format!("failed to read {}", marketplace_path.display()))?;
    let marketplace: CodexMarketplace = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse JSON in {}", marketplace_path.display()))?;
    if marketplace.plugins.is_empty() {
        bail!(
            "{} must declare at least one plugin",
            marketplace_path.display()
        );
    }

    let mut manifest = loaded.manifest.clone();
    let mut single_plugin_version = None;
    let mut aliases = HashSet::new();
    let mut warnings = loaded.warnings.clone();
    let plugin_count = marketplace.plugins.len();
    for plugin in marketplace.plugins {
        match load_codex_marketplace_plugin(
            loaded,
            &mut manifest,
            &marketplace_path,
            &mut aliases,
            plugin,
        ) {
            Ok(plugin_version) => {
                if plugin_count == 1 {
                    single_plugin_version = plugin_version.or(single_plugin_version);
                }
            }
            Err(error) => warnings.push(error.to_string()),
        }
    }

    if manifest.version.is_none() {
        manifest.version = single_plugin_version;
    }

    Ok(Some(LoadedManifest {
        root: loaded.root.clone(),
        manifest_path: loaded.manifest_path.clone(),
        manifest,
        discovered: PackageContents::default(),
        warnings,
        claude_plugin: None,
        extra_package_files: vec![marketplace_path],
        allows_empty_dependency_wrapper: true,
        allows_unpinned_git_dependencies: false,
        manifest_contents_override: None,
    }))
}

fn dependency_from_claude_marketplace_source(
    plugin_name: &str,
    source: ClaudeMarketplaceRemoteSource,
    marketplace_path: &Path,
) -> Result<DependencySpec> {
    let source_kind = source.source.trim();
    if source_kind.is_empty() {
        bail!(
            "{} plugin `{plugin_name}` must declare a non-empty `source.source`",
            marketplace_path.display()
        );
    }

    let (github, url) = match source_kind {
        "url" | "git-subdir" => {
            let raw = source.url.as_deref().map(str::trim).unwrap_or_default();
            if raw.is_empty() {
                bail!(
                    "{} plugin `{plugin_name}` source kind `{source_kind}` must declare a non-empty `source.url`",
                    marketplace_path.display()
                );
            }
            match github_slug_from_url(raw) {
                Some(repo) => (Some(repo), None),
                None => (None, Some(raw.to_string())),
            }
        }
        "github" => {
            let repo = source.repo.as_deref().map(str::trim).unwrap_or_default();
            if repo.is_empty() {
                bail!(
                    "{} plugin `{plugin_name}` source kind `github` must declare a non-empty `source.repo`",
                    marketplace_path.display()
                );
            }
            (Some(repo.to_string()), None)
        }
        other => {
            bail!(
                "{} plugin `{plugin_name}` uses unsupported source kind `{other}`",
                marketplace_path.display()
            )
        }
    };

    let subpath = source
        .path
        .as_deref()
        .map(|value| {
            normalize_manifest_relative_path(value, &format!("plugin `{plugin_name}` source.path"))
        })
        .transpose()
        .with_context(|| marketplace_path.display().to_string())?;

    if source_kind == "git-subdir" && subpath.is_none() {
        bail!(
            "{} plugin `{plugin_name}` source kind `git-subdir` must declare `source.path`",
            marketplace_path.display()
        );
    }

    let revision = source
        .sha
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let branch = revision.is_none().then(|| {
        source
            .git_ref
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    });

    Ok(DependencySpec {
        github,
        url,
        path: None,
        subpath,
        tag: None,
        branch: branch.flatten(),
        revision,
        version: None,
        components: None,
        members: None,
        managed: None,
        enabled: true,
    })
}

fn load_claude_marketplace_plugin(
    loaded: &LoadedManifest,
    manifest: &mut Manifest,
    marketplace_path: &Path,
    aliases: &mut HashSet<String>,
    plugin: super::types::ClaudeMarketplacePlugin,
) -> Result<Option<Version>> {
    let name = plugin.name.trim();
    if name.is_empty() {
        bail!(
            "ignoring Claude marketplace plugin with empty name: {} plugin names must not be empty",
            marketplace_path.display()
        );
    }

    let declared_version = plugin
        .version
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            Version::parse(value).with_context(|| {
                format!(
                    "failed to parse plugin `{name}` version `{value}` in {}",
                    marketplace_path.display()
                )
            })
        })
        .transpose()?;

    let alias = normalize_dependency_alias(name)?;
    if !aliases.insert(alias.clone()) {
        bail!(
            "ignoring Claude marketplace plugin `{name}`: {} contains duplicate plugin alias `{alias}` after normalization",
            marketplace_path.display()
        );
    }

    match plugin.source {
        ClaudeMarketplaceSource::LocalPath(source) => {
            let source = source.trim();
            if source.is_empty() {
                bail!(
                    "ignoring Claude marketplace plugin `{name}`: {} plugin `{name}` must declare a non-empty `source`",
                    marketplace_path.display()
                );
            }

            let source_path = PathBuf::from(source);
            let joined_source = loaded.root.join(&source_path);
            if !joined_source.exists() {
                bail!(
                    "skipping marketplace plugin `{name}` because local source `{source}` is missing from {}",
                    loaded.root.display()
                );
            }
            let plugin_root = match loaded.resolve_existing_directory(&source_path) {
                Ok(plugin_root) => plugin_root,
                Err(error) => {
                    if fs::metadata(&joined_source)
                        .map(|metadata| metadata.is_file())
                        .unwrap_or(false)
                    {
                        bail!(
                            "ignoring Claude marketplace plugin `{name}`: plugin `{name}` source `{source}` must point to a directory"
                        );
                    }
                    return Err(error).with_context(|| {
                        format!(
                            "ignoring Claude marketplace plugin `{name}`: plugin `{name}` has invalid source `{source}`"
                        )
                    });
                }
            };
            if plugin_root == loaded.root {
                if let Some(mcp_servers) = plugin.mcp_servers {
                    import_marketplace_mcp_servers(
                        manifest,
                        name,
                        mcp_servers,
                        marketplace_path,
                        &loaded.root,
                    )?;
                } else if !loaded
                    .root
                    .join(".claude-plugin")
                    .join("plugin.json")
                    .exists()
                {
                    bail!(
                        "ignoring Claude marketplace plugin `{name}`: plugin `{name}` source `{source}` must not point at the package root"
                    );
                }
                return Ok(declared_version);
            }

            if !local_source_contains_nodus_manageable_content(&plugin_root)? {
                bail!(
                    "skipping marketplace plugin `{name}` because local source `{source}` does not expose Nodus-manageable package content"
                );
            }

            let plugin_manifest = load_dependency_from_dir(&plugin_root).with_context(|| {
                format!(
                    "ignoring Claude marketplace plugin `{name}`: plugin `{name}` source `{source}` does not match the Nodus package layout"
                )
            })?;

            manifest.dependencies.insert(
                alias,
                DependencySpec {
                    github: None,
                    url: None,
                    path: Some(source_path),
                    subpath: None,
                    tag: None,
                    branch: None,
                    revision: None,
                    version: None,
                    components: None,
                    members: None,
                    managed: None,
                    enabled: true,
                },
            );

            Ok(declared_version.or_else(|| plugin_manifest.effective_version()))
        }
        ClaudeMarketplaceSource::Remote(source) => {
            manifest.dependencies.insert(
                alias,
                dependency_from_claude_marketplace_source(name, source, marketplace_path)?,
            );
            Ok(declared_version)
        }
    }
}

fn load_codex_marketplace_plugin(
    loaded: &LoadedManifest,
    manifest: &mut Manifest,
    marketplace_path: &Path,
    aliases: &mut HashSet<String>,
    plugin: CodexMarketplacePlugin,
) -> Result<Option<Version>> {
    let name = plugin.name.trim();
    if name.is_empty() {
        bail!(
            "ignoring Codex marketplace plugin with empty name: {} plugin names must not be empty",
            marketplace_path.display()
        );
    }

    let source_kind = plugin.source.source.trim();
    if source_kind != "local" {
        bail!(
            "{} plugin `{name}` uses unsupported source kind `{source_kind}`",
            marketplace_path.display()
        );
    }

    let source = plugin.source.path.trim();
    if source.is_empty() {
        bail!(
            "ignoring Codex marketplace plugin `{name}`: {} plugin `{name}` must declare a non-empty `source.path`",
            marketplace_path.display()
        );
    }

    let alias = normalize_dependency_alias(name)?;
    if !aliases.insert(alias.clone()) {
        bail!(
            "ignoring Codex marketplace plugin `{name}`: {} contains duplicate plugin alias `{alias}` after normalization",
            marketplace_path.display()
        );
    }

    let source_path = PathBuf::from(source);
    let joined_source = loaded.root.join(&source_path);
    let plugin_root = match loaded.resolve_existing_directory(&source_path) {
        Ok(plugin_root) => plugin_root,
        Err(error) => {
            if fs::metadata(&joined_source)
                .map(|metadata| metadata.is_file())
                .unwrap_or(false)
            {
                bail!(
                    "ignoring Codex marketplace plugin `{name}`: plugin `{name}` source path `{source}` must point to a directory"
                );
            }
            return Err(error).with_context(|| {
                format!(
                    "ignoring Codex marketplace plugin `{name}`: plugin `{name}` has invalid source path `{source}`"
                )
            });
        }
    };
    if plugin_root == loaded.root {
        bail!(
            "ignoring Codex marketplace plugin `{name}`: plugin `{name}` source path `{source}` must not point at the package root"
        );
    }

    let plugin_manifest = load_dependency_from_dir(&plugin_root).with_context(|| {
        format!(
            "ignoring Codex marketplace plugin `{name}`: plugin `{name}` source path `{source}` does not match the Nodus package layout"
        )
    })?;

    manifest.dependencies.insert(
        alias,
        DependencySpec {
            github: None,
            url: None,
            path: Some(source_path),
            subpath: None,
            tag: None,
            branch: None,
            revision: None,
            version: None,
            components: None,
            members: None,
            managed: None,
            enabled: true,
        },
    );

    Ok(plugin_manifest.effective_version())
}

fn import_marketplace_mcp_servers(
    manifest: &mut Manifest,
    plugin_name: &str,
    mcp_servers: ClaudeMarketplaceMcpServers,
    marketplace_path: &Path,
    plugin_root: &Path,
) -> Result<()> {
    let servers = match mcp_servers {
        ClaudeMarketplaceMcpServers::Inline(servers) => servers,
        ClaudeMarketplaceMcpServers::Path(path) => {
            bail!(
                "{} plugin `{plugin_name}` uses unsupported `mcpServers` path `{path}`",
                marketplace_path.display()
            )
        }
    };

    let mut normalized_servers = BTreeMap::new();
    for (server_id, server) in servers {
        let server = normalize_claude_plugin_mcp_server(server, plugin_root);
        normalized_servers.insert(server_id, server);
    }

    insert_mcp_servers(
        manifest,
        normalized_servers,
        &format!(
            "{} plugin `{plugin_name}` MCP configuration",
            marketplace_path.display()
        ),
    )?;

    Ok(())
}

pub(super) fn import_claude_plugin_metadata(loaded: &mut LoadedManifest) -> Result<()> {
    let metadata_path = loaded.root.join(".claude-plugin").join("plugin.json");
    let metadata_exists = metadata_path.exists();
    let mut extras = if metadata_exists {
        let (descriptor, version) = read_claude_plugin_descriptor(&metadata_path)?;
        let extras = parse_claude_plugin_extras(
            &loaded.root,
            &descriptor,
            &metadata_path,
            &mut loaded.warnings,
        )?;
        if loaded.manifest.version.is_none()
            && let Some(version) = version.as_deref()
        {
            loaded.manifest.version = parse_plugin_metadata_version(
                version,
                "Claude plugin",
                &metadata_path,
                &mut loaded.warnings,
            );
        }
        extras
    } else {
        read_supported_claude_plugin_extras(&loaded.root)?.unwrap_or_default()
    };
    extras.hook_compat_sources.extend(
        loaded
            .manifest
            .normalized_claude_plugin_hooks()?
            .into_iter()
            .map(ClaudePluginHookCompatSource::Path),
    );
    if manifest_declares_native_claude_hooks(&loaded.manifest) {
        extras.hook_compat_sources.clear();
    }
    dedupe_claude_plugin_hook_compat_sources(&mut extras.hook_compat_sources);
    if !metadata_exists && extras.is_empty() {
        return Ok(());
    }
    loaded.allows_empty_dependency_wrapper = true;
    loaded.claude_plugin = (!extras.is_empty()).then_some(extras.clone());

    if metadata_exists {
        loaded.extra_package_files.push(metadata_path.clone());
    }

    if !extras.hook_compat_sources.is_empty() {
        loaded
            .extra_package_files
            .extend(collect_claude_plugin_runtime_files(&loaded.root, &extras)?);
    }

    let mut normalized_servers = BTreeMap::new();

    let config_path = loaded.root.join(".mcp.json");
    if config_path.exists() {
        extend_claude_plugin_mcp_servers_from_path(loaded, &mut normalized_servers, &config_path)?;
    }

    for source in &extras.mcp_servers {
        match source {
            ClaudePluginMcpSource::Inline(servers) => {
                for (server_id, server) in servers {
                    normalized_servers.insert(
                        server_id.clone(),
                        normalize_claude_plugin_mcp_server(server.clone(), &loaded.root),
                    );
                }
            }
            ClaudePluginMcpSource::Path(path) => {
                let resolved = loaded.resolve_existing_path(path).with_context(|| {
                    format!(
                        "Claude plugin manifest `mcpServers` path `{}` is invalid in {}",
                        display_path(path),
                        metadata_path.display()
                    )
                })?;
                extend_claude_plugin_mcp_servers_from_path(
                    loaded,
                    &mut normalized_servers,
                    &resolved,
                )?;
            }
        }
    }

    if !normalized_servers.is_empty() {
        insert_mcp_servers(
            &mut loaded.manifest,
            normalized_servers,
            &metadata_path.display().to_string(),
        )?;
    }

    loaded.extra_package_files.sort();
    loaded.extra_package_files.dedup();
    Ok(())
}

fn manifest_declares_native_claude_hooks(manifest: &Manifest) -> bool {
    manifest
        .hooks
        .iter()
        .any(|hook| hook.adapters.is_empty() || hook.adapters.contains(&Adapter::Claude))
}

fn dedupe_claude_plugin_hook_compat_sources(sources: &mut Vec<ClaudePluginHookCompatSource>) {
    let mut deduped = Vec::with_capacity(sources.len());
    for source in std::mem::take(sources) {
        if !deduped.contains(&source) {
            deduped.push(source);
        }
    }
    *sources = deduped;
}

fn read_supported_claude_plugin_extras(root: &Path) -> Result<Option<ClaudePluginExtras>> {
    let metadata_path = root.join(".claude-plugin").join("plugin.json");
    let mut warnings = Vec::new();

    if metadata_path.exists() {
        let (descriptor, _) = read_claude_plugin_descriptor(&metadata_path)?;
        return Ok(Some(parse_claude_plugin_extras(
            root,
            &descriptor,
            &metadata_path,
            &mut warnings,
        )?));
    }

    let mut extras = ClaudePluginExtras::default();
    let default_hooks = PathBuf::from("hooks").join("hooks.json");
    if root.join(&default_hooks).is_file() {
        extras
            .hook_compat_sources
            .push(ClaudePluginHookCompatSource::Path(default_hooks));
    }

    Ok((!extras.is_empty()).then_some(extras))
}

fn collect_claude_plugin_runtime_files(
    root: &Path,
    extras: &ClaudePluginExtras,
) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    for relative in [
        PathBuf::from(".claude-plugin").join("plugin.json"),
        PathBuf::from("hooks"),
        PathBuf::from("scripts"),
        PathBuf::from("bin"),
        PathBuf::from("settings.json"),
        PathBuf::from(".mcp.json"),
        PathBuf::from(".lsp.json"),
        PathBuf::from("monitors"),
        PathBuf::from("output-styles"),
    ] {
        collect_existing_path_files(root, &relative, &mut files)?;
    }

    for path in &extras.skills {
        collect_existing_path_files(root, path, &mut files)?;
    }
    for path in &extras.agents {
        collect_existing_path_files(root, path, &mut files)?;
    }
    for command in &extras.commands {
        collect_existing_path_files(root, &command.path, &mut files)?;
    }
    for hook in &extras.hook_compat_sources {
        if let ClaudePluginHookCompatSource::Path(path) = hook {
            collect_existing_path_files(root, path, &mut files)?;
        }
    }
    for source in &extras.mcp_servers {
        if let ClaudePluginMcpSource::Path(path) = source {
            collect_existing_path_files(root, path, &mut files)?;
        }
    }

    files.sort();
    files.dedup();
    Ok(files)
}

fn collect_existing_path_files(
    root: &Path,
    relative: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    let path = root.join(relative);
    if !path.exists() {
        return Ok(());
    }
    if path.is_file() {
        files.push(canonicalize_existing_path(&path)?);
        return Ok(());
    }
    if path.is_dir() {
        files.extend(collect_files(&path)?.into_iter().filter(|candidate| {
            candidate
                .strip_prefix(&path)
                .ok()
                .and_then(|suffix| suffix.components().next())
                .is_none_or(|component| {
                    component.as_os_str() != ".git"
                        && component.as_os_str() != ".nodus"
                        && component.as_os_str() != ".claude"
                        && component.as_os_str() != ".codex"
                        && component.as_os_str() != ".cursor"
                        && component.as_os_str() != ".github"
                        && component.as_os_str() != ".opencode"
                        && component.as_os_str() != ".agents"
                })
        }));
    }
    Ok(())
}

fn read_claude_plugin_descriptor(path: &Path) -> Result<(Value, Option<String>)> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let descriptor: Value = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse JSON in {}", path.display()))?;
    let version = descriptor
        .as_object()
        .and_then(|object| object.get("version"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok((descriptor, version))
}

fn parse_claude_plugin_extras(
    root: &Path,
    descriptor: &Value,
    metadata_path: &Path,
    warnings: &mut Vec<String>,
) -> Result<ClaudePluginExtras> {
    let object = descriptor.as_object().ok_or_else(|| {
        anyhow!(
            "failed to parse JSON in {}: root must be a JSON object",
            metadata_path.display()
        )
    })?;

    Ok(ClaudePluginExtras {
        skills: parse_claude_plugin_relative_paths(
            object.get("skills"),
            "skills",
            metadata_path,
            warnings,
        )?,
        agents: parse_claude_plugin_relative_paths(
            object.get("agents"),
            "agents",
            metadata_path,
            warnings,
        )?,
        commands: parse_claude_plugin_commands(
            root,
            object.get("commands"),
            metadata_path,
            warnings,
        )?,
        hook_compat_sources: parse_claude_plugin_hook_compat_sources(
            root,
            object.get("hooks"),
            metadata_path,
            warnings,
        )?,
        mcp_servers: parse_claude_plugin_mcp_sources(
            root,
            object.get("mcpServers"),
            metadata_path,
            warnings,
        )?,
    })
}

fn parse_claude_plugin_relative_paths(
    value: Option<&Value>,
    field: &str,
    metadata_path: &Path,
    warnings: &mut Vec<String>,
) -> Result<Vec<PathBuf>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };

    match value {
        Value::String(path) => Ok(vec![normalize_manifest_relative_path(
            Path::new(path),
            &format!(
                "Claude plugin field `{field}` in {}",
                metadata_path.display()
            ),
        )?]),
        Value::Array(items) => {
            let mut paths = Vec::new();
            for item in items {
                let Some(path) = item.as_str() else {
                    warnings.push(format!(
                        "ignoring unsupported Claude plugin field `{field}` entry in {}: expected a relative path string",
                        metadata_path.display()
                    ));
                    continue;
                };
                paths.push(normalize_manifest_relative_path(
                    Path::new(path),
                    &format!(
                        "Claude plugin field `{field}` entry in {}",
                        metadata_path.display()
                    ),
                )?);
            }
            Ok(paths)
        }
        _ => {
            warnings.push(format!(
                "ignoring unsupported Claude plugin field `{field}` in {}: expected a relative path or array of relative paths",
                metadata_path.display()
            ));
            Ok(Vec::new())
        }
    }
}

fn parse_claude_plugin_commands(
    root: &Path,
    value: Option<&Value>,
    metadata_path: &Path,
    warnings: &mut Vec<String>,
) -> Result<Vec<ClaudePluginCommandSpec>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };

    match value {
        Value::String(path) => {
            Ok(
                parse_claude_plugin_command_path(root, None, path, metadata_path, warnings)?
                    .into_iter()
                    .collect(),
            )
        }
        Value::Array(items) => {
            let mut commands = Vec::new();
            for item in items {
                let Some(path) = item.as_str() else {
                    warnings.push(format!(
                        "ignoring unsupported Claude plugin field `commands` entry in {}: expected a relative markdown path",
                        metadata_path.display()
                    ));
                    continue;
                };
                commands.extend(parse_claude_plugin_command_path(
                    root,
                    None,
                    path,
                    metadata_path,
                    warnings,
                )?);
            }
            Ok(commands)
        }
        Value::Object(entries) => {
            let mut commands = Vec::new();
            for (command_id, entry) in entries {
                let normalized_id = command_id.trim();
                if normalized_id.is_empty()
                    || normalized_id.contains('/')
                    || normalized_id.contains('\\')
                {
                    warnings.push(format!(
                        "ignoring unsupported Claude plugin command mapping `{command_id}` in {}: command ids must be non-empty and must not contain path separators",
                        metadata_path.display()
                    ));
                    continue;
                }

                let Some(entry) = entry.as_object() else {
                    warnings.push(format!(
                        "ignoring unsupported Claude plugin command mapping `{command_id}` in {}: expected an object",
                        metadata_path.display()
                    ));
                    continue;
                };

                let Some(source) = entry.get("source").and_then(Value::as_str) else {
                    if entry.get("content").is_some() {
                        warnings.push(format!(
                            "ignoring unsupported inline Claude plugin command `{command_id}` in {}: only file-backed commands are supported",
                            metadata_path.display()
                        ));
                    } else {
                        warnings.push(format!(
                            "ignoring unsupported Claude plugin command `{command_id}` in {}: expected a `source` path",
                            metadata_path.display()
                        ));
                    }
                    continue;
                };

                commands.extend(parse_claude_plugin_command_path(
                    root,
                    Some(normalized_id.to_string()),
                    source,
                    metadata_path,
                    warnings,
                )?);
            }
            Ok(commands)
        }
        _ => {
            warnings.push(format!(
                "ignoring unsupported Claude plugin field `commands` in {}: expected a relative markdown path, array of paths, or object mapping",
                metadata_path.display()
            ));
            Ok(Vec::new())
        }
    }
}

fn parse_claude_plugin_command_path(
    root: &Path,
    id: Option<String>,
    raw_path: &str,
    metadata_path: &Path,
    warnings: &mut Vec<String>,
) -> Result<Option<ClaudePluginCommandSpec>> {
    let path = normalize_manifest_relative_path(
        Path::new(raw_path),
        &format!("Claude plugin command path in {}", metadata_path.display()),
    )?;
    let joined = root.join(&path);
    let metadata = fs::metadata(&joined).with_context(|| {
        format!(
            "failed to access Claude plugin command `{}`",
            path.display()
        )
    })?;
    if metadata.is_dir() {
        warnings.push(format!(
            "ignoring unsupported Claude plugin command path `{}` in {}: directory-backed commands are not supported",
            path.display(),
            metadata_path.display()
        ));
        return Ok(None);
    }

    Ok(Some(ClaudePluginCommandSpec { id, path }))
}

fn parse_claude_plugin_hook_compat_sources(
    root: &Path,
    value: Option<&Value>,
    metadata_path: &Path,
    warnings: &mut Vec<String>,
) -> Result<Vec<ClaudePluginHookCompatSource>> {
    let Some(value) = value else {
        let default_path = PathBuf::from("hooks").join("hooks.json");
        return Ok(root
            .join(&default_path)
            .is_file()
            .then_some(ClaudePluginHookCompatSource::Path(default_path))
            .into_iter()
            .collect());
    };

    match value {
        Value::String(path) => Ok(vec![ClaudePluginHookCompatSource::Path(
            normalize_manifest_relative_path(
                Path::new(path),
                &format!(
                    "Claude plugin field `hooks` path in {}",
                    metadata_path.display()
                ),
            )?,
        )]),
        Value::Array(items) => {
            let mut sources = Vec::new();
            for item in items {
                match item {
                    Value::String(path) => sources.push(ClaudePluginHookCompatSource::Path(
                        normalize_manifest_relative_path(
                            Path::new(path),
                            &format!(
                                "Claude plugin field `hooks` path in {}",
                                metadata_path.display()
                            ),
                        )?,
                    )),
                    Value::Object(_) => {
                        sources.push(ClaudePluginHookCompatSource::Inline(item.clone()))
                    }
                    _ => warnings.push(format!(
                        "ignoring unsupported Claude plugin field `hooks` entry in {}: expected an inline object or relative JSON path",
                        metadata_path.display()
                    )),
                }
            }
            Ok(sources)
        }
        Value::Object(_) => Ok(vec![ClaudePluginHookCompatSource::Inline(value.clone())]),
        _ => {
            warnings.push(format!(
                "ignoring unsupported Claude plugin field `hooks` in {}: expected an inline object, relative JSON path, or array of those forms",
                metadata_path.display()
            ));
            Ok(Vec::new())
        }
    }
}

fn parse_claude_plugin_mcp_sources(
    root: &Path,
    value: Option<&Value>,
    metadata_path: &Path,
    warnings: &mut Vec<String>,
) -> Result<Vec<ClaudePluginMcpSource>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };

    match value {
        Value::String(path) => parse_claude_plugin_mcp_path_entry(path, metadata_path, warnings)
            .map(|path| path.into_iter().collect()),
        Value::Array(items) => {
            let mut sources = Vec::new();
            for item in items {
                match item {
                    Value::String(path) => {
                        sources.extend(parse_claude_plugin_mcp_path_entry(
                            path,
                            metadata_path,
                            warnings,
                        )?);
                    }
                    Value::Object(_) => sources.push(ClaudePluginMcpSource::Inline(
                        parse_claude_plugin_inline_mcp_servers(item.clone(), metadata_path, root)?,
                    )),
                    _ => warnings.push(format!(
                        "ignoring unsupported Claude plugin field `mcpServers` entry in {}: expected an inline object or relative JSON path",
                        metadata_path.display()
                    )),
                }
            }
            Ok(sources)
        }
        Value::Object(_) => Ok(vec![ClaudePluginMcpSource::Inline(
            parse_claude_plugin_inline_mcp_servers(value.clone(), metadata_path, root)?,
        )]),
        _ => {
            warnings.push(format!(
                "ignoring unsupported Claude plugin field `mcpServers` in {}: expected an inline object, relative JSON path, or array of those forms",
                metadata_path.display()
            ));
            Ok(Vec::new())
        }
    }
}

fn parse_claude_plugin_inline_mcp_servers(
    value: Value,
    metadata_path: &Path,
    plugin_root: &Path,
) -> Result<BTreeMap<String, McpServerConfig>> {
    let servers: BTreeMap<String, McpServerConfig> =
        serde_json::from_value(value).with_context(|| {
            format!(
                "failed to parse Claude plugin MCP servers in {}",
                metadata_path.display()
            )
        })?;
    let mut normalized = BTreeMap::new();
    for (server_id, server) in servers {
        normalized.insert(
            server_id,
            normalize_claude_plugin_mcp_server(server, plugin_root),
        );
    }
    Ok(normalized)
}

fn parse_claude_plugin_mcp_path_entry(
    raw_path: &str,
    metadata_path: &Path,
    warnings: &mut Vec<String>,
) -> Result<Vec<ClaudePluginMcpSource>> {
    if !is_supported_claude_plugin_mcp_path(raw_path) {
        warnings.push(format!(
            "ignoring unsupported Claude plugin field `mcpServers` path `{raw_path}` in {}",
            metadata_path.display()
        ));
        return Ok(Vec::new());
    }

    Ok(vec![ClaudePluginMcpSource::Path(
        normalize_manifest_relative_path(
            Path::new(raw_path),
            &format!(
                "Claude plugin field `mcpServers` path in {}",
                metadata_path.display()
            ),
        )?,
    )])
}

fn is_supported_claude_plugin_mcp_path(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty()
        && !trimmed.contains("://")
        && !trimmed.ends_with(".mcpb")
        && Path::new(trimmed)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
}

fn extend_claude_plugin_mcp_servers_from_path(
    loaded: &mut LoadedManifest,
    target: &mut BTreeMap<String, McpServerConfig>,
    path: &Path,
) -> Result<()> {
    if !path.is_file() {
        bail!(
            "Claude plugin MCP config {} must point to a file",
            path.display()
        );
    }

    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let config: ClaudePluginMcpConfig = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse JSON in {}", path.display()))?;
    let servers = match config {
        ClaudePluginMcpConfig::Wrapped { mcp_servers } => mcp_servers,
        ClaudePluginMcpConfig::Flat(servers) => servers,
    };

    for (server_id, server) in servers {
        target.insert(
            server_id,
            normalize_claude_plugin_mcp_server(server, &loaded.root),
        );
    }
    loaded
        .extra_package_files
        .push(canonicalize_existing_path(path)?);
    Ok(())
}

pub(super) fn import_codex_plugin_metadata(loaded: &mut LoadedManifest) -> Result<()> {
    let metadata_path = loaded.root.join(".codex-plugin").join("plugin.json");
    if !metadata_path.exists() {
        return Ok(());
    }
    loaded.allows_empty_dependency_wrapper = true;

    let contents = fs::read_to_string(&metadata_path)
        .with_context(|| format!("failed to read {}", metadata_path.display()))?;
    let metadata: CodexPluginMetadata = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse JSON in {}", metadata_path.display()))?;

    if loaded.manifest.version.is_none()
        && let Some(version) = metadata.version.as_deref()
    {
        loaded.manifest.version = parse_plugin_metadata_version(
            version,
            "Codex plugin",
            &metadata_path,
            &mut loaded.warnings,
        );
    }

    loaded.extra_package_files.push(metadata_path.clone());

    let Some(mcp_servers_path) = metadata.mcp_servers.as_deref() else {
        return Ok(());
    };

    let mcp_servers_path = mcp_servers_path.trim();
    if mcp_servers_path.is_empty() {
        return Ok(());
    }

    let config_path = loaded
        .resolve_existing_path(Path::new(mcp_servers_path))
        .with_context(|| {
            format!("Codex plugin metadata `mcpServers` path `{mcp_servers_path}` is invalid")
        })?;
    if !config_path.is_file() {
        bail!("Codex plugin metadata `mcpServers` path `{mcp_servers_path}` must point to a file");
    }

    let contents = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let config: CodexPluginMcpConfig = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse JSON in {}", config_path.display()))?;
    let mut normalized_servers = BTreeMap::new();
    for (server_id, server) in config.mcp_servers {
        let server = normalize_claude_plugin_mcp_server(server, &loaded.root);
        normalized_servers.insert(server_id, server);
    }
    insert_mcp_servers(
        &mut loaded.manifest,
        normalized_servers,
        &config_path.display().to_string(),
    )?;

    loaded.extra_package_files.push(config_path);
    loaded.extra_package_files.sort();
    loaded.extra_package_files.dedup();
    Ok(())
}

pub(super) fn import_opencode_plugin_hooks(loaded: &mut LoadedManifest) -> Result<()> {
    let hooks = loaded.manifest.normalized_opencode_plugin_hooks()?;
    if hooks.is_empty() {
        return Ok(());
    }

    loaded.allows_empty_dependency_wrapper = true;
    let mut files = Vec::new();
    for hook in hooks {
        let resolved = loaded.resolve_existing_path(&hook).with_context(|| {
            format!(
                "manifest field `opencode_plugin_hooks` entry `{}` is invalid",
                display_path(&hook)
            )
        })?;
        files.push(resolved);
        if let Some(parent) = hook.parent()
            && parent != Path::new("")
        {
            collect_existing_path_files(&loaded.root, parent, &mut files)?;
        }
    }

    loaded.extra_package_files.extend(files);
    loaded.extra_package_files.sort();
    loaded.extra_package_files.dedup();
    Ok(())
}

fn insert_mcp_servers(
    manifest: &mut Manifest,
    servers: BTreeMap<String, McpServerConfig>,
    source_label: &str,
) -> Result<()> {
    for (server_id, server) in servers {
        let id = server_id.trim();
        if id.is_empty() {
            bail!("{source_label} contains an empty MCP server id");
        }
        if let Some(existing) = manifest.mcp_servers.get(id) {
            if existing == &server {
                continue;
            }
            bail!("{source_label} declares conflicting MCP server `{id}`");
        }
        manifest.mcp_servers.insert(id.to_string(), server);
    }

    Ok(())
}

fn normalize_claude_plugin_root_arg(value: &str, plugin_root: &Path) -> String {
    if value == "${CLAUDE_PLUGIN_ROOT}" {
        return display_path(plugin_root);
    }
    if let Some(suffix) = value.strip_prefix("${CLAUDE_PLUGIN_ROOT}/") {
        return display_path(&plugin_root.join(suffix));
    }

    value.to_string()
}

fn normalize_claude_plugin_root_cwd(cwd: &Path, plugin_root: &Path) -> PathBuf {
    let cwd_display = display_path(cwd);
    if cwd_display == "${CLAUDE_PLUGIN_ROOT}" {
        return plugin_root.to_path_buf();
    }
    if let Some(suffix) = cwd_display.strip_prefix("${CLAUDE_PLUGIN_ROOT}/") {
        return plugin_root.join(suffix);
    }

    cwd.to_path_buf()
}

fn normalize_claude_plugin_mcp_server(
    mut server: McpServerConfig,
    plugin_root: &Path,
) -> McpServerConfig {
    let mut normalized_args = Vec::with_capacity(server.args.len());
    let mut index = 0;
    while index < server.args.len() {
        if server.args[index] == "--cwd"
            && server
                .args
                .get(index + 1)
                .is_some_and(|value| value == "${CLAUDE_PLUGIN_ROOT}")
        {
            server.cwd.get_or_insert_with(|| plugin_root.to_path_buf());
            index += 2;
            continue;
        }

        normalized_args.push(normalize_claude_plugin_root_arg(
            &server.args[index],
            plugin_root,
        ));
        index += 1;
    }
    server.args = normalized_args;
    if let Some(cwd) = &server.cwd {
        server.cwd = Some(normalize_claude_plugin_root_cwd(cwd, plugin_root));
    }
    server
}

pub(super) fn discover_package_contents(
    root: &Path,
    manifest: &Manifest,
    claude_plugin: Option<&ClaudePluginExtras>,
) -> Result<PackageContents> {
    let mut skills = Vec::new();
    let mut skill_ids = HashSet::new();
    let mut agents = Vec::new();
    let mut agent_variants = HashSet::new();
    let mut rules = Vec::new();
    let mut rule_ids = HashSet::new();
    let mut commands = Vec::new();
    let mut command_ids = HashSet::new();

    for discovery_root in discovery_roots(root, manifest)? {
        merge_skill_entries(
            &mut skills,
            &mut skill_ids,
            discover_skills(root, &discovery_root)?,
        )?;
        merge_agent_entries(
            &mut agents,
            &mut agent_variants,
            discover_agents(root, &discovery_root)?,
        )?;
        merge_file_entries(
            &mut rules,
            &mut rule_ids,
            "rule",
            discover_files(root, &discovery_root, "rules", false, true)?,
        )?;
        merge_file_entries(
            &mut commands,
            &mut command_ids,
            "command",
            discover_files(root, &discovery_root, "commands", false, true)?,
        )?;
    }

    if let Some(claude_plugin) = claude_plugin {
        merge_skill_entries(
            &mut skills,
            &mut skill_ids,
            discover_explicit_skill_roots(root, &claude_plugin.skills)?,
        )?;
        merge_agent_entries(
            &mut agents,
            &mut agent_variants,
            discover_explicit_agents(root, &claude_plugin.agents, Some(("agents", false)))?,
        )?;
        merge_file_entries(
            &mut commands,
            &mut command_ids,
            "command",
            discover_explicit_commands(root, &claude_plugin.commands)?,
        )?;
    }

    skills.sort_by(|left, right| left.id.cmp(&right.id));
    agents.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then(left.path.cmp(&right.path))
            .then(left.qualifiers.cmp(&right.qualifiers))
            .then(left.format.cmp(&right.format))
    });
    rules.sort_by(|left, right| left.id.cmp(&right.id));
    commands.sort_by(|left, right| left.id.cmp(&right.id));

    Ok(PackageContents {
        skills,
        agents,
        rules,
        commands,
    })
}

fn discovery_roots(root: &Path, manifest: &Manifest) -> Result<Vec<PathBuf>> {
    let mut roots = vec![PathBuf::new()];
    for content_root in manifest.normalized_content_roots()? {
        let resolved = canonicalize_existing_directory_path(&root.join(&content_root))
            .with_context(|| {
                format!(
                    "manifest field `content_roots` contains invalid path `{}`",
                    content_root.display()
                )
            })?;
        if !resolved.starts_with(root) {
            bail!(
                "manifest field `content_roots` path `{}` escapes the package root",
                content_root.display()
            );
        }
        roots.push(content_root);
    }
    Ok(roots)
}

fn merge_skill_entries(
    destination: &mut Vec<SkillEntry>,
    ids: &mut HashSet<String>,
    discovered: Vec<SkillEntry>,
) -> Result<()> {
    for skill in discovered {
        if skill
            .id
            .starts_with(crate::adapters::codex::SYNTHETIC_COMMAND_SKILL_PREFIX)
        {
            bail!(
                "skill id `{}` uses reserved prefix `{}` for generated Codex command compatibility",
                skill.id,
                crate::adapters::codex::SYNTHETIC_COMMAND_SKILL_PREFIX
            );
        }
        if !ids.insert(skill.id.clone()) {
            if destination.iter().any(|existing| existing == &skill) {
                continue;
            }
            bail!("duplicate skill id `{}`", skill.id);
        }
        destination.push(skill);
    }
    Ok(())
}

fn merge_agent_entries(
    destination: &mut Vec<AgentEntry>,
    variants: &mut HashSet<String>,
    discovered: Vec<AgentEntry>,
) -> Result<()> {
    for agent in discovered {
        let variant_key = agent_variant_key(&agent);
        if !variants.insert(variant_key.clone()) {
            if destination.iter().any(|existing| existing == &agent) {
                continue;
            }
            bail!("duplicate agent variant `{variant_key}`");
        }
        destination.push(agent);
    }
    Ok(())
}

fn merge_file_entries(
    destination: &mut Vec<FileEntry>,
    ids: &mut HashSet<String>,
    singular: &str,
    discovered: Vec<FileEntry>,
) -> Result<()> {
    for entry in discovered {
        if !ids.insert(entry.id.clone()) {
            if destination.iter().any(|existing| existing == &entry) {
                continue;
            }
            bail!("duplicate {singular} id `{}`", entry.id);
        }
        destination.push(entry);
    }
    Ok(())
}

fn discover_skills(root: &Path, discovery_root: &Path) -> Result<Vec<SkillEntry>> {
    let skills_relative_root = discovery_root.join("skills");
    let skills_root = root.join(&skills_relative_root);
    if !skills_root.exists() {
        return Ok(Vec::new());
    }
    let resolved_skills_root = canonicalize_existing_directory_path(&skills_root)
        .map_err(|_| anyhow!("`{}` must be a directory", skills_relative_root.display()))?;
    if !resolved_skills_root.starts_with(root) {
        bail!(
            "`{}` escapes the package root",
            skills_relative_root.display()
        );
    }
    if !resolved_skills_root.is_dir() {
        bail!("`{}` must be a directory", skills_relative_root.display());
    }

    let mut skills = Vec::new();
    discover_skills_in_dir(
        root,
        &skills_relative_root,
        &skills_relative_root,
        &resolved_skills_root,
        &mut skills,
    )?;

    skills.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(skills)
}

fn discover_skills_in_dir(
    root: &Path,
    skills_relative_root: &Path,
    current_relative_dir: &Path,
    current_dir: &Path,
    skills: &mut Vec<SkillEntry>,
) -> Result<bool> {
    let mut found_skill = false;

    for entry in fs::read_dir(current_dir)
        .with_context(|| format!("failed to read {}", current_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if should_ignore_discovery_entry(&path) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let relative = current_relative_dir.join(&name);
        let skill_dir = canonicalize_existing_directory_path(&path).map_err(|_| {
            anyhow!(
                "`{}` entries must be directories",
                skills_relative_root.display()
            )
        })?;
        let skill_file = skill_dir.join("SKILL.md");
        if skill_file.is_file() {
            let relative_under_skills = relative
                .strip_prefix(skills_relative_root)
                .with_context(|| format!("failed to make {} relative", relative.display()))?;
            let id = derive_file_entry_id(relative_under_skills)?;
            if !skill_dir.starts_with(root) {
                bail!("skill `{id}` escapes the package root");
            }
            validate_skill_directory(&skill_dir, &name)
                .with_context(|| format!("skill `{id}` is invalid"))?;
            skills.push(SkillEntry { id, path: relative });
            found_skill = true;
            continue;
        }

        if !skill_dir.starts_with(root) {
            bail!(
                "`{}` entries must stay within the package root",
                skills_relative_root.display()
            );
        }
        found_skill |=
            discover_skills_in_dir(root, skills_relative_root, &relative, &skill_dir, skills)?;
    }

    Ok(found_skill)
}

fn discover_agents(root: &Path, discovery_root: &Path) -> Result<Vec<AgentEntry>> {
    let dir_relative_root = discovery_root.join("agents");
    let dir_root = root.join(&dir_relative_root);
    if !dir_root.exists() {
        return Ok(Vec::new());
    }
    let resolved_dir_root = canonicalize_existing_directory_path(&dir_root)
        .map_err(|_| anyhow!("`{}` must be a directory", dir_relative_root.display()))?;
    if !resolved_dir_root.starts_with(root) {
        bail!("`{}` escapes the package root", dir_relative_root.display());
    }
    if !resolved_dir_root.is_dir() {
        bail!("`{}` must be a directory", dir_relative_root.display());
    }

    let mut items = Vec::new();
    let walker = walkdir::WalkDir::new(&resolved_dir_root).min_depth(1);
    for entry in walker {
        let entry = entry?;
        let path = entry.path();
        if should_ignore_discovery_entry(path) {
            continue;
        }
        if entry.file_type().is_dir() {
            continue;
        }
        if !entry.file_type().is_file() {
            bail!("`{}` entries must be files", dir_relative_root.display());
        }

        let relative = path
            .strip_prefix(&resolved_dir_root)
            .with_context(|| format!("failed to make {} relative", path.display()))?;
        let logical_path = dir_relative_root.join(relative);
        let agent = derive_agent_entry(&logical_path, relative)?;

        let canonical = canonicalize_existing_path(path)?;
        if !canonical.starts_with(root) {
            bail!("`agents` item `{}` escapes the package root", agent.id);
        }
        validate_agent_entry(path, &agent)?;
        items.push(agent);
    }

    items.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then(left.path.cmp(&right.path))
            .then(left.qualifiers.cmp(&right.qualifiers))
            .then(left.format.cmp(&right.format))
    });
    Ok(items)
}

fn discover_files(
    root: &Path,
    discovery_root: &Path,
    directory: &str,
    markdown_only: bool,
    recursive: bool,
) -> Result<Vec<FileEntry>> {
    let dir_relative_root = discovery_root.join(directory);
    let dir_root = root.join(&dir_relative_root);
    if !dir_root.exists() {
        return Ok(Vec::new());
    }
    let resolved_dir_root = canonicalize_existing_directory_path(&dir_root)
        .map_err(|_| anyhow!("`{}` must be a directory", dir_relative_root.display()))?;
    if !resolved_dir_root.starts_with(root) {
        bail!("`{}` escapes the package root", dir_relative_root.display());
    }
    if !resolved_dir_root.is_dir() {
        bail!("`{}` must be a directory", dir_relative_root.display());
    }

    let mut items = Vec::new();
    let walker = if recursive {
        walkdir::WalkDir::new(&resolved_dir_root).min_depth(1)
    } else {
        walkdir::WalkDir::new(&resolved_dir_root)
            .min_depth(1)
            .max_depth(1)
    };

    for entry in walker {
        let entry = entry?;
        let path = entry.path();
        if should_ignore_discovery_entry(path) {
            if entry.file_type().is_dir() {
                continue;
            }
            continue;
        }
        if entry.file_type().is_dir() {
            continue;
        }
        if !entry.file_type().is_file() {
            bail!("`{}` entries must be files", dir_relative_root.display());
        }

        if markdown_only && path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            bail!(
                "`{}` entries must use the `.md` extension",
                dir_relative_root.display()
            );
        }

        let relative = path
            .strip_prefix(&resolved_dir_root)
            .with_context(|| format!("failed to make {} relative", path.display()))?;
        let id = derive_file_entry_id(relative)?;

        let relative = dir_relative_root.join(relative);
        let canonical = canonicalize_existing_path(path)?;
        if !canonical.starts_with(root) {
            bail!("`{directory}` item `{id}` escapes the package root");
        }
        items.push(FileEntry { id, path: relative });
    }

    items.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(items)
}

fn discover_explicit_skill_roots(root: &Path, skill_roots: &[PathBuf]) -> Result<Vec<SkillEntry>> {
    let mut skills = Vec::new();
    for skill_root in skill_roots {
        let resolved_root =
            canonicalize_existing_directory_path(&root.join(skill_root)).map_err(|_| {
                anyhow!(
                    "Claude plugin skill root `{}` must be a directory",
                    skill_root.display()
                )
            })?;
        if !resolved_root.starts_with(root) {
            bail!(
                "Claude plugin skill root `{}` escapes the package root",
                skill_root.display()
            );
        }
        discover_skills_in_dir(root, skill_root, skill_root, &resolved_root, &mut skills)?;
    }
    skills.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(skills)
}

fn discover_explicit_agents(
    root: &Path,
    files: &[PathBuf],
    standard_root: Option<(&str, bool)>,
) -> Result<Vec<AgentEntry>> {
    let mut items = Vec::new();
    for relative_path in files {
        let canonical =
            canonicalize_existing_path(&root.join(relative_path)).with_context(|| {
                format!(
                    "failed to resolve Claude plugin file `{}`",
                    relative_path.display()
                )
            })?;
        if !canonical.starts_with(root) {
            bail!(
                "Claude plugin file `{}` escapes the package root",
                relative_path.display()
            );
        }
        if canonical.is_dir() {
            bail!(
                "Claude plugin file `{}` must point to a file",
                relative_path.display()
            );
        }
        let agent = derive_explicit_agent_entry(relative_path, standard_root)?;
        validate_agent_entry(&canonical, &agent)?;
        items.push(agent);
    }

    items.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then(left.path.cmp(&right.path))
            .then(left.qualifiers.cmp(&right.qualifiers))
            .then(left.format.cmp(&right.format))
    });
    Ok(items)
}

fn discover_explicit_commands(
    root: &Path,
    commands: &[ClaudePluginCommandSpec],
) -> Result<Vec<FileEntry>> {
    let mut items = Vec::new();
    for command in commands {
        let joined = root.join(&command.path);
        let metadata = fs::metadata(&joined).with_context(|| {
            format!(
                "failed to access Claude plugin command `{}`",
                command.path.display()
            )
        })?;
        if metadata.is_dir() {
            continue;
        }

        let canonical = canonicalize_existing_path(&joined).with_context(|| {
            format!(
                "failed to resolve Claude plugin command `{}`",
                command.path.display()
            )
        })?;
        if !canonical.starts_with(root) {
            bail!(
                "Claude plugin command `{}` escapes the package root",
                command.path.display()
            );
        }
        if canonical
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("md")
        {
            bail!(
                "Claude plugin command `{}` must use the `.md` extension",
                command.path.display()
            );
        }
        items.push(FileEntry {
            id: match &command.id {
                Some(id) => id.clone(),
                None => derive_explicit_file_entry_id(&command.path, Some(("commands", true)))?,
            },
            path: command.path.clone(),
        });
    }

    items.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(items)
}

fn should_ignore_discovery_entry(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    name.starts_with('.') || name.eq_ignore_ascii_case("README.md")
}

fn derive_agent_entry(logical_path: &Path, relative: &Path) -> Result<AgentEntry> {
    let (id, qualifiers, format) = parse_agent_relative_path(relative)?;
    Ok(AgentEntry {
        id,
        path: logical_path.to_path_buf(),
        qualifiers,
        format,
    })
}

fn derive_explicit_agent_entry(
    relative: &Path,
    standard_root: Option<(&str, bool)>,
) -> Result<AgentEntry> {
    if let Some((root, allow_nested)) = standard_root
        && let Ok(stripped) = relative.strip_prefix(root)
        && (allow_nested || stripped.components().count() == 1)
    {
        return derive_agent_entry(relative, stripped);
    }

    derive_agent_entry(relative, relative)
}

fn parse_agent_relative_path(relative: &Path) -> Result<(String, Vec<String>, String)> {
    let mut id_parts = Vec::new();
    let mut components = relative.components().peekable();

    while let Some(component) = components.next() {
        let Component::Normal(value) = component else {
            bail!("failed to derive agent id from {}", relative.display());
        };
        let value = value
            .to_str()
            .ok_or_else(|| anyhow!("failed to derive agent id from {}", relative.display()))?;
        if components.peek().is_none() {
            let segments = value.split('.').collect::<Vec<_>>();
            if segments.len() < 2 {
                bail!(
                    "agent file `{}` must include a format extension",
                    relative.display()
                );
            }
            let format = segments.last().copied().ok_or_else(|| {
                anyhow!("failed to derive agent format from {}", relative.display())
            })?;
            if format.is_empty() {
                bail!(
                    "agent file `{}` must include a non-empty format extension",
                    relative.display()
                );
            }
            let id_head = segments[0];
            if id_head.is_empty() {
                bail!(
                    "agent file `{}` must not start with `.`",
                    relative.display()
                );
            }
            id_parts.push(id_head.to_string());
            let qualifiers = segments[1..segments.len() - 1]
                .iter()
                .map(|segment| segment.to_string())
                .collect::<Vec<_>>();
            return Ok((id_parts.join("__"), qualifiers, format.to_string()));
        }
        id_parts.push(value.to_string());
    }

    bail!("failed to derive agent id from {}", relative.display());
}

fn validate_agent_entry(path: &Path, agent: &AgentEntry) -> Result<()> {
    if !agent_uses_known_codex_toml(agent) {
        return Ok(());
    }

    let contents = fs::read(path)
        .with_context(|| format!("failed to read agent source {}", path.display()))?;
    parse_codex_agent_config(&contents, &format!("agent `{}`", display_path(&agent.path)))?;
    Ok(())
}

fn agent_uses_known_codex_toml(agent: &AgentEntry) -> bool {
    agent.format.eq_ignore_ascii_case("toml")
        && (agent.qualifiers.is_empty()
            || (agent.qualifiers.len() == 1 && agent.qualifiers[0].eq_ignore_ascii_case("codex")))
}

fn agent_variant_key(agent: &AgentEntry) -> String {
    format!(
        "{}|{}|{}",
        agent.id,
        agent.qualifiers.join("."),
        agent.format
    )
}

fn derive_file_entry_id(relative: &Path) -> Result<String> {
    let stemmed = relative.with_extension("");
    let parts = stemmed
        .iter()
        .map(|value| {
            value
                .to_str()
                .ok_or_else(|| anyhow!("failed to derive id from {}", relative.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    let id = parts.join("__");
    if id.is_empty() {
        bail!("failed to derive id from {}", relative.display());
    }
    Ok(id)
}

fn derive_explicit_file_entry_id(
    relative: &Path,
    standard_root: Option<(&str, bool)>,
) -> Result<String> {
    if let Some((root, allow_nested)) = standard_root
        && let Ok(stripped) = relative.strip_prefix(root)
        && (allow_nested || stripped.components().count() == 1)
    {
        return derive_file_entry_id(stripped);
    }

    derive_file_entry_id(relative)
}

fn validate_skill_directory(skill_dir: &Path, fallback_name: &str) -> Result<()> {
    let skill_file = skill_dir.join("SKILL.md");
    let contents = fs::read_to_string(&skill_file)
        .with_context(|| format!("failed to read {}", skill_file.display()))?;
    let frontmatter = parse_skill_frontmatter(&contents)?;
    let resolved_name = frontmatter
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(fallback_name);
    if resolved_name.is_empty() {
        bail!("skill directory name must not be empty");
    }
    if frontmatter.description.trim().is_empty() {
        bail!("`SKILL.md` frontmatter field `description` must not be empty");
    }
    Ok(())
}

fn parse_skill_frontmatter(contents: &str) -> Result<SkillFrontmatter> {
    let mut lines = contents.lines();
    if lines.next() != Some("---") {
        bail!("`SKILL.md` must begin with YAML frontmatter");
    }

    let mut yaml = Vec::new();
    for line in lines {
        if line == "---" {
            let yaml = yaml.join("\n");
            return match serde_yaml::from_str(&yaml) {
                Ok(frontmatter) => Ok(frontmatter),
                Err(error) => parse_simple_frontmatter(&yaml).with_context(|| {
                    format!("failed to parse SKILL.md frontmatter as YAML: {error}")
                }),
            };
        }
        yaml.push(line);
    }

    bail!("`SKILL.md` frontmatter is missing a closing `---`");
}

fn parse_simple_frontmatter(yaml: &str) -> Result<SkillFrontmatter> {
    let mut name = None;
    let mut description = None;

    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let Some((key, value)) = trimmed.split_once(':') else {
            bail!("frontmatter line `{trimmed}` is not a simple `key: value` entry");
        };
        let key = key.trim();
        let value = unquote_frontmatter_value(value.trim());

        match key {
            "name" => name = Some(value),
            "description" => description = Some(value),
            _ => {}
        }
    }

    Ok(SkillFrontmatter {
        name,
        description: description.ok_or_else(|| anyhow!("missing `description` in frontmatter"))?,
    })
}

fn unquote_frontmatter_value(value: &str) -> String {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let first = bytes[0] as char;
        let last = bytes[value.len() - 1] as char;
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

pub(super) fn collect_ignored_field_warnings(table: &Table) -> Vec<String> {
    const SUPPORTED_ROOT_FIELDS: &[&str] = &[
        "api_version",
        "name",
        "version",
        "content_roots",
        "publish_root",
        "managed_exports",
        "capabilities",
        "mcp_servers",
        "adapters",
        "hooks",
        "claude_plugin_hooks",
        "opencode_plugin_hooks",
        "launch_hooks",
        "workspace",
        "dependencies",
        "dev-dependencies",
    ];

    let mut warnings = Vec::new();
    for key in table.keys() {
        if !SUPPORTED_ROOT_FIELDS.contains(&key.as_str()) {
            warnings.push(format!("ignoring unsupported manifest field `{key}`"));
        }
    }
    warnings.sort();
    warnings
}

pub(super) fn validate_dependency_managed_specs(
    alias: &str,
    managed: Option<&[ManagedPathSpec]>,
) -> Result<()> {
    let Some(managed) = managed else {
        return Ok(());
    };
    if managed.is_empty() {
        bail!("dependency `{alias}` field `managed` must not be empty");
    }

    let mut seen = HashSet::new();
    for mapping in managed {
        let normalized_source = mapping
            .normalized_source()
            .with_context(|| format!("dependency `{alias}` field `managed.source` is invalid"))?;
        let normalized_target = mapping
            .normalized_target()
            .with_context(|| format!("dependency `{alias}` field `managed.target` is invalid"))?;
        if !seen.insert((normalized_source, normalized_target)) {
            bail!(
                "dependency `{alias}` field `managed` must not contain duplicate source/target pairs"
            );
        }
    }

    Ok(())
}

pub(super) fn validate_managed_export_specs(managed_exports: &[ManagedExportSpec]) -> Result<()> {
    if managed_exports.is_empty() {
        return Ok(());
    }

    let mut seen = HashSet::new();
    for mapping in managed_exports {
        let normalized_source = mapping
            .normalized_source()
            .context("manifest field `managed_exports.source` is invalid")?;
        let normalized_target = mapping
            .normalized_target()
            .context("manifest field `managed_exports.target` is invalid")?;
        if !seen.insert((normalized_source, normalized_target, mapping.placement)) {
            bail!("manifest field `managed_exports` must not contain duplicate entries");
        }
    }

    Ok(())
}

pub(super) fn normalize_manifest_relative_path(value: &Path, label: &str) -> Result<PathBuf> {
    if value.as_os_str().is_empty() {
        bail!("{label} must not be empty");
    }
    if value.is_absolute() {
        bail!("{label} must be relative");
    }

    let mut normalized = PathBuf::new();
    for component in value.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => bail!("{label} must not contain `..`"),
            Component::RootDir | Component::Prefix(_) => bail!("{label} must be relative"),
        }
    }

    if normalized.as_os_str().is_empty() {
        bail!("{label} must not be empty");
    }

    Ok(normalized)
}

pub(super) fn collect_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() {
            files.push(canonicalize_existing_path(entry.path())?);
        }
    }
    files.sort();
    Ok(files)
}

pub fn normalize_dependency_alias(value: &str) -> Result<String> {
    let mut alias = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            alias.push(character.to_ascii_lowercase());
        } else if !alias.ends_with('_') {
            alias.push('_');
        }
    }

    let alias = alias.trim_matches('_').to_string();
    if alias.is_empty() {
        bail!("failed to derive a valid dependency alias from `{value}`");
    }
    Ok(alias)
}

pub(super) fn load_claude_plugin_version(
    root: &Path,
    warnings: &mut Vec<String>,
) -> Result<Option<Version>> {
    let metadata_path = root.join("claude-code.json");
    if !metadata_path.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&metadata_path)
        .with_context(|| format!("failed to read {}", metadata_path.display()))?;
    let metadata: ClaudePluginMetadata = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse JSON in {}", metadata_path.display()))?;
    let Some(version) = metadata.version else {
        return Ok(None);
    };

    Ok(parse_plugin_metadata_version(
        &version,
        "Claude plugin",
        &metadata_path,
        warnings,
    ))
}

fn parse_plugin_metadata_version(
    raw_version: &str,
    plugin_kind: &str,
    metadata_path: &Path,
    warnings: &mut Vec<String>,
) -> Option<Version> {
    let version = raw_version.trim();
    if version.is_empty() {
        return None;
    }

    match Version::parse(version) {
        Ok(version) => Some(version),
        Err(_) => {
            warnings.push(format!(
                "ignoring non-SemVer {plugin_kind} version `{version}` in {}",
                display_path(metadata_path)
            ));
            None
        }
    }
}

pub(super) fn canonicalize_existing_path(path: &Path) -> Result<PathBuf> {
    canonicalize_path(path).with_context(|| format!("failed to canonicalize {}", path.display()))
}

pub(super) fn canonicalize_existing_directory_path(path: &Path) -> Result<PathBuf> {
    if path_is_dir(path) {
        return canonicalize_existing_path(path);
    }

    if let Some(canonical) = try_resolve_directory_placeholder(path)? {
        return Ok(canonical);
    }

    let canonical = canonicalize_existing_path(path)?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        bail!("{} is not a directory", display_path(path));
    }
}

fn try_resolve_directory_placeholder(path: &Path) -> Result<Option<PathBuf>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to access {}", path.display()));
        }
    };
    if !metadata.is_file() {
        return Ok(None);
    }

    let raw_target =
        fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let Ok(raw_target) = String::from_utf8(raw_target) else {
        return Ok(None);
    };
    let raw_target = raw_target.trim_end_matches(['\r', '\n']);
    if raw_target.is_empty() {
        return Ok(None);
    }

    let target = PathBuf::from(raw_target);
    let target = if target.is_absolute() {
        target
    } else {
        path.parent()
            .map(|parent| parent.join(&target))
            .unwrap_or(target)
    };

    match canonicalize_path(&target) {
        Ok(canonical) if canonical.is_dir() => Ok(Some(canonical)),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to resolve directory placeholder {} -> {}",
                display_path(path),
                display_path(&target)
            )
        }),
    }
}

fn path_points_to_directory(path: &Path) -> bool {
    path_is_dir(path)
        || try_resolve_directory_placeholder(path)
            .ok()
            .flatten()
            .is_some()
}

pub(super) fn default_manifest_contents() -> &'static str {
    ""
}

pub(super) fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub(super) fn default_skill_contents() -> &'static str {
    "---\nname: Example\ndescription: Describe what this skill helps with.\n---\n# Example\n"
}

pub(super) fn default_package_name(root: &Path) -> String {
    let name = root
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("agentpack");
    let mut normalized = String::new();

    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            normalized.push(character.to_ascii_lowercase());
        } else if !normalized.ends_with('-') {
            normalized.push('-');
        }
    }

    normalized.trim_matches('-').to_string()
}
