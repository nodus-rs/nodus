use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use semver::{Version, VersionReq};

use super::discover::{
    canonicalize_existing_directory_path, canonicalize_existing_path, collect_files,
    default_package_name, normalize_manifest_relative_path, quote,
    validate_dependency_managed_specs, validate_managed_export_specs,
};
use super::*;
use crate::adapters::Adapter;
use crate::paths::{display_path, strip_path_prefix};

const CODEX_INSTALLATION_POLICIES: &[&str] =
    &["NOT_AVAILABLE", "AVAILABLE", "INSTALLED_BY_DEFAULT"];
const CODEX_AUTHENTICATION_POLICIES: &[&str] = &["ON_INSTALL", "ON_USE"];
const LEGACY_SYNC_ON_STARTUP_HOOK_ID: &str = "nodus.sync_on_startup";

impl LoadedManifest {
    pub fn validate(&self, role: PackageRole) -> Result<()> {
        if let Some(api_version) = &self.manifest.api_version
            && api_version.trim().is_empty()
        {
            bail!("manifest field `api_version` must not be empty");
        }
        if let Some(name) = &self.manifest.name
            && name.trim().is_empty()
        {
            bail!("manifest field `name` must not be empty");
        }
        let normalized_content_roots = self.manifest.normalized_content_roots()?;
        for content_root in &normalized_content_roots {
            self.resolve_existing_directory(content_root)
                .with_context(|| {
                    format!(
                        "manifest field `content_roots` contains invalid path `{}`",
                        display_path(content_root)
                    )
                })?;
        }
        if let Some(adapters) = &self.manifest.adapters {
            if adapters.enabled.is_empty() {
                bail!("manifest field `adapters.enabled` must not be empty");
            }

            let mut sorted = adapters.enabled.clone();
            sorted.sort();
            sorted.dedup();
            if sorted.len() != adapters.enabled.len() {
                bail!("manifest field `adapters.enabled` must not contain duplicates");
            }
        }
        if let Some(launch_hooks) = &self.manifest.launch_hooks
            && !launch_hooks.sync_on_startup
        {
            bail!("manifest field `launch_hooks.sync_on_startup` must be true when set");
        }
        validate_hooks(&self.manifest.hooks, role)?;
        if self.manifest.workspace.is_some() {
            validate_workspace(self)?;
            if !self.discovered.is_empty() {
                bail!(
                    "workspace roots must not declare root-level `agents/`, `commands/`, `rules/`, or `skills/`; move package content into workspace members"
                );
            }
            if !self.manifest.content_roots.is_empty() {
                bail!("workspace roots must not declare `content_roots`");
            }
            if self.manifest.publish_root {
                bail!("workspace roots must not declare `publish_root`");
            }
            if !self.manifest.managed_exports.is_empty() {
                bail!("workspace roots must not declare `managed_exports`");
            }
            if !self.manifest.mcp_servers.is_empty() {
                bail!("workspace roots must not declare `mcp_servers`");
            }
        }

        let allow_empty_package = match role {
            PackageRole::Root => true,
            PackageRole::Dependency => {
                if self.manifest.workspace.is_some() || self.allows_empty_dependency_wrapper {
                    true
                } else {
                    self.manifest_path.is_some()
                        && (!self.manifest.dependencies.is_empty()
                            || !self.manifest.mcp_servers.is_empty()
                            || !self.manifest.managed_exports.is_empty()
                            || !self.manifest.hooks.is_empty()
                            || !self.manifest.opencode_plugin_hooks.is_empty())
                }
            }
        };
        if self.discovered.is_empty()
            && self.manifest.mcp_servers.is_empty()
            && self.manifest.hooks.is_empty()
            && self.manifest.opencode_plugin_hooks.is_empty()
            && !allow_empty_package
        {
            bail!(
                "package at {} must contain at least one of `agents/`, `commands/`, `rules/`, or `skills/`, declare `hooks`, declare `opencode_plugin_hooks`, declare `managed_exports`, declare `mcp_servers`, or declare dependencies in nodus.toml",
                self.root.display()
            );
        }

        validate_managed_export_specs(&self.manifest.managed_exports)?;

        for (server_id, server) in &self.manifest.mcp_servers {
            validate_mcp_server(server_id, server)?;
        }

        let mut aliases = HashSet::new();
        for entry in self.manifest.all_dependency_entries() {
            if !aliases.insert(entry.alias) {
                bail!(
                    "manifest must not declare `{}` `{}` in more than one dependency section",
                    entry.kind.label(),
                    entry.alias
                );
            }
            validate_dependency_entry(self, entry)?;
        }

        Ok(())
    }

    pub fn package_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = self.discovered.files(self)?;
        if let Some(manifest_path) = &self.manifest_path {
            files.push(manifest_path.clone());
        }
        files.extend(self.extra_package_files.iter().cloned());
        files.sort();
        files.dedup();
        Ok(files)
    }

    pub fn with_manifest(&self, manifest: Manifest, role: PackageRole) -> Result<Self> {
        let mut loaded = self.clone();
        loaded.manifest = manifest;
        loaded.manifest_path = Some(loaded.root.join(MANIFEST_FILE));
        loaded.manifest_contents_override =
            Some(serialize_manifest(&loaded.manifest)?.into_bytes());
        loaded.validate(role)?;
        Ok(loaded)
    }

    pub fn read_package_file(&self, path: &Path) -> Result<Vec<u8>> {
        if self.manifest_path.as_deref() == Some(path)
            && let Some(contents) = &self.manifest_contents_override
        {
            return Ok(contents.clone());
        }

        let resolved = self.resolve_package_file_path(path)?;
        fs::read(&resolved).with_context(|| format!("failed to read {}", path.display()))
    }

    pub fn resolve_path(&self, value: &Path) -> Result<PathBuf> {
        self.resolve_existing_path(value)
    }

    pub fn effective_name(&self) -> String {
        self.manifest
            .name
            .clone()
            .unwrap_or_else(|| default_package_name(&self.root))
    }

    pub fn effective_version(&self) -> Option<Version> {
        self.manifest.version.clone()
    }

    pub fn claude_plugin_hook_compat_sources(&self) -> &[ClaudePluginHookCompatSource] {
        self.claude_plugin
            .as_ref()
            .map(|plugin| plugin.hook_compat_sources.as_slice())
            .unwrap_or(&[])
    }

    fn resolve_package_file_path(&self, path: &Path) -> Result<PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        if absolute.is_file() {
            return Ok(absolute);
        }
        if !absolute.starts_with(&self.root) {
            return Ok(absolute);
        }

        for ancestor in absolute.ancestors().skip(1) {
            if !ancestor.starts_with(&self.root) {
                continue;
            }
            let Some(relative_ancestor) = strip_path_prefix(ancestor, &self.root) else {
                continue;
            };
            let Some(relative_suffix) = strip_path_prefix(&absolute, ancestor) else {
                continue;
            };
            let Ok(resolved_dir) = self.resolve_existing_directory(relative_ancestor) else {
                continue;
            };
            let candidate = resolved_dir.join(relative_suffix);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }

        Ok(absolute)
    }

    pub fn workspace_member_statuses(&self) -> Result<Vec<WorkspaceMemberStatus>> {
        let Some(workspace) = &self.manifest.workspace else {
            return Ok(Vec::new());
        };

        let mut members_by_key = std::collections::BTreeMap::new();
        for (id, member) in &workspace.package {
            members_by_key.insert(
                workspace_member_path_key(&member.path),
                (id.as_str(), member),
            );
        }

        let mut ordered = Vec::with_capacity(workspace.members.len());
        for member_path in &workspace.members {
            let key = workspace_member_path_key(member_path);
            let Some((id, member)) = members_by_key.remove(&key) else {
                bail!(
                    "manifest field `workspace.members` path `{}` must match a `[workspace.package.<id>]` entry",
                    display_path(member_path)
                );
            };
            ordered.push(self.workspace_member_status(id, member)?);
        }

        Ok(ordered)
    }

    pub fn resolved_workspace_members(&self) -> Result<Vec<ResolvedWorkspaceMember>> {
        Ok(self
            .workspace_member_statuses()?
            .into_iter()
            .filter(|member| member.enabled)
            .map(|member| ResolvedWorkspaceMember {
                id: member.id,
                path: member.path,
                name: member.name,
                codex: member.codex,
            })
            .collect())
    }

    pub(super) fn resolve_existing_path(&self, value: &Path) -> Result<PathBuf> {
        if value.is_absolute() {
            bail!(
                "manifest path `{}` must be relative to {}",
                value.display(),
                self.root.display()
            );
        }

        let joined = self.root.join(value);
        let canonical = canonicalize_existing_path(&joined)
            .with_context(|| format!("missing path `{}`", value.display()))?;
        if !canonical.starts_with(&self.root) {
            bail!(
                "path `{}` escapes the package root {}",
                value.display(),
                self.root.display()
            );
        }

        Ok(canonical)
    }

    pub(super) fn resolve_existing_directory(&self, value: &Path) -> Result<PathBuf> {
        if value.is_absolute() {
            bail!(
                "manifest path `{}` must be relative to {}",
                value.display(),
                self.root.display()
            );
        }

        let joined = self.root.join(value);
        let canonical = canonicalize_existing_directory_path(&joined)
            .with_context(|| format!("failed to resolve directory `{}`", display_path(value)))?;
        if !canonical.starts_with(&self.root) {
            bail!(
                "path `{}` escapes the package root {}",
                display_path(value),
                self.root.display()
            );
        }

        Ok(canonical)
    }

    fn workspace_member_status(
        &self,
        id: &str,
        member: &WorkspaceMemberSpec,
    ) -> Result<WorkspaceMemberStatus> {
        validate_workspace_member_codex_metadata(id, member)?;
        let warning = match validate_workspace_member(self, id, member) {
            Ok(()) => None,
            Err(error) => Some(format!("ignoring workspace member `{id}`: {error}")),
        };
        Ok(WorkspaceMemberStatus {
            id: id.to_string(),
            path: member.path.clone(),
            name: member.name.clone(),
            codex: member.codex.clone(),
            enabled: warning.is_none(),
            warning,
        })
    }
}

impl Manifest {
    pub fn dependency_section(
        &self,
        kind: DependencyKind,
    ) -> &std::collections::BTreeMap<String, DependencySpec> {
        match kind {
            DependencyKind::Dependency => &self.dependencies,
            DependencyKind::DevDependency => &self.dev_dependencies,
        }
    }

    pub fn dependency_section_mut(
        &mut self,
        kind: DependencyKind,
    ) -> &mut std::collections::BTreeMap<String, DependencySpec> {
        match kind {
            DependencyKind::Dependency => &mut self.dependencies,
            DependencyKind::DevDependency => &mut self.dev_dependencies,
        }
    }

    pub fn contains_dependency_alias(&self, alias: &str) -> bool {
        self.dependencies.contains_key(alias) || self.dev_dependencies.contains_key(alias)
    }

    pub fn dependency_kind(&self, alias: &str) -> Option<DependencyKind> {
        if self.dependencies.contains_key(alias) {
            Some(DependencyKind::Dependency)
        } else if self.dev_dependencies.contains_key(alias) {
            Some(DependencyKind::DevDependency)
        } else {
            None
        }
    }

    pub fn get_dependency(&self, alias: &str) -> Option<DependencyEntry<'_>> {
        self.dependencies
            .get_key_value(alias)
            .map(|(alias, spec)| DependencyEntry {
                alias,
                spec,
                kind: DependencyKind::Dependency,
            })
            .or_else(|| {
                self.dev_dependencies
                    .get_key_value(alias)
                    .map(|(alias, spec)| DependencyEntry {
                        alias,
                        spec,
                        kind: DependencyKind::DevDependency,
                    })
            })
    }

    pub fn all_dependency_entries(&self) -> Vec<DependencyEntry<'_>> {
        self.dependencies
            .iter()
            .map(|(alias, spec)| DependencyEntry {
                alias,
                spec,
                kind: DependencyKind::Dependency,
            })
            .chain(
                self.dev_dependencies
                    .iter()
                    .map(|(alias, spec)| DependencyEntry {
                        alias,
                        spec,
                        kind: DependencyKind::DevDependency,
                    }),
            )
            .collect()
    }

    pub fn active_dependency_entries(&self) -> Vec<DependencyEntry<'_>> {
        self.all_dependency_entries()
            .into_iter()
            .filter(|entry| entry.spec.is_enabled())
            .collect()
    }

    pub fn dependency_entries_for_role(&self, role: PackageRole) -> Vec<DependencyEntry<'_>> {
        match role {
            PackageRole::Root => self.all_dependency_entries(),
            PackageRole::Dependency => self
                .dependencies
                .iter()
                .map(|(alias, spec)| DependencyEntry {
                    alias,
                    spec,
                    kind: DependencyKind::Dependency,
                })
                .collect(),
        }
    }

    pub fn active_dependency_entries_for_role(
        &self,
        role: PackageRole,
    ) -> Vec<DependencyEntry<'_>> {
        self.dependency_entries_for_role(role)
            .into_iter()
            .filter(|entry| entry.spec.is_enabled())
            .collect()
    }

    pub fn enabled_adapters(&self) -> Option<&[Adapter]> {
        self.adapters
            .as_ref()
            .map(|config| config.enabled.as_slice())
    }

    pub fn normalized_content_roots(&self) -> Result<Vec<PathBuf>> {
        let mut normalized_roots = Vec::with_capacity(self.content_roots.len());
        let mut seen = HashSet::new();
        for root in &self.content_roots {
            let normalized =
                normalize_manifest_relative_path(root, "manifest field `content_roots` entry")?;
            if !seen.insert(normalized.clone()) {
                bail!("manifest field `content_roots` must not contain duplicate paths");
            }
            normalized_roots.push(normalized);
        }
        Ok(normalized_roots)
    }

    pub fn normalized_claude_plugin_hooks(&self) -> Result<Vec<PathBuf>> {
        let mut normalized_paths = Vec::with_capacity(self.claude_plugin_hooks.len());
        let mut seen = HashSet::new();
        for path in &self.claude_plugin_hooks {
            let normalized = normalize_manifest_relative_path(
                path,
                "manifest field `claude_plugin_hooks` entry",
            )?;
            if !seen.insert(normalized.clone()) {
                bail!("manifest field `claude_plugin_hooks` must not contain duplicate paths");
            }
            normalized_paths.push(normalized);
        }
        Ok(normalized_paths)
    }

    pub fn normalized_opencode_plugin_hooks(&self) -> Result<Vec<PathBuf>> {
        let mut normalized_paths = Vec::with_capacity(self.opencode_plugin_hooks.len());
        let mut seen = HashSet::new();
        for path in &self.opencode_plugin_hooks {
            let normalized = normalize_manifest_relative_path(
                path,
                "manifest field `opencode_plugin_hooks` entry",
            )?;
            if !seen.insert(normalized.clone()) {
                bail!("manifest field `opencode_plugin_hooks` must not contain duplicate paths");
            }
            normalized_paths.push(normalized);
        }
        Ok(normalized_paths)
    }

    pub fn set_enabled_adapters(&mut self, adapters: &[Adapter]) {
        self.adapters = Some(AdapterConfig::normalized(adapters));
    }

    pub fn effective_hooks(&self) -> Vec<HookSpec> {
        let mut hooks = self.hooks.clone();
        if self.sync_on_launch_enabled() && !hooks.iter().any(Self::is_sync_on_launch_hook) {
            hooks.push(Self::legacy_sync_on_startup_hook());
        }
        hooks
    }

    pub fn legacy_sync_on_startup_hook() -> HookSpec {
        HookSpec {
            id: LEGACY_SYNC_ON_STARTUP_HOOK_ID.to_string(),
            event: HookEvent::SessionStart,
            adapters: Vec::new(),
            matcher: Some(HookMatcher {
                sources: vec![HookSessionSource::Startup, HookSessionSource::Resume],
                tool_names: Vec::new(),
            }),
            handler: HookHandler {
                handler_type: HookHandlerType::Command,
                command: "nodus sync".to_string(),
                cwd: HookWorkingDirectory::GitRoot,
            },
            timeout_sec: None,
            blocking: false,
        }
    }

    pub fn sync_on_launch_enabled(&self) -> bool {
        self.hooks.iter().any(Self::is_sync_on_launch_hook)
            || self
                .launch_hooks
                .as_ref()
                .is_some_and(|hooks| hooks.sync_on_startup)
    }

    pub fn uses_legacy_launch_hook_config(&self) -> bool {
        self.launch_hooks.is_some()
    }

    pub fn set_sync_on_launch(&mut self, enabled: bool) {
        self.launch_hooks = None;
        self.hooks
            .retain(|hook| !Self::is_sync_on_launch_hook(hook));
        if enabled {
            self.hooks.push(Self::legacy_sync_on_startup_hook());
        }
    }

    pub(crate) fn is_sync_on_launch_hook(hook: &HookSpec) -> bool {
        hook.id == LEGACY_SYNC_ON_STARTUP_HOOK_ID
    }

    pub fn remove_managed_mapping(&mut self, alias: &str, target_root: &Path) -> Result<bool> {
        let Some(kind) = self.dependency_kind(alias) else {
            return Ok(false);
        };
        let Some(dependency) = self.dependency_section_mut(kind).get_mut(alias) else {
            return Ok(false);
        };
        let Some(managed) = dependency.managed.as_mut() else {
            return Ok(false);
        };

        let before = managed.len();
        managed.retain(|mapping| {
            mapping
                .normalized_target()
                .map(|target| target != target_root)
                .unwrap_or(true)
        });
        let removed = managed.len() != before;
        if managed.is_empty() {
            dependency.managed = None;
        }

        Ok(removed)
    }
}

fn validate_hooks(hooks: &[HookSpec], _role: PackageRole) -> Result<()> {
    if hooks.is_empty() {
        return Ok(());
    }

    let mut ids = HashSet::new();
    for hook in hooks {
        if hook.id.trim().is_empty() {
            bail!("manifest field `hooks.id` must not be empty");
        }
        if !ids.insert(hook.id.as_str()) {
            bail!("manifest field `hooks` must not contain duplicate ids");
        }
        if hook.handler.command.trim().is_empty() {
            bail!(
                "manifest hook `{}` field `handler.command` must not be empty",
                hook.id
            );
        }

        let mut adapters = hook.adapters.clone();
        adapters.sort();
        adapters.dedup();
        if adapters.len() != hook.adapters.len() {
            bail!(
                "manifest hook `{}` field `adapters` must not contain duplicates",
                hook.id
            );
        }

        if let Some(matcher) = &hook.matcher {
            let mut sources = matcher.sources.clone();
            sources.sort_by_key(|source| source.as_str());
            sources.dedup_by_key(|source| source.as_str());
            if sources.len() != matcher.sources.len() {
                bail!(
                    "manifest hook `{}` field `matcher.sources` must not contain duplicates",
                    hook.id
                );
            }

            let mut tool_names = matcher.tool_names.clone();
            tool_names.sort_by_key(|tool_name| tool_name.as_str());
            tool_names.dedup_by_key(|tool_name| tool_name.as_str());
            if tool_names.len() != matcher.tool_names.len() {
                bail!(
                    "manifest hook `{}` field `matcher.tool_names` must not contain duplicates",
                    hook.id
                );
            }

            match hook.event {
                HookEvent::SessionStart => {
                    if !matcher.tool_names.is_empty() {
                        bail!(
                            "manifest hook `{}` field `matcher.tool_names` is not supported for `session_start`",
                            hook.id
                        );
                    }
                }
                HookEvent::PreToolUse | HookEvent::PermissionRequest | HookEvent::PostToolUse => {
                    if !matcher.sources.is_empty() {
                        bail!(
                            "manifest hook `{}` field `matcher.sources` is not supported for tool hook events",
                            hook.id
                        );
                    }
                }
                HookEvent::UserPromptSubmit | HookEvent::SubagentStop | HookEvent::SessionEnd => {
                    if !matcher.sources.is_empty() || !matcher.tool_names.is_empty() {
                        bail!(
                            "manifest hook `{}` field `matcher` is not supported for `{}`",
                            hook.id,
                            hook.event.as_str()
                        );
                    }
                }
                HookEvent::Stop => {
                    if !matcher.sources.is_empty() || !matcher.tool_names.is_empty() {
                        bail!(
                            "manifest hook `{}` must not declare matcher fields for `stop`",
                            hook.id
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

fn validate_dependency_entry(package: &LoadedManifest, entry: DependencyEntry<'_>) -> Result<()> {
    let alias = entry.alias;
    let dependency = entry.spec;
    let label = entry.kind.label();

    if alias.trim().is_empty() {
        bail!("{label} names must not be empty");
    }
    match dependency.source_kind()? {
        DependencySourceKind::Git => {
            let url = dependency.resolved_git_url()?;
            if url.trim().is_empty() {
                bail!("{label} `{alias}` has an empty git source");
            }
            if let Some(subpath) = &dependency.subpath {
                normalize_manifest_relative_path(
                    subpath,
                    &format!("{label} `{alias}` field `subpath`"),
                )?;
            }
            let tag = dependency.tag.as_deref().map(str::trim).unwrap_or_default();
            let branch = dependency
                .branch
                .as_deref()
                .map(str::trim)
                .unwrap_or_default();
            let revision = dependency
                .revision
                .as_deref()
                .map(str::trim)
                .unwrap_or_default();
            let requested_ref_count = usize::from(!tag.is_empty())
                + usize::from(!branch.is_empty())
                + usize::from(!revision.is_empty());
            match requested_ref_count {
                0 => {
                    if dependency.version.is_none() && !package.allows_unpinned_git_dependencies {
                        bail!(
                            "{label} `{alias}` must declare `tag`, `branch`, `revision`, or `version` for git sources"
                        )
                    }
                }
                1 => {}
                _ => {
                    bail!(
                        "{label} `{alias}` must not declare more than one of `tag`, `branch`, or `revision`"
                    )
                }
            }
            if dependency.version.is_some() && !tag.is_empty() {
                bail!("{label} `{alias}` must not declare both `version` and `tag`");
            }
            if dependency.version.is_some() && !branch.is_empty() {
                bail!("{label} `{alias}` must not declare both `version` and `branch`");
            }
            if dependency.version.is_some() && !revision.is_empty() {
                bail!("{label} `{alias}` must not declare both `version` and `revision`");
            }
        }
        DependencySourceKind::Path => {
            let Some(path) = &dependency.path else {
                bail!("{label} `{alias}` must declare `path`");
            };
            if dependency.version.is_some() {
                bail!("{label} `{alias}` must not declare `version` for path sources");
            }
            if dependency.subpath.is_some() {
                bail!("{label} `{alias}` must not declare `subpath` for path sources");
            }
            let _dependency_root = package
                .resolve_existing_directory(path)
                .with_context(|| format!("{label} `{alias}` path must point to a directory"))?;
        }
    }

    if let Some(components) = &dependency.components {
        if components.is_empty() {
            bail!("{label} `{alias}` field `components` must not be empty");
        }

        let mut sorted = components.clone();
        sorted.sort();
        sorted.dedup();
        if sorted.len() != components.len() {
            bail!("{label} `{alias}` field `components` must not contain duplicates");
        }
    }

    if let Some(members) = &dependency.members {
        let mut seen = HashSet::new();
        for member in members {
            if member.trim().is_empty() {
                bail!("{label} `{alias}` field `members` must not contain empty names");
            }
            if !seen.insert(member) {
                bail!("{label} `{alias}` field `members` must not contain duplicates");
            }
        }
    }

    validate_dependency_managed_specs(alias, dependency.managed.as_deref())?;
    Ok(())
}

impl DependencySpec {
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn inline_fields(&self) -> Vec<String> {
        self.key_value_fields()
    }

    pub fn key_value_fields(&self) -> Vec<String> {
        let mut fields = Vec::new();
        if let Some(github) = &self.github {
            fields.push(format!("github = {}", quote(github)));
        }
        if let Some(url) = &self.url {
            fields.push(format!("url = {}", quote(url)));
        }
        if let Some(path) = &self.path {
            fields.push(format!("path = {}", quote(&display_path(path))));
        }
        if let Some(subpath) = &self.subpath {
            fields.push(format!("subpath = {}", quote(&display_path(subpath))));
        }
        if let Some(tag) = &self.tag {
            fields.push(format!("tag = {}", quote(tag)));
        }
        if let Some(branch) = &self.branch {
            fields.push(format!("branch = {}", quote(branch)));
        }
        if let Some(revision) = &self.revision {
            fields.push(format!("revision = {}", quote(revision)));
        }
        if let Some(version) = &self.version {
            fields.push(format!("version = {}", quote(&version.to_string())));
        }
        if let Some(components) = self.explicit_components_sorted() {
            let encoded = components
                .into_iter()
                .map(|component| quote(component.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            fields.push(format!("components = [{encoded}]"));
        }
        if let Some(members) = self.explicit_members_sorted() {
            let encoded = members
                .into_iter()
                .map(|member| quote(&member))
                .collect::<Vec<_>>()
                .join(", ");
            fields.push(format!("members = [{encoded}]"));
        }
        if !self.enabled {
            fields.push("enabled = false".to_string());
        }
        fields
    }

    pub fn explicit_components_sorted(&self) -> Option<Vec<DependencyComponent>> {
        let mut components = self.components.clone()?;
        components.sort();
        Some(components)
    }

    pub fn explicit_members_sorted(&self) -> Option<Vec<String>> {
        let mut members = self.members.clone()?;
        members.sort();
        Some(members)
    }

    pub fn normalized_components(&self) -> Vec<DependencyComponent> {
        self.explicit_components_sorted()
            .unwrap_or_else(|| DependencyComponent::ALL.to_vec())
    }

    pub fn effective_selected_components(&self) -> Option<Vec<DependencyComponent>> {
        let components = self.normalized_components();
        (components.len() != DependencyComponent::ALL.len()).then_some(components)
    }

    pub fn source_kind(&self) -> Result<DependencySourceKind> {
        let git_sources = usize::from(self.github.is_some()) + usize::from(self.url.is_some());
        match (git_sources, self.path.is_some()) {
            (1, false) => Ok(DependencySourceKind::Git),
            (0, true) => Ok(DependencySourceKind::Path),
            (0, false) => {
                bail!("dependency source must declare either `github`, `url`, or `path`")
            }
            (_, true) => {
                bail!(
                    "dependency source must not declare both a git source (`github` or `url`) and `path`"
                )
            }
            _ => bail!("dependency source must not declare both `github` and `url`"),
        }
    }

    pub fn resolved_git_url(&self) -> Result<String> {
        if let Some(url) = &self.url {
            let trimmed = url.trim();
            if trimmed.is_empty() {
                bail!("git dependency `url` must not be empty");
            }
            return Ok(trimmed.to_string());
        }

        if let Some(github) = &self.github {
            let trimmed = github.trim().trim_matches('/');
            let Some((owner, repo)) = trimmed.split_once('/') else {
                bail!("git dependency `github` must use the format `owner/repo`");
            };
            if owner.is_empty() || repo.is_empty() || repo.contains('/') {
                bail!("git dependency `github` must use the format `owner/repo`");
            }
            return Ok(format!("https://github.com/{owner}/{repo}"));
        }

        bail!("dependency source must declare either `github` or `url` for git dependencies")
    }

    pub fn requested_git_ref(&self) -> Result<RequestedGitRef<'_>> {
        self.requested_git_ref_or_none()?.ok_or_else(|| {
            anyhow::anyhow!("git dependency must declare `tag`, `branch`, `revision`, or `version`")
        })
    }

    pub fn requested_git_ref_or_none(&self) -> Result<Option<RequestedGitRef<'_>>> {
        match (
            self.tag
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
            self.branch
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
            self.revision
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        ) {
            (Some(tag), None, None) => Ok(Some(RequestedGitRef::Tag(tag))),
            (None, Some(branch), None) => Ok(Some(RequestedGitRef::Branch(branch))),
            (None, None, Some(revision)) => Ok(Some(RequestedGitRef::Revision(revision))),
            (None, None, None) => Ok(self.version.as_ref().map(RequestedGitRef::VersionReq)),
            _ => bail!(
                "git dependency must not declare more than one of `tag`, `branch`, or `revision`"
            ),
        }
    }

    pub fn managed_mappings(&self) -> &[ManagedPathSpec] {
        self.managed.as_deref().unwrap_or(&[])
    }
}

fn validate_workspace(package: &LoadedManifest) -> Result<()> {
    let Some(workspace) = &package.manifest.workspace else {
        return Ok(());
    };
    if workspace.members.is_empty() {
        bail!("manifest field `workspace.members` must not be empty");
    }
    if workspace.package.is_empty() {
        bail!("manifest field `workspace.package` must not be empty");
    }

    let mut seen_paths = HashSet::new();
    for member_path in &workspace.members {
        let path_key = workspace_member_path_key(member_path);
        if !seen_paths.insert(path_key) {
            bail!("manifest field `workspace.members` must not contain duplicate paths");
        }
    }

    let mut package_paths = HashSet::new();
    for (id, member) in &workspace.package {
        let normalized_id = normalize_dependency_alias(id)?;
        if normalized_id != *id {
            bail!("workspace package id `{id}` must already be normalized as `{normalized_id}`");
        }
        let path_key = workspace_member_path_key(&member.path);
        if !package_paths.insert(path_key.clone()) {
            bail!("manifest field `workspace.package` must not contain duplicate paths");
        }
        if !seen_paths.contains(&path_key) {
            bail!(
                "manifest field `workspace.package.{id}.path` must also appear in `workspace.members`"
            );
        }
    }

    if seen_paths.len() != package_paths.len() {
        bail!(
            "manifest field `workspace.members` and `workspace.package` must describe the same member set"
        );
    }

    Ok(())
}

fn validate_workspace_member(
    package: &LoadedManifest,
    id: &str,
    member: &WorkspaceMemberSpec,
) -> Result<()> {
    if let Some(name) = &member.name
        && name.trim().is_empty()
    {
        bail!("manifest field `workspace.package.{id}.name` must not be empty");
    }

    let normalized_path = normalize_manifest_relative_path(
        &member.path,
        &format!("manifest field `workspace.package.{id}.path`"),
    )?;
    let resolved = package
        .resolve_existing_directory(&normalized_path)
        .with_context(|| {
            format!("manifest field `workspace.package.{id}.path` must point to a directory")
        })?;

    load_dependency_from_dir(&resolved).with_context(|| {
        format!(
            "workspace member `{id}` at `{}` is invalid",
            display_path(&member.path)
        )
    })?;

    Ok(())
}

fn validate_workspace_member_codex_metadata(id: &str, member: &WorkspaceMemberSpec) -> Result<()> {
    if let Some(codex) = &member.codex {
        if codex.category.trim().is_empty() {
            bail!("manifest field `workspace.package.{id}.codex.category` must not be empty");
        }
        validate_codex_workspace_policy(
            &format!("manifest field `workspace.package.{id}.codex.installation`"),
            &codex.installation,
            CODEX_INSTALLATION_POLICIES,
        )?;
        validate_codex_workspace_policy(
            &format!("manifest field `workspace.package.{id}.codex.authentication`"),
            &codex.authentication,
            CODEX_AUTHENTICATION_POLICIES,
        )?;
    }

    Ok(())
}

fn validate_codex_workspace_policy(field: &str, value: &str, allowed: &[&str]) -> Result<()> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{field} must not be empty");
    }
    if !allowed.contains(&value) {
        bail!("{field} must be one of: {}", allowed.join(", "));
    }
    Ok(())
}

fn workspace_member_path_key(path: &Path) -> String {
    normalize_manifest_relative_path(path, "workspace member path")
        .map(|normalized| display_path(&normalized))
        .unwrap_or_else(|_| display_path(path))
}

impl ManagedPathSpec {
    pub fn normalized_source(&self) -> Result<PathBuf> {
        normalize_manifest_relative_path(&self.source, "managed source path")
    }

    pub fn normalized_target(&self) -> Result<PathBuf> {
        normalize_manifest_relative_path(&self.target, "managed target path")
    }
}

impl ManagedExportSpec {
    pub fn normalized_source(&self) -> Result<PathBuf> {
        normalize_manifest_relative_path(&self.source, "managed export source path")
    }

    pub fn normalized_target(&self) -> Result<PathBuf> {
        normalize_manifest_relative_path(&self.target, "managed export target path")
    }
}

fn validate_mcp_server(server_id: &str, server: &McpServerConfig) -> Result<()> {
    if server_id.trim().is_empty() {
        bail!("manifest field `mcp_servers` contains an empty server id");
    }
    if server
        .transport_type
        .as_deref()
        .is_some_and(|transport_type| transport_type.trim().is_empty())
    {
        bail!("manifest field `mcp_servers.{server_id}.type` must not be empty");
    }
    if server
        .command
        .as_deref()
        .is_some_and(|command| command.trim().is_empty())
    {
        bail!("manifest field `mcp_servers.{server_id}.command` must not be empty");
    }
    if server
        .url
        .as_deref()
        .is_some_and(|url| url.trim().is_empty())
    {
        bail!("manifest field `mcp_servers.{server_id}.url` must not be empty");
    }
    match (
        server
            .command
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
        server
            .url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
    ) {
        (Some(_), None) => {}
        (None, Some(_)) => {}
        (None, None) => {
            bail!("manifest field `mcp_servers.{server_id}` must declare either `command` or `url`")
        }
        (Some(_), Some(_)) => {
            bail!(
                "manifest field `mcp_servers.{server_id}` must not declare both `command` and `url`"
            )
        }
    }
    if server.url.is_some()
        && (!server.args.is_empty() || !server.env.is_empty() || server.cwd.is_some())
    {
        bail!(
            "manifest field `mcp_servers.{server_id}` must not combine `url` with `args`, `env`, or `cwd`"
        );
    }
    if !server.headers.is_empty() && server.url.is_none() {
        bail!("manifest field `mcp_servers.{server_id}.headers` requires `url` to be set");
    }
    if let Some(cwd) = &server.cwd
        && cwd.as_os_str().is_empty()
    {
        bail!("manifest field `mcp_servers.{server_id}.cwd` must not be empty");
    }
    for key in server.env.keys() {
        if key.trim().is_empty() {
            bail!("manifest field `mcp_servers.{server_id}.env` must not contain empty keys");
        }
    }
    for key in server.headers.keys() {
        if key.trim().is_empty() {
            bail!("manifest field `mcp_servers.{server_id}.headers` must not contain empty keys");
        }
    }

    if server
        .command
        .as_deref()
        .is_some_and(|command| command.contains("${CLAUDE_PLUGIN_ROOT}"))
        || server
            .url
            .as_deref()
            .is_some_and(|url| url.contains("${CLAUDE_PLUGIN_ROOT}"))
        || server
            .args
            .iter()
            .any(|arg| arg.contains("${CLAUDE_PLUGIN_ROOT}"))
        || server
            .env
            .values()
            .any(|value| value.contains("${CLAUDE_PLUGIN_ROOT}"))
        || server
            .headers
            .values()
            .any(|value| value.contains("${CLAUDE_PLUGIN_ROOT}"))
        || server
            .cwd
            .as_ref()
            .is_some_and(|cwd| display_path(cwd).contains("${CLAUDE_PLUGIN_ROOT}"))
    {
        bail!(
            "manifest field `mcp_servers.{server_id}` uses unsupported `${{CLAUDE_PLUGIN_ROOT}}` interpolation"
        );
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedGitRef<'a> {
    Tag(&'a str),
    Branch(&'a str),
    Revision(&'a str),
    VersionReq(&'a VersionReq),
}

impl PackageContents {
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
            && self.agents.is_empty()
            && self.rules.is_empty()
            && self.commands.is_empty()
    }

    pub fn selected_agents(&self, adapter: Adapter) -> Vec<&AgentEntry> {
        let mut selected = BTreeMap::<&str, (u8, &AgentEntry)>::new();

        for agent in &self.agents {
            let Some(priority) = agent.adapter_priority(adapter) else {
                continue;
            };
            match selected.get(agent.id.as_str()) {
                Some((existing_priority, existing))
                    if *existing_priority > priority
                        || (*existing_priority == priority && existing.path <= agent.path) => {}
                _ => {
                    selected.insert(agent.id.as_str(), (priority, agent));
                }
            }
        }

        selected
            .into_values()
            .map(|(_, agent)| agent)
            .collect::<Vec<_>>()
    }

    pub fn unique_agent_ids(&self) -> Vec<&String> {
        let mut seen = HashSet::new();
        let mut ids = Vec::new();
        for agent in &self.agents {
            if seen.insert(agent.id.as_str()) {
                ids.push(&agent.id);
            }
        }
        ids.sort();
        ids
    }

    pub fn files(&self, package: &LoadedManifest) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for skill in &self.skills {
            let logical_root = package.root.join(&skill.path);
            let resolved_root = package.resolve_existing_directory(&skill.path)?;
            for file in collect_files(&resolved_root)? {
                let relative = strip_path_prefix(&file, &resolved_root).with_context(|| {
                    format!(
                        "failed to make {} relative to {}",
                        file.display(),
                        resolved_root.display()
                    )
                })?;
                files.push(logical_root.join(relative));
            }
        }
        for agent in &self.agents {
            files.push(package.root.join(&agent.path));
        }
        for rule in &self.rules {
            files.push(package.root.join(&rule.path));
        }
        for command in &self.commands {
            files.push(package.root.join(&command.path));
        }
        files.sort();
        files.dedup();
        Ok(files)
    }
}

impl AgentEntry {
    pub fn adapter_priority(&self, adapter: Adapter) -> Option<u8> {
        match adapter {
            Adapter::Codex => {
                if self.is_codex_specific_toml() {
                    Some(4)
                } else if self.is_plain_toml() {
                    Some(3)
                } else if self.is_plain_markdown() {
                    Some(2)
                } else {
                    None
                }
            }
            Adapter::Claude | Adapter::Copilot | Adapter::OpenCode => {
                if self.is_plain_markdown() {
                    Some(4)
                } else if self.is_plain_toml() {
                    Some(3)
                } else if self.is_codex_specific_toml() {
                    Some(2)
                } else {
                    None
                }
            }
            Adapter::Agents | Adapter::Cursor => None,
        }
    }

    pub fn is_markdown(&self) -> bool {
        self.format.eq_ignore_ascii_case("md")
    }

    pub fn is_toml(&self) -> bool {
        self.format.eq_ignore_ascii_case("toml")
    }

    pub fn is_plain_markdown(&self) -> bool {
        self.is_markdown() && self.qualifiers.is_empty()
    }

    pub fn is_plain_toml(&self) -> bool {
        self.is_toml() && self.qualifiers.is_empty()
    }

    pub fn is_codex_specific_toml(&self) -> bool {
        self.is_toml()
            && self.qualifiers.len() == 1
            && self.qualifiers[0].eq_ignore_ascii_case("codex")
    }
}
