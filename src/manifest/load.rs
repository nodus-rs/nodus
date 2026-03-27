use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use super::discover::{
    canonicalize_existing_path, discover_package_contents, import_codex_plugin_metadata,
    load_claude_marketplace_wrapper, load_claude_plugin_version, load_codex_marketplace_wrapper,
    load_manifest_str, quote, should_try_plugin_wrapper_fallback,
};
use super::{DependencyKind, LoadedManifest, MANIFEST_FILE, Manifest, PackageRole};
use crate::paths::display_path;

pub fn load_root_from_dir(root: &Path) -> Result<LoadedManifest> {
    load_from_dir(root, PackageRole::Root)
}

pub fn load_dependency_from_dir(root: &Path) -> Result<LoadedManifest> {
    load_from_dir(root, PackageRole::Dependency)
}

pub fn load_from_dir(root: &Path, role: PackageRole) -> Result<LoadedManifest> {
    let root = canonicalize_existing_path(root)
        .with_context(|| format!("failed to access project root {}", root.display()))?;
    let manifest_path = root.join(MANIFEST_FILE);
    let (manifest, warnings, manifest_path) = if manifest_path.exists() {
        let contents = fs::read_to_string(&manifest_path)
            .with_context(|| format!("failed to read manifest {}", manifest_path.display()))?;
        let (manifest, warnings) = load_manifest_str(&manifest_path, &contents)?;
        (manifest, warnings, Some(manifest_path))
    } else {
        (Manifest::default(), Vec::new(), None)
    };

    let discovered = discover_package_contents(&root, &manifest)?;

    let mut loaded = LoadedManifest {
        root: root.clone(),
        manifest_path,
        manifest,
        discovered,
        warnings,
        extra_package_files: Vec::new(),
        allows_empty_dependency_wrapper: false,
        manifest_contents_override: None,
    };

    if should_try_plugin_wrapper_fallback(&loaded) {
        if let Some(marketplace_loaded) = load_claude_marketplace_wrapper(&loaded)? {
            loaded = marketplace_loaded;
        } else if let Some(marketplace_loaded) = load_codex_marketplace_wrapper(&loaded)? {
            loaded = marketplace_loaded;
        }
    }

    import_codex_plugin_metadata(&mut loaded)?;

    if loaded.manifest.version.is_none() {
        loaded.manifest.version = load_claude_plugin_version(&loaded.root)?;
    }

    loaded.validate(role)?;
    Ok(loaded)
}

pub fn serialize_manifest(manifest: &Manifest) -> Result<String> {
    let mut output = String::new();

    if let Some(api_version) = &manifest.api_version {
        output.push_str(&format!("api_version = {}\n", quote(api_version)));
    }
    if let Some(name) = &manifest.name {
        output.push_str(&format!("name = {}\n", quote(name)));
    }
    if let Some(version) = &manifest.version {
        output.push_str(&format!("version = {}\n", quote(&version.to_string())));
    }
    if !manifest.content_roots.is_empty() {
        let encoded = manifest
            .content_roots
            .iter()
            .map(|path| quote(&display_path(path)))
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!("content_roots = [{encoded}]\n"));
    }
    if manifest.publish_root {
        output.push_str("publish_root = true\n");
    }

    if !manifest.capabilities.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        for capability in &manifest.capabilities {
            output.push_str("[[capabilities]]\n");
            output.push_str(&format!("id = {}\n", quote(&capability.id)));
            output.push_str(&format!(
                "sensitivity = {}\n",
                quote(&capability.sensitivity)
            ));
            if let Some(justification) = &capability.justification {
                output.push_str(&format!("justification = {}\n", quote(justification)));
            }
            output.push('\n');
        }
    }

    if !manifest.mcp_servers.is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        for (id, server) in &manifest.mcp_servers {
            output.push_str(&format!("[mcp_servers.{id}]\n"));
            if let Some(command) = &server.command {
                output.push_str(&format!("command = {}\n", quote(command)));
            }
            if let Some(url) = &server.url {
                output.push_str(&format!("url = {}\n", quote(url)));
            }
            if !server.args.is_empty() {
                let encoded = server
                    .args
                    .iter()
                    .map(|arg| quote(arg))
                    .collect::<Vec<_>>()
                    .join(", ");
                output.push_str(&format!("args = [{encoded}]\n"));
            }
            if let Some(cwd) = &server.cwd {
                output.push_str(&format!("cwd = {}\n", quote(&display_path(cwd))));
            }
            if !server.enabled {
                output.push_str("enabled = false\n");
            }
            if !server.env.is_empty() {
                output.push_str("[mcp_servers.");
                output.push_str(id);
                output.push_str(".env]\n");
                for (key, value) in &server.env {
                    output.push_str(&format!("{key} = {}\n", quote(value)));
                }
            }
            output.push('\n');
        }
    }

    if let Some(adapters) = &manifest.adapters {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str("[adapters]\n");
        let mut enabled = adapters.enabled.clone();
        enabled.sort();
        let encoded = enabled
            .into_iter()
            .map(|adapter| quote(adapter.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!("enabled = [{encoded}]\n"));
    }

    if let Some(launch_hooks) = &manifest.launch_hooks {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str("[launch_hooks]\n");
        output.push_str(&format!(
            "sync_on_startup = {}\n",
            launch_hooks.sync_on_startup
        ));
    }

    append_dependency_section(&mut output, manifest, DependencyKind::Dependency);
    append_dependency_section(&mut output, manifest, DependencyKind::DevDependency);

    Ok(output)
}

fn append_dependency_section(output: &mut String, manifest: &Manifest, kind: DependencyKind) {
    let dependencies = manifest.dependency_section(kind);
    if dependencies.is_empty() {
        return;
    }

    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&format!("[{}]\n", kind.manifest_section()));
    for (alias, dependency) in dependencies {
        if dependency.managed.is_some() {
            continue;
        }

        output.push_str(&format!(
            "{alias} = {{ {} }}\n",
            dependency.inline_fields().join(", ")
        ));
    }

    for (alias, dependency) in dependencies {
        let Some(managed) = &dependency.managed else {
            continue;
        };

        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&format!("[{}.{alias}]\n", kind.manifest_section()));
        for field in dependency.key_value_fields() {
            output.push_str(&field);
            output.push('\n');
        }
        for mapping in managed {
            output.push('\n');
            output.push_str(&format!(
                "[[{}.{alias}.managed]]\n",
                kind.manifest_section()
            ));
            output.push_str(&format!(
                "source = {}\n",
                quote(&display_path(&mapping.source))
            ));
            output.push_str(&format!(
                "target = {}\n",
                quote(&display_path(&mapping.target))
            ));
        }
    }
}
