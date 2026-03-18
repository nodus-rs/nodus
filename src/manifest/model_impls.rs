use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use semver::Version;

use super::discover::{
    canonicalize_existing_path, collect_files, default_package_name,
    normalize_manifest_relative_path, quote, validate_dependency_managed_specs,
};
use super::*;
use crate::adapters::Adapter;
use crate::paths::display_path;

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

        let allow_empty_package = match role {
            PackageRole::Root => true,
            PackageRole::Dependency => {
                (self.manifest_path.is_some() || self.allows_empty_dependency_wrapper)
                    && !self.manifest.dependencies.is_empty()
            }
        };
        if self.discovered.is_empty() && !allow_empty_package {
            bail!(
                "package at {} must contain at least one of `agents/`, `commands/`, `rules/`, or `skills/`, or declare dependencies in nodus.toml",
                self.root.display()
            );
        }

        for (alias, dependency) in &self.manifest.dependencies {
            if alias.trim().is_empty() {
                bail!("dependency names must not be empty");
            }
            match dependency.source_kind()? {
                DependencySourceKind::Git => {
                    let url = dependency.resolved_git_url()?;
                    if url.trim().is_empty() {
                        bail!("dependency `{alias}` has an empty git source");
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
                            bail!(
                                "dependency `{alias}` must declare `tag`, `branch`, or `revision` for git sources"
                            )
                        }
                        1 => {}
                        _ => {
                            bail!(
                                "dependency `{alias}` must not declare more than one of `tag`, `branch`, or `revision`"
                            )
                        }
                    }
                }
                DependencySourceKind::Path => {
                    let Some(path) = &dependency.path else {
                        bail!("dependency `{alias}` must declare `path`");
                    };
                    let dependency_root = self.resolve_existing_path(path)?;
                    if !dependency_root.is_dir() {
                        bail!(
                            "dependency `{alias}` path must point to a directory, found {}",
                            dependency_root.display()
                        );
                    }
                }
            }

            if let Some(components) = &dependency.components {
                if components.is_empty() {
                    bail!("dependency `{alias}` field `components` must not be empty");
                }

                let mut sorted = components.clone();
                sorted.sort();
                sorted.dedup();
                if sorted.len() != components.len() {
                    bail!("dependency `{alias}` field `components` must not contain duplicates");
                }
            }

            validate_dependency_managed_specs(alias, dependency.managed.as_deref())?;
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

        fs::read(path).with_context(|| format!("failed to read {}", path.display()))
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
}

impl Manifest {
    pub fn enabled_adapters(&self) -> Option<&[Adapter]> {
        self.adapters
            .as_ref()
            .map(|config| config.enabled.as_slice())
    }

    pub fn set_enabled_adapters(&mut self, adapters: &[Adapter]) {
        self.adapters = Some(AdapterConfig::normalized(adapters));
    }

    pub fn sync_on_launch_enabled(&self) -> bool {
        self.launch_hooks
            .as_ref()
            .is_some_and(|hooks| hooks.sync_on_startup)
    }

    pub fn set_sync_on_launch(&mut self, enabled: bool) {
        self.launch_hooks = enabled.then_some(LaunchHookConfig {
            sync_on_startup: true,
        });
    }

    pub fn remove_managed_mapping(&mut self, alias: &str, target_root: &Path) -> Result<bool> {
        let Some(dependency) = self.dependencies.get_mut(alias) else {
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

impl DependencySpec {
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
        fields
    }

    pub fn explicit_components_sorted(&self) -> Option<Vec<DependencyComponent>> {
        let mut components = self.components.clone()?;
        components.sort();
        Some(components)
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
            (Some(tag), None, None) => Ok(RequestedGitRef::Tag(tag)),
            (None, Some(branch), None) => Ok(RequestedGitRef::Branch(branch)),
            (None, None, Some(revision)) => Ok(RequestedGitRef::Revision(revision)),
            (None, None, None) => {
                bail!("git dependency must declare `tag`, `branch`, or `revision`")
            }
            _ => bail!(
                "git dependency must not declare more than one of `tag`, `branch`, or `revision`"
            ),
        }
    }

    pub fn managed_mappings(&self) -> &[ManagedPathSpec] {
        self.managed.as_deref().unwrap_or(&[])
    }
}

impl ManagedPathSpec {
    pub fn normalized_source(&self) -> Result<PathBuf> {
        normalize_manifest_relative_path(&self.source, "managed source path")
    }

    pub fn normalized_target(&self) -> Result<PathBuf> {
        normalize_manifest_relative_path(&self.target, "managed target path")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedGitRef<'a> {
    Tag(&'a str),
    Branch(&'a str),
    Revision(&'a str),
}

impl PackageContents {
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
            && self.agents.is_empty()
            && self.rules.is_empty()
            && self.commands.is_empty()
    }

    pub fn files(&self, package: &LoadedManifest) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for skill in &self.skills {
            files.extend(collect_files(&package.resolve_existing_path(&skill.path)?)?);
        }
        for agent in &self.agents {
            files.push(package.resolve_existing_path(&agent.path)?);
        }
        for rule in &self.rules {
            files.push(package.resolve_existing_path(&rule.path)?);
        }
        for command in &self.commands {
            files.push(package.resolve_existing_path(&command.path)?);
        }
        files.sort();
        files.dedup();
        Ok(files)
    }
}
