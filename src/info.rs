use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anstyle::{AnsiColor, Effects, Style};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::adapters::Adapter;
use crate::git::{ensure_git_dependency, normalize_alias_from_url, normalize_git_url};
use crate::manifest::{
    DependencyComponent, DependencySourceKind, DependencySpec, LoadedManifest, PackageRole,
    load_dependency_from_dir, load_root_from_dir, normalize_dependency_alias,
};
use crate::paths::display_path;
use crate::report::Reporter;

#[derive(Debug, Clone, Serialize)]
pub struct PackageInfo {
    alias: String,
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
    adapters: Vec<Adapter>,
    skills: Vec<String>,
    agents: Vec<String>,
    rules: Vec<String>,
    commands: Vec<String>,
    mcp_servers: Vec<String>,
    dependencies: Vec<String>,
    dev_dependencies: Vec<String>,
    capabilities: Vec<PackageCapability>,
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
        tag: Option<String>,
        branch: Option<String>,
        rev: String,
    },
}

#[derive(Debug, Clone, Serialize)]
struct PackageCapability {
    id: String,
    sensitivity: String,
    justification: Option<String>,
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
            None,
            None,
            role,
        ));
    }

    let normalized_url = normalize_git_url(trimmed);
    let alias = normalize_alias_from_url(&normalized_url)?;
    let checkout = ensure_git_dependency(
        cache_root,
        &normalized_url,
        match (tag, branch) {
            (Some(tag), None) => Some(crate::manifest::RequestedGitRef::Tag(tag)),
            (None, Some(branch)) => Some(crate::manifest::RequestedGitRef::Branch(branch)),
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
            tag: checkout.tag,
            branch: checkout.branch,
            rev: checkout.rev,
        },
        None,
        None,
        role,
    ))
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

fn load_from_dependency_spec(
    alias: &str,
    dependency: &DependencySpec,
    root_manifest: &LoadedManifest,
    cache_root: &Path,
    reporter: &Reporter,
) -> Result<PackageInfo> {
    match dependency.source_kind()? {
        DependencySourceKind::Path => {
            let declared_path = dependency
                .path
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("dependency `{alias}` must declare `path`"))?;
            let package_root = root_manifest.resolve_path(declared_path)?;
            let manifest = load_dependency_from_dir(&package_root)?;
            Ok(package_info_from_loaded(
                alias.to_string(),
                manifest,
                PackageInfoSource::Path {
                    path: declared_path.clone(),
                    tag: dependency.tag.clone(),
                },
                dependency.effective_selected_components(),
                None,
                PackageRole::Dependency,
            ))
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
            Ok(package_info_from_loaded(
                alias.to_string(),
                manifest,
                PackageInfoSource::Git {
                    url: checkout.url,
                    tag: checkout.tag,
                    branch: checkout.branch,
                    rev: checkout.rev,
                },
                dependency.effective_selected_components(),
                dependency.version.as_ref().map(ToString::to_string),
                PackageRole::Dependency,
            ))
        }
    }
}

fn load_package_manifest_for_inspection(root: &Path) -> Result<(LoadedManifest, PackageRole)> {
    match load_root_from_dir(root) {
        Ok(manifest) => Ok((manifest, PackageRole::Root)),
        Err(_) => {
            load_dependency_from_dir(root).map(|manifest| (manifest, PackageRole::Dependency))
        }
    }
}

fn package_info_from_loaded(
    alias: String,
    manifest: LoadedManifest,
    source: PackageInfoSource,
    selected_components: Option<Vec<DependencyComponent>>,
    version_requirement: Option<String>,
    role: PackageRole,
) -> PackageInfo {
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
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    dependencies.sort();
    let mut dev_dependencies = if role == PackageRole::Root {
        manifest
            .manifest
            .dev_dependencies
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    dev_dependencies.sort();

    PackageInfo {
        alias,
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
                tag,
                branch,
                rev,
            } => {
                let mut details = Vec::new();
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
