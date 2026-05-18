use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{Adapter, ManagedFile, VirtualPluginSurface, virtual_plugin_surface};
use crate::manifest::LoadedManifest;
use crate::paths::{display_path, strip_path_prefix};
use crate::resolver::ResolvedPackage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VirtualPluginEntry {
    pub package_alias: String,
    pub entry_path: PathBuf,
    pub install_root: PathBuf,
    pub loader_path: PathBuf,
}

pub(crate) trait VirtualPluginBackend: Sync {
    fn adapter(&self) -> Adapter;

    fn entry_paths_from_manifest(&self, manifest: &LoadedManifest) -> Result<Vec<PathBuf>>;

    fn loader_path_for_alias(&self, package_alias: &str, entry_path: &Path) -> PathBuf;

    fn loader_contents(&self, package: &ResolvedPackage, entry: &VirtualPluginEntry) -> Vec<u8>;

    fn entry_paths(&self, package: &ResolvedPackage) -> Result<Vec<PathBuf>> {
        self.entry_paths_from_manifest(&package.manifest)
    }

    fn loader_path(&self, package: &ResolvedPackage, entry_path: &Path) -> PathBuf {
        self.loader_path_for_alias(&package.alias, entry_path)
    }

    fn surface(&self) -> VirtualPluginSurface {
        virtual_plugin_surface(self.adapter())
            .expect("virtual plugin backends require a profile surface")
    }

    fn loader_file_prefix_for_alias(&self, package_alias: &str) -> String {
        format!("{}{}-", self.surface().loader_file_prefix, package_alias)
    }

    fn loader_file_prefix(&self, package: &ResolvedPackage) -> String {
        self.loader_file_prefix_for_alias(&package.alias)
    }
}

pub(crate) fn virtual_plugin_entries_for_manifest(
    backend: &dyn VirtualPluginBackend,
    package_alias: &str,
    manifest: &LoadedManifest,
) -> Result<Vec<VirtualPluginEntry>> {
    let install_root =
        virtual_plugin_install_root_relative_for_alias(backend.adapter(), package_alias);
    let mut entries = backend
        .entry_paths_from_manifest(manifest)?
        .into_iter()
        .map(|entry_path| VirtualPluginEntry {
            package_alias: package_alias.to_string(),
            loader_path: backend.loader_path_for_alias(package_alias, &entry_path),
            install_root: install_root.clone(),
            entry_path,
        })
        .collect::<Vec<_>>();
    sort_entries(&mut entries);
    Ok(entries)
}

pub(crate) fn virtual_plugin_entries_for_package(
    backend: &dyn VirtualPluginBackend,
    package: &ResolvedPackage,
) -> Result<Vec<VirtualPluginEntry>> {
    let install_root = virtual_plugin_install_root_relative(backend.adapter(), package);
    let mut entries = backend
        .entry_paths(package)?
        .into_iter()
        .map(|entry_path| VirtualPluginEntry {
            package_alias: package.alias.clone(),
            loader_path: backend.loader_path(package, &entry_path),
            install_root: install_root.clone(),
            entry_path,
        })
        .collect::<Vec<_>>();
    sort_entries(&mut entries);
    Ok(entries)
}

fn sort_entries(entries: &mut [VirtualPluginEntry]) {
    entries.sort_by(|left, right| {
        left.loader_path
            .cmp(&right.loader_path)
            .then(left.entry_path.cmp(&right.entry_path))
    });
}

pub(crate) fn virtual_plugin_install_root_relative(
    adapter: Adapter,
    package: &ResolvedPackage,
) -> PathBuf {
    virtual_plugin_install_root_relative_for_alias(adapter, &package.alias)
}

pub(crate) fn virtual_plugin_install_root_relative_for_alias(
    adapter: Adapter,
    package_alias: &str,
) -> PathBuf {
    let surface =
        virtual_plugin_surface(adapter).expect("virtual plugin install roots require a surface");
    PathBuf::from(".nodus")
        .join("packages")
        .join(package_alias)
        .join(surface.install_root_name)
}

pub(crate) fn emit_virtual_plugin_files(
    project_root: &Path,
    backend: &dyn VirtualPluginBackend,
    plugin_packages: &[(&ResolvedPackage, &Path)],
) -> Result<Vec<ManagedFile>> {
    let mut files = Vec::new();

    for (package, snapshot_root) in plugin_packages {
        let entries = virtual_plugin_entries_for_package(backend, package)?;
        let Some(first_entry) = entries.first() else {
            continue;
        };

        files.extend(copy_package_files(
            project_root.join(&first_entry.install_root),
            package,
            snapshot_root,
        )?);

        for entry in entries {
            files.push(ManagedFile {
                path: project_root.join(&entry.loader_path),
                contents: backend.loader_contents(package, &entry),
            });
        }
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn copy_package_files(
    target_root: impl AsRef<Path>,
    package: &ResolvedPackage,
    source_root: impl AsRef<Path>,
) -> Result<Vec<ManagedFile>> {
    let target_root = target_root.as_ref();
    let source_root = source_root.as_ref();
    let mut files = Vec::new();

    for path in package.package_files()? {
        let relative = strip_path_prefix(&path, &package.manifest.root)
            .with_context(|| format!("failed to make {} relative", path.display()))?;
        files.push(copy_file(
            target_root.join(relative),
            source_root.join(relative),
        )?);
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn copy_file(target_path: impl AsRef<Path>, source_path: impl AsRef<Path>) -> Result<ManagedFile> {
    let target_path = target_path.as_ref();
    let source_path = source_path.as_ref();
    Ok(ManagedFile {
        path: target_path.to_path_buf(),
        contents: fs::read(source_path)
            .with_context(|| format!("failed to read snapshot file {}", source_path.display()))?,
    })
}

pub(crate) fn display_path_js(path: &Path) -> String {
    display_path(path).replace('\\', "/")
}
