use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use tempfile::TempDir;
use walkdir::WalkDir;

use super::*;
use crate::adapters::{
    Adapter, Adapters, ArtifactKind, ManagedArtifactNames, ManagedPackageIdentities,
    build_output_plan,
};
use crate::git::{
    AddDependencyOptions, AddSummary, RemoveSummary, add_dependency_at_paths_with_adapters,
    add_dependency_in_dir_with_adapters as add_dependency_in_dir_with_adapters_impl,
    normalize_alias_from_url, remove_dependency_at_paths,
    remove_dependency_in_dir as remove_dependency_in_dir_impl, shared_checkout_path,
    shared_repository_path,
};
use crate::install_paths::InstallPaths;
use crate::manifest::{
    DependencyComponent, DependencyKind, MANIFEST_FILE, RequestedGitRef, load_root_from_dir,
};
use crate::paths::{canonicalize_path, display_path};
use crate::report::{ColorMode, Reporter};

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut file = fs::File::create(path).unwrap();
    file.write_all(contents.as_bytes()).unwrap();
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> bool {
    let result = if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    };
    result.is_ok()
}

fn run_git(path: &Path, args: &[&str]) {
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
}

fn write_manifest(path: &Path, contents: &str) {
    write_file(&path.join(MANIFEST_FILE), contents);
}

fn write_skill(path: &Path, name: &str) {
    write_file(
        &path.join("SKILL.md"),
        &format!("---\nname: {name}\ndescription: Example skill.\n---\n# {name}\n"),
    );
}

fn write_codex_agent_toml(path: &Path, name: &str, description: &str, instructions: &str) {
    write_file(
        path,
        &format!(
            "name = {name:?}\ndescription = {description:?}\ndeveloper_instructions = {instructions:?}\n"
        ),
    );
}

fn write_marketplace(path: &Path, contents: &str) {
    write_file(&path.join(".claude-plugin/marketplace.json"), contents);
}

fn write_claude_plugin_json(path: &Path, version: &str) {
    write_file(
        &path.join("claude-code.json"),
        &format!("{{\n  \"name\": \"plugin\",\n  \"version\": \"{version}\"\n}}\n"),
    );
}

fn write_modern_claude_plugin_json(path: &Path, version: &str) {
    write_file(
        &path.join(".claude-plugin/plugin.json"),
        &format!("{{\n  \"name\": \"plugin\",\n  \"version\": \"{version}\"\n}}\n"),
    );
}

fn write_modern_claude_plugin_json_with_fields(path: &Path, fields: &[&str]) {
    let mut all_fields = vec![String::from(r#"  "name": "plugin""#)];
    all_fields.extend(fields.iter().map(|field| field.to_string()));
    write_file(
        &path.join(".claude-plugin/plugin.json"),
        &format!("{{\n{}\n}}\n", all_fields.join(",\n")),
    );
}

fn write_codex_marketplace(path: &Path, contents: &str) {
    write_file(&path.join(".agents/plugins/marketplace.json"), contents);
}

fn generated_claude_marketplace_path(path: &Path) -> PathBuf {
    path.join(".nodus-global/.claude-plugin/marketplace.json")
}

fn generated_codex_marketplace_path(path: &Path) -> PathBuf {
    path.join(".nodus-global/.agents/plugins/marketplace.json")
}

fn generated_codex_marketplace_root(path: &Path) -> PathBuf {
    path.join(".nodus-global")
}

fn generated_codex_user_config_path(path: &Path) -> PathBuf {
    path.join(".codex-user/config.toml")
}

fn generated_global_packages_root(path: &Path) -> PathBuf {
    path.join(".nodus-global/packages")
}

fn global_native_plugin_root(
    project_root: &Path,
    package: &ResolvedPackage,
    adapter: Adapter,
) -> PathBuf {
    let identities = ManagedPackageIdentities::from_resolved_packages([package]);
    match adapter {
        Adapter::Claude => generated_global_packages_root(project_root)
            .join(identities.managed_package_id(package))
            .join("claude-plugin"),
        Adapter::Codex => generated_global_packages_root(project_root)
            .join(identities.managed_package_id(package))
            .join("codex-plugin"),
        Adapter::OpenCode => generated_global_packages_root(project_root)
            .join(identities.managed_package_id(package))
            .join("opencode-plugin"),
        Adapter::Agents | Adapter::Copilot | Adapter::Cursor => {
            panic!("adapter {adapter} does not have a native plugin root")
        }
    }
}

fn dependency_managed_package_id(resolution: &Resolution) -> String {
    let package = resolution
        .packages
        .iter()
        .find(|package| !matches!(package.source, PackageSource::Root))
        .expect("resolution should include a dependency package");
    let identities = ManagedPackageIdentities::from_resolved_packages(resolution.packages.iter());
    identities.managed_package_id(package)
}

fn read_codex_project_config(path: &Path) -> toml::Value {
    toml::from_str(&fs::read_to_string(path.join(".codex/config.toml")).unwrap()).unwrap()
}

fn read_codex_user_config(path: &Path) -> toml::Value {
    toml::from_str(&fs::read_to_string(generated_codex_user_config_path(path)).unwrap()).unwrap()
}

fn generated_codex_user_overlay_path(path: &Path, profile: &str) -> PathBuf {
    path.join(format!(".codex-user/{profile}.config.toml"))
}

fn read_codex_user_overlay(path: &Path, profile: &str) -> toml::Value {
    toml::from_str(&fs::read_to_string(generated_codex_user_overlay_path(path, profile)).unwrap())
        .unwrap()
}

/// Assert the base `$CODEX_HOME/config.toml` carries no nodus-managed
/// marketplace registration (it is either absent or cleaned).
fn assert_codex_user_base_has_no_managed_marketplace(path: &Path) {
    let base = generated_codex_user_config_path(path);
    if !base.exists() {
        return;
    }
    let config: toml::Value = toml::from_str(&fs::read_to_string(&base).unwrap()).unwrap();
    if let Some(marketplaces) = config.get("marketplaces").and_then(toml::Value::as_table) {
        assert!(
            !marketplaces.contains_key("nodus"),
            "base Codex user config should not register the nodus marketplace; config was {config:?}"
        );
    }
}

fn assert_codex_user_config_registers_plugins(project_root: &Path, plugin_keys: &[&str]) {
    let config = read_codex_user_config(project_root);
    assert_codex_config_registers_plugins(project_root, &config, plugin_keys);
}

fn assert_codex_config_registers_plugins(
    project_root: &Path,
    config: &toml::Value,
    plugin_keys: &[&str],
) {
    assert_eq!(
        config["marketplaces"]["nodus"]["source_type"].as_str(),
        Some("local")
    );
    assert_eq!(
        config["marketplaces"]["nodus"]["source"].as_str(),
        Some(display_path(&generated_codex_marketplace_root(project_root)).as_str())
    );
    for plugin_key in plugin_keys {
        let enabled = config
            .get("plugins")
            .and_then(toml::Value::as_table)
            .and_then(|plugins| plugins.get(*plugin_key))
            .and_then(|plugin| plugin.get("enabled"))
            .and_then(toml::Value::as_bool);
        assert_eq!(
            enabled,
            Some(true),
            "expected Codex user config to enable `{plugin_key}`; config was {config:?}"
        );
    }
}

fn assert_no_codex_managed_marketplace_config(
    project_root: &Path,
    marketplace: &str,
) -> toml::Value {
    let config = read_codex_project_config(project_root);
    if let Some(marketplaces) = config.get("marketplaces").and_then(toml::Value::as_table) {
        assert!(
            !marketplaces.contains_key(marketplace),
            "Codex project config should not register managed marketplace `{marketplace}`"
        );
    }
    if let Some(plugins) = config.get("plugins").and_then(toml::Value::as_table) {
        let suffix = format!("@{marketplace}");
        assert!(
            plugins.keys().all(|key| !key.ends_with(&suffix)),
            "Codex project config should not enable managed plugins for `{marketplace}`"
        );
    }
    config
}

fn write_codex_plugin_json(path: &Path, version: &str, mcp_servers_path: Option<&str>) {
    let mut fields = vec![
        String::from(r#"  "name": "plugin""#),
        format!(r#"  "version": "{version}""#),
    ];
    if let Some(mcp_servers_path) = mcp_servers_path {
        fields.push(format!(r#"  "mcpServers": "{mcp_servers_path}""#));
    }
    write_file(
        &path.join(".codex-plugin/plugin.json"),
        &format!("{{\n{}\n}}\n", fields.join(",\n")),
    );
}

fn write_codex_mcp_config(path: &Path) {
    write_file(
        &path.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "figma": {
      "url": "http://127.0.0.1:3845/mcp"
    }
  }
}
"#,
    );
}

fn namespaced_skill_id(package: &ResolvedPackage, skill_id: &str) -> String {
    ManagedArtifactNames::from_resolved_packages([package]).managed_skill_id(package, skill_id)
}

fn namespaced_file_name(package: &ResolvedPackage, artifact_id: &str, extension: &str) -> String {
    let kind = match extension {
        "agent.md" | "md"
            if package
                .manifest
                .discovered
                .agents
                .iter()
                .any(|agent| agent.id == artifact_id) =>
        {
            ArtifactKind::Agent
        }
        "mdc" => ArtifactKind::Rule,
        "md" => {
            if package
                .manifest
                .discovered
                .rules
                .iter()
                .any(|rule| rule.id == artifact_id)
            {
                ArtifactKind::Rule
            } else {
                ArtifactKind::Command
            }
        }
        _ => ArtifactKind::Command,
    };
    ManagedArtifactNames::from_resolved_packages([package]).managed_file_name(
        package,
        kind,
        artifact_id,
        extension,
    )
}

fn adapter_runtime_root_name(adapter: Adapter) -> &'static str {
    match adapter {
        Adapter::Agents => ".agents",
        Adapter::Claude => ".claude",
        Adapter::Codex => ".codex",
        Adapter::Copilot => ".github",
        Adapter::Cursor => ".cursor",
        Adapter::OpenCode => ".opencode",
    }
}

fn path_contains_adapter_runtime(path: &Path, adapter: Adapter) -> bool {
    let runtime = adapter_runtime_root_name(adapter);
    if path
        .components()
        .any(|component| component.as_os_str() == runtime)
    {
        return true;
    }

    let plugin_root = match adapter {
        Adapter::Claude => "claude-plugin",
        Adapter::Codex => "codex-plugin",
        Adapter::Agents | Adapter::Copilot | Adapter::Cursor | Adapter::OpenCode => {
            return false;
        }
    };
    path.components()
        .any(|component| component.as_os_str() == plugin_root)
}

fn runtime_skill_paths(project_root: &Path, adapter: Adapter, skill_id: &str) -> Vec<PathBuf> {
    let mut paths = WalkDir::new(project_root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == "SKILL.md")
        .map(|entry| entry.into_path())
        .filter(|path| {
            path_contains_adapter_runtime(path, adapter)
                && path
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|name| name == skill_id)
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn runtime_skill_path(project_root: &Path, adapter: Adapter, skill_id: &str) -> PathBuf {
    let mut paths = runtime_skill_paths(project_root, adapter, skill_id);
    assert_eq!(paths.len(), 1, "expected one {adapter} skill `{skill_id}`");
    paths.remove(0)
}

fn runtime_skill_exists(project_root: &Path, adapter: Adapter, skill_id: &str) -> bool {
    !runtime_skill_paths(project_root, adapter, skill_id).is_empty()
}

fn runtime_file_paths(project_root: &Path, adapter: Adapter, file_name: &str) -> Vec<PathBuf> {
    let mut paths = WalkDir::new(project_root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == file_name)
        .map(|entry| entry.into_path())
        .filter(|path| path_contains_adapter_runtime(path, adapter))
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn runtime_file_path(project_root: &Path, adapter: Adapter, file_name: &str) -> PathBuf {
    let mut paths = runtime_file_paths(project_root, adapter, file_name);
    assert_eq!(paths.len(), 1, "expected one {adapter} file `{file_name}`");
    paths.remove(0)
}

fn runtime_file_exists(project_root: &Path, adapter: Adapter, file_name: &str) -> bool {
    !runtime_file_paths(project_root, adapter, file_name).is_empty()
}

fn plugin_hook_script_path(
    project_root: &Path,
    _package_alias: &str,
    name_fragment: &str,
) -> PathBuf {
    let scripts_root = generated_global_packages_root(project_root);
    let matches = WalkDir::new(&scripts_root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| {
            path.components()
                .any(|component| component.as_os_str() == "claude-plugin")
                && path
                    .components()
                    .any(|component| component.as_os_str() == "scripts")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains(name_fragment))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "expected one plugin hook script containing `{name_fragment}` in {}",
        scripts_root.display()
    );
    matches.into_iter().next().unwrap()
}

fn global_plugin_file_exists(
    project_root: &Path,
    adapter_plugin_dir: &str,
    path_fragment: &str,
) -> bool {
    WalkDir::new(generated_global_packages_root(project_root))
        .into_iter()
        .filter_map(Result::ok)
        .any(|entry| {
            entry.file_type().is_file()
                && entry
                    .path()
                    .components()
                    .any(|component| component.as_os_str() == adapter_plugin_dir)
                && display_path(entry.path()).contains(path_fragment)
        })
}

fn activation_context_from_script(script: &str) -> String {
    let json_line = script
        .lines()
        .find(|line| line.starts_with("{\"hookSpecificOutput\""))
        .expect("activation script should embed hook output JSON");
    let output: serde_json::Value = serde_json::from_str(json_line).unwrap();
    output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_string()
}

fn managed_skill_file(
    project_root: &Path,
    adapter: Adapter,
    package: &ResolvedPackage,
    skill_id: &str,
) -> PathBuf {
    let names = ManagedArtifactNames::from_resolved_packages([package]);
    let runtime_root = if matches!(adapter, Adapter::Claude | Adapter::Codex)
        && !matches!(package.source, PackageSource::Root)
        && !matches!(
            project_root.file_name().and_then(|name| name.to_str()),
            Some("claude-plugin") | Some("codex-plugin")
        ) {
        global_native_plugin_root(project_root, package, adapter)
    } else {
        project_root.to_path_buf()
    };
    crate::adapters::managed_skill_root(&names, &runtime_root, adapter, package, skill_id)
        .join("SKILL.md")
}

fn managed_artifact_file(
    project_root: &Path,
    adapter: Adapter,
    kind: ArtifactKind,
    package: &ResolvedPackage,
    artifact_id: &str,
) -> PathBuf {
    let names = ManagedArtifactNames::from_resolved_packages([package]);
    let runtime_root = if adapter == Adapter::Claude
        && !matches!(package.source, PackageSource::Root)
        && project_root.file_name().and_then(|name| name.to_str()) != Some("claude-plugin")
    {
        global_native_plugin_root(project_root, package, adapter)
    } else {
        project_root.to_path_buf()
    };
    crate::adapters::managed_artifact_path(
        &names,
        &runtime_root,
        adapter,
        kind,
        package,
        artifact_id,
    )
    .unwrap()
}

fn simulate_legacy_direct_claude_codex_skill_outputs(project_root: &Path) {
    let _ = fs::remove_dir_all(project_root.join(".agents"));
    let _ = fs::remove_dir_all(project_root.join(".claude"));
    let _ = fs::remove_dir_all(project_root.join(".claude-plugin"));
    let _ = fs::remove_dir_all(project_root.join(".codex"));
    let _ = fs::remove_dir_all(project_root.join(".nodus"));

    write_skill(
        &project_root.join(".claude/skills/review"),
        "Legacy Claude Review",
    );
    write_skill(
        &project_root.join(".codex/skills/review"),
        "Legacy Codex Review",
    );

    let mut lockfile = Lockfile::read(&project_root.join(LOCKFILE_NAME)).unwrap();
    lockfile.legacy_managed_files = vec![
        ".claude/skills/review".into(),
        ".codex/skills/review".into(),
    ];
    lockfile.write(&project_root.join(LOCKFILE_NAME)).unwrap();
}

fn resolution_skill_id(
    resolution: &Resolution,
    package: &ResolvedPackage,
    skill_id: &str,
) -> String {
    ManagedArtifactNames::from_resolved_packages(resolution.packages.iter())
        .managed_skill_id(package, skill_id)
}

fn resolution_file_name(
    resolution: &Resolution,
    package: &ResolvedPackage,
    kind: ArtifactKind,
    artifact_id: &str,
    extension: &str,
) -> String {
    ManagedArtifactNames::from_resolved_packages(resolution.packages.iter()).managed_file_name(
        package,
        kind,
        artifact_id,
        extension,
    )
}

fn resolution_codex_command_skill_id(
    _resolution: &Resolution,
    package: &ResolvedPackage,
    command_id: &str,
) -> String {
    let names = ManagedArtifactNames::from_resolved_packages([package]);
    crate::adapters::codex::synthetic_command_skill_id(&names, package, command_id)
}

fn init_git_repo(path: &Path) {
    run_git(path, &["init"]);
    run_git(path, &["config", "user.email", "test@example.com"]);
    run_git(path, &["config", "user.name", "Test User"]);
    run_git(path, &["config", "core.autocrlf", "false"]);
    write_file(&path.join(".gitattributes"), "* text eol=lf\n");
    run_git(path, &["add", "."]);
    run_git(path, &["commit", "-m", "initial"]);
}

fn create_git_dependency() -> (TempDir, String) {
    let repo = TempDir::new().unwrap();
    write_skill(&repo.path().join("skills/review"), "Review");
    write_file(&repo.path().join("agents/security.md"), "# Security\n");
    init_git_repo(repo.path());

    let output = Command::new("git")
        .args(["tag", "v0.1.0"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let url = repo.path().to_string_lossy().to_string();
    (repo, url)
}

fn create_workspace_dependency() -> TempDir {
    let repo = TempDir::new().unwrap();
    write_workspace_dependency(repo.path());
    init_git_repo(repo.path());
    tag_repo(repo.path(), "v0.2.0");
    repo
}

fn write_workspace_dependency(path: &Path) {
    write_manifest(
        path,
        r#"
[workspace]
members = ["plugins/axiom", "plugins/firebase"]

[workspace.package.axiom]
path = "plugins/axiom"
name = "Axiom"

[workspace.package.axiom.codex]
category = "Productivity"
installation = "AVAILABLE"
authentication = "ON_INSTALL"

[workspace.package.firebase]
path = "plugins/firebase"
name = "Firebase"

[workspace.package.firebase.codex]
category = "Productivity"
installation = "AVAILABLE"
authentication = "ON_INSTALL"
"#,
    );
    write_skill(&path.join("plugins/axiom/skills/review"), "Review");
    write_skill(&path.join("plugins/firebase/skills/checks"), "Checks");
}

fn write_namespaced_workspace_dependency(path: &Path) {
    write_manifest(
        path,
        r#"
[workspace]
members = ["plugins/core", "plugins/rust"]
namespace = "ena"

[workspace.package.core]
path = "plugins/core"
name = "Core"

[workspace.package.core.codex]
category = "Productivity"
installation = "AVAILABLE"
authentication = "ON_INSTALL"

[workspace.package.rust]
path = "plugins/rust"
name = "Rust"

[workspace.package.rust.codex]
category = "Productivity"
installation = "AVAILABLE"
authentication = "ON_INSTALL"
"#,
    );
    write_skill(&path.join("plugins/core/skills/plan"), "Plan");
    write_skill(&path.join("plugins/rust/skills/checks"), "Checks");
}

fn write_single_workspace_dependency(path: &Path) {
    write_manifest(
        path,
        r#"
[workspace]
members = ["plugins/axiom"]

[workspace.package.axiom]
path = "plugins/axiom"
name = "Axiom"

[workspace.package.axiom.codex]
category = "Productivity"
installation = "AVAILABLE"
authentication = "ON_INSTALL"
"#,
    );
    write_skill(&path.join("plugins/axiom/skills/review"), "Review");
}

fn write_workspace_dependency_with_invalid_member(path: &Path) {
    write_manifest(
        path,
        r#"
[workspace]
members = ["plugins/axiom", "plugins/firebase"]

[workspace.package.axiom]
path = "plugins/axiom"
name = "Axiom"

[workspace.package.axiom.codex]
category = "Productivity"
installation = "AVAILABLE"
authentication = "ON_INSTALL"

[workspace.package.firebase]
path = "plugins/firebase"
name = "Firebase"

[workspace.package.firebase.codex]
category = "Productivity"
installation = "AVAILABLE"
authentication = "ON_INSTALL"
"#,
    );
    write_skill(&path.join("plugins/axiom/skills/review"), "Review");
    write_file(
        &path.join("plugins/firebase/README.md"),
        "# Not a package\n",
    );
}

fn write_workspace_dependency_with_non_codex_member(path: &Path) {
    write_manifest(
        path,
        r#"
[workspace]
members = ["plugins/axiom", "plugins/firebase"]

[workspace.package.axiom]
path = "plugins/axiom"
name = "Axiom"

[workspace.package.axiom.codex]
category = "Productivity"
installation = "AVAILABLE"
authentication = "ON_INSTALL"

[workspace.package.firebase]
path = "plugins/firebase"
name = "Firebase"
"#,
    );
    write_skill(&path.join("plugins/axiom/skills/review"), "Review");
    write_skill(&path.join("plugins/firebase/skills/checks"), "Checks");
}

fn tag_repo(path: &Path, tag: &str) {
    run_git(path, &["tag", tag]);
}

fn rename_current_branch(path: &Path, branch: &str) {
    run_git(path, &["branch", "-m", branch]);
}

fn commit_all(path: &Path, message: &str) {
    let output = Command::new("git")
        .args(["add", "."])
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn cache_dir() -> TempDir {
    TempDir::new().unwrap()
}

#[derive(Clone, Default)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl SharedBuffer {
    fn contents(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn resolve_project(root: &Path, cache_root: &Path, mode: ResolveMode) -> Result<Resolution> {
    let reporter = Reporter::silent();
    super::resolve_project(
        root,
        cache_root,
        mode,
        &reporter,
        super::resolve::ResolveProjectOptions::new(
            None,
            None,
            None,
            DependencyFailureMode::Graceful,
        ),
    )
}

fn sync_in_dir(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
) -> Result<SyncSummary> {
    let reporter = Reporter::silent();
    super::sync_in_dir_with_adapters(
        cwd,
        cache_root,
        locked,
        allow_high_sensitivity,
        false,
        &[],
        false,
        &reporter,
    )
}

fn sync_in_dir_frozen(
    cwd: &Path,
    cache_root: &Path,
    allow_high_sensitivity: bool,
) -> Result<SyncSummary> {
    let reporter = Reporter::silent();
    super::sync_in_dir_with_adapters_frozen(
        cwd,
        cache_root,
        allow_high_sensitivity,
        false,
        &[],
        false,
        &reporter,
    )
}

fn sync_in_dir_strict(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
) -> Result<SyncSummary> {
    let reporter = Reporter::silent();
    super::sync_in_dir_with_adapters_strict(
        cwd,
        cache_root,
        locked,
        allow_high_sensitivity,
        false,
        &[],
        false,
        &reporter,
    )
}

fn sync_in_dir_with_adapters(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    adapters: &[Adapter],
) -> Result<SyncSummary> {
    let reporter = Reporter::silent();
    super::sync_in_dir_with_adapters(
        cwd,
        cache_root,
        locked,
        allow_high_sensitivity,
        false,
        adapters,
        false,
        &reporter,
    )
}

fn sync_in_dir_with_adapters_no_fast_path(
    cwd: &Path,
    cache_root: &Path,
    adapters: &[Adapter],
) -> Result<SyncSummary> {
    let reporter = Reporter::silent();
    super::sync_in_dir_with_adapters_full(
        cwd, cache_root, false, false, false, adapters, false, true, None, &reporter,
    )
}

fn sync_in_dir_with_adapters_force(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    adapters: &[Adapter],
) -> Result<SyncSummary> {
    let reporter = Reporter::silent();
    super::sync_in_dir_with_adapters(
        cwd,
        cache_root,
        locked,
        allow_high_sensitivity,
        true,
        adapters,
        false,
        &reporter,
    )
}

fn sync_in_dir_with_adapters_dry_run_force(
    cwd: &Path,
    cache_root: &Path,
    locked: bool,
    allow_high_sensitivity: bool,
    adapters: &[Adapter],
) -> Result<SyncSummary> {
    let reporter = Reporter::silent();
    super::sync_in_dir_with_adapters_dry_run(
        cwd,
        cache_root,
        locked,
        allow_high_sensitivity,
        true,
        adapters,
        false,
        &reporter,
    )
}

struct StubManagedCollisionResolver {
    choice: ManagedCollisionChoice,
}

impl ManagedCollisionResolver for StubManagedCollisionResolver {
    fn resolve(
        &mut self,
        _project_root: &Path,
        _collision: &ManagedCollision,
    ) -> Result<ManagedCollisionChoice> {
        Ok(self.choice)
    }
}

fn sync_in_dir_with_collision_choice(
    cwd: &Path,
    cache_root: &Path,
    choice: ManagedCollisionChoice,
) -> Result<SyncSummary> {
    let reporter = Reporter::silent();
    let mut resolver = StubManagedCollisionResolver { choice };
    let install_paths = InstallPaths::project(cwd);
    super::sync_in_dir_with_adapters_mode_and_collision_resolution(
        &install_paths,
        cache_root,
        SyncMode::Normal,
        false,
        false,
        &Adapter::ALL,
        false,
        ExecutionMode::Apply,
        None,
        DependencyFailureMode::Graceful,
        false,
        None,
        Some(&mut resolver),
        &reporter,
    )
}

fn doctor_in_dir(cwd: &Path, cache_root: &Path) -> Result<DoctorSummary> {
    let reporter = Reporter::silent();
    super::doctor_in_dir_with_mode(cwd, cache_root, DoctorMode::Repair, &reporter)
}

fn doctor_in_dir_with_mode(
    cwd: &Path,
    cache_root: &Path,
    mode: DoctorMode,
    reporter: &Reporter,
) -> Result<DoctorSummary> {
    super::doctor_in_dir_with_mode(cwd, cache_root, mode, reporter)
}

fn resolve_project_from_existing_lockfile_in_dir(
    cwd: &Path,
    cache_root: &Path,
    adapters: &[Adapter],
) -> Result<(Resolution, Lockfile)> {
    let reporter = Reporter::silent();
    super::resolve_project_from_existing_lockfile_in_dir(
        cwd,
        cache_root,
        Adapters::from_slice(adapters),
        &reporter,
    )
}

fn add_dependency_in_dir_with_adapters(
    project_root: &Path,
    cache_root: &Path,
    url: &str,
    tag: Option<&str>,
    adapters: &[Adapter],
    components: &[DependencyComponent],
) -> Result<AddSummary> {
    add_dependency_in_dir_with_adapters_accept_all(
        project_root,
        cache_root,
        url,
        tag,
        adapters,
        components,
        false,
    )
}

fn add_dependency_in_dir_with_adapters_accept_all(
    project_root: &Path,
    cache_root: &Path,
    url: &str,
    tag: Option<&str>,
    adapters: &[Adapter],
    components: &[DependencyComponent],
    accept_all_dependencies: bool,
) -> Result<AddSummary> {
    let reporter = Reporter::silent();
    add_dependency_in_dir_with_adapters_impl(
        project_root,
        cache_root,
        url,
        AddDependencyOptions {
            git_ref: tag.map(RequestedGitRef::Tag),
            version_req: None,
            kind: DependencyKind::Dependency,
            adapters,
            components,
            sync_on_launch: false,
            accept_all_dependencies,
        },
        &reporter,
    )
}

fn add_dependency_in_dir_with_git_ref(
    project_root: &Path,
    cache_root: &Path,
    url: &str,
    git_ref: RequestedGitRef<'_>,
    adapters: &[Adapter],
    components: &[DependencyComponent],
) -> Result<AddSummary> {
    let reporter = Reporter::silent();
    add_dependency_in_dir_with_adapters_impl(
        project_root,
        cache_root,
        url,
        AddDependencyOptions {
            git_ref: Some(git_ref),
            version_req: None,
            kind: DependencyKind::Dependency,
            adapters,
            components,
            sync_on_launch: false,
            accept_all_dependencies: false,
        },
        &reporter,
    )
}

fn remove_dependency_in_dir(
    project_root: &Path,
    cache_root: &Path,
    package: &str,
) -> Result<RemoveSummary> {
    let reporter = Reporter::silent();
    remove_dependency_in_dir_impl(project_root, cache_root, package, &reporter)
}

fn sync_all(project_root: &Path, cache_root: &Path) {
    sync_in_dir_with_adapters(project_root, cache_root, false, false, &Adapter::ALL).unwrap();
}

/// Slice-3 migration helper: asserts that the v10 per-package ownership view
/// claims `relative` (a workspace-relative path) as Nodus-owned. v10
/// lockfiles no longer populate `legacy_managed_files`, so the previous
/// `lockfile.legacy_managed_files.contains(...)` shape of these tests needs
/// the equivalent semantic check via [`Lockfile::owned_set`].
#[track_caller]
fn assert_owned(lockfile: &Lockfile, project_root: &Path, relative: &str) {
    let owned = lockfile.owned_set(project_root).unwrap();
    let target = project_root.join(relative);
    assert!(
        owned.contains(&target),
        "expected Nodus to own `{relative}`; owned_set:\n  exact={:?}\n  subtrees={:?}\n  prefixes={:?}",
        owned.exact,
        owned.subtrees,
        owned.prefixes,
    );
}

#[track_caller]
fn assert_not_owned(lockfile: &Lockfile, project_root: &Path, relative: &str) {
    let owned = lockfile.owned_set(project_root).unwrap();
    let target = project_root.join(relative);
    assert!(
        !owned.contains(&target),
        "expected Nodus to NOT own `{relative}`; owned_set:\n  exact={:?}\n  subtrees={:?}\n  prefixes={:?}",
        owned.exact,
        owned.subtrees,
        owned.prefixes,
    );
}

fn sync_all_result(project_root: &Path, cache_root: &Path) -> Result<SyncSummary> {
    sync_in_dir_with_adapters(project_root, cache_root, false, false, &Adapter::ALL)
}

fn sync_all_force_result(project_root: &Path, cache_root: &Path) -> Result<SyncSummary> {
    sync_in_dir_with_adapters_force(project_root, cache_root, false, false, &Adapter::ALL)
}

fn add_dependency_all(project_root: &Path, cache_root: &Path, url: &str, tag: Option<&str>) {
    add_dependency_in_dir_with_adapters(project_root, cache_root, url, tag, &Adapter::ALL, &[])
        .unwrap();
}

fn git_output(path: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn stage_git_symlink(path: &Path, link: &Path, target: &str) {
    let target_blob_path = path.join(".git-symlink-target");
    write_file(&target_blob_path, target);
    let blob = git_output(path, &["hash-object", "-w", "--", ".git-symlink-target"]);
    fs::remove_file(target_blob_path).unwrap();
    run_git(
        path,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{blob},{}", display_path(link)),
        ],
    );
}

fn canonicalize_git_path_output(path: String) -> PathBuf {
    canonicalize_path(&PathBuf::from(path)).unwrap()
}

fn toml_path_value(path: &Path) -> String {
    display_path(path)
}

#[test]
fn resolves_local_path_dependencies_with_discovery() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );

    write_skill(&temp.path().join("vendor/shared/skills/checks"), "Checks");

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let lockfile = resolution
        .to_lockfile(Adapters::from_slice(&Adapter::ALL), temp.path())
        .unwrap();

    assert_eq!(lockfile.packages.len(), 2);
    assert_eq!(lockfile.packages[0].alias, "root");
    assert_eq!(lockfile.packages[1].alias, "shared");
    assert_not_owned(&lockfile, temp.path(), ".claude/skills/review");
    let shared = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), shared, Adapter::Claude);
    let plugin_root_relative = display_path(plugin_root.strip_prefix(temp.path()).unwrap());
    assert_not_owned(&lockfile, temp.path(), &plugin_root_relative);
}

#[test]
fn sync_installs_root_skill_dependency() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let package = temp.path().join("vendor/root-skill-pack");
    write_skill(&package, "Root Skill");
    write_file(&package.join("assets/reference.md"), "asset notes\n");
    write_file(&package.join(".git/config"), "private git metadata\n");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies]
root_skill_pack = { path = "vendor/root-skill-pack" }
"#,
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Claude]).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "root_skill_pack")
        .unwrap();
    let emitted_skill = global_native_plugin_root(temp.path(), dependency, Adapter::Claude)
        .join("skills/root-skill-pack");
    assert!(emitted_skill.join("SKILL.md").exists());
    assert_eq!(
        fs::read_to_string(emitted_skill.join("assets/reference.md")).unwrap(),
        "asset notes\n"
    );
    assert!(!emitted_skill.join(".git/config").exists());
}

#[test]
fn resolves_local_path_dependencies_with_configured_content_roots() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
content_roots = ["nodus-development"]
"#,
    );
    write_skill(
        &temp
            .path()
            .join("vendor/shared/nodus-development/skills/checks"),
        "Checks",
    );

    sync_all(temp.path(), cache.path());

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "checks");

    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
    assert!(
        temp.path()
            .join(format!(".cursor/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
}

#[test]
fn add_dependency_clones_repo_and_updates_manifest() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let (_repo, url) = create_git_dependency();

    add_dependency_all(temp.path(), cache.path(), &url, Some("v0.1.0"));

    let mirror_path = shared_repository_path(cache.path(), &url).unwrap();
    let rev = git_output(&mirror_path, &["rev-parse", "v0.1.0^{commit}"]);
    let checkout_path = shared_checkout_path(cache.path(), &url, &rev).unwrap();
    assert!(mirror_path.exists());
    assert!(checkout_path.exists());
    assert_eq!(
        git_output(&mirror_path, &["rev-parse", "--is-bare-repository"]),
        "true"
    );
    assert_eq!(
        canonicalize_git_path_output(git_output(
            &checkout_path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"]
        )),
        canonicalize_path(&mirror_path).unwrap()
    );
    let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
    assert!(manifest.contains("[dependencies]"));
    assert!(manifest.contains("tag = \"v0.1.0\""));
    assert!(manifest.contains("url = "));
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let dependency_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias != "root")
        .unwrap();
    assert!(
        !dependency_package.owned_subtrees.is_empty()
            || !dependency_package.owned_files.is_empty()
            || !dependency_package.owned_prefixes.is_empty(),
        "dependency package should declare at least one ownership rule after sync"
    );
    assert_eq!(dependency_package.version_tag.as_deref(), Some("v0.1.0"));

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias != "root")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
}

#[test]
fn add_dependency_writes_selected_components_to_manifest() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let (_repo, url) = create_git_dependency();

    add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &url,
        Some("v0.1.0"),
        &[Adapter::Codex],
        &[DependencyComponent::Agents, DependencyComponent::Skills],
    )
    .unwrap();

    let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
    assert!(manifest.contains("components = [\"skills\", \"agents\"]"));
}

#[test]
fn add_dependency_uses_latest_tag_when_not_provided() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let repo = TempDir::new().unwrap();
    write_skill(&repo.path().join("skills/review"), "Review");
    init_git_repo(repo.path());

    for tag in ["v0.1.0", "v1.2.0", "v0.9.0"] {
        let output = Command::new("git")
            .args(["tag", tag])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &repo.path().to_string_lossy(),
        None,
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
    assert!(manifest.contains("tag = \"v1.2.0\""));
}

#[test]
fn managed_package_ids_prefer_tag_branch_version_then_revision() {
    let tag_project = TempDir::new().unwrap();
    let tag_cache = cache_dir();
    let tagged_repo = TempDir::new().unwrap();
    write_manifest(tagged_repo.path(), r#"name = "playbook-ios""#);
    write_skill(&tagged_repo.path().join("skills/review"), "Review");
    init_git_repo(tagged_repo.path());
    tag_repo(tagged_repo.path(), "v0.6.1");

    add_dependency_in_dir_with_adapters(
        tag_project.path(),
        tag_cache.path(),
        &tagged_repo.path().to_string_lossy(),
        Some("v0.6.1"),
        &[Adapter::Claude],
        &[],
    )
    .unwrap();
    let (resolution, _) = resolve_project_from_existing_lockfile_in_dir(
        tag_project.path(),
        tag_cache.path(),
        &[Adapter::Claude],
    )
    .unwrap();
    assert_eq!(
        dependency_managed_package_id(&resolution),
        "playbook-ios+v0.6.1"
    );

    // A branch pin is the human-stable identity and wins over the manifest
    // version, so every package from the same repo + commit lines up on the
    // branch suffix (e.g. `ena+main`, `ena-core+main`) instead of drifting onto
    // per-package versions or digests.
    let branch_over_version_project = TempDir::new().unwrap();
    let branch_over_version_cache = cache_dir();
    let branch_over_version_repo = TempDir::new().unwrap();
    write_manifest(
        branch_over_version_repo.path(),
        r#"name = "playbook-ios"
version = "0.6.1"
"#,
    );
    write_skill(
        &branch_over_version_repo.path().join("skills/review"),
        "Review",
    );
    init_git_repo(branch_over_version_repo.path());
    rename_current_branch(branch_over_version_repo.path(), "main");

    add_dependency_in_dir_with_adapters(
        branch_over_version_project.path(),
        branch_over_version_cache.path(),
        &branch_over_version_repo.path().to_string_lossy(),
        None,
        &[Adapter::Claude],
        &[],
    )
    .unwrap();
    let (resolution, _) = resolve_project_from_existing_lockfile_in_dir(
        branch_over_version_project.path(),
        branch_over_version_cache.path(),
        &[Adapter::Claude],
    )
    .unwrap();
    assert_eq!(
        dependency_managed_package_id(&resolution),
        "playbook-ios+main"
    );

    // Version fallback: a revision pin records no branch, so the manifest
    // version supplies the suffix (rather than the bare revision hash).
    let version_project = TempDir::new().unwrap();
    let version_cache = cache_dir();
    let version_repo_parent = TempDir::new().unwrap();
    let version_repo = version_repo_parent.path().join("playbook-ios");
    write_manifest(&version_repo, r#"version = "0.6.1""#);
    write_skill(&version_repo.join("skills/review"), "Review");
    init_git_repo(&version_repo);
    let version_rev = crate::git::current_rev(&version_repo).unwrap();

    add_dependency_in_dir_with_git_ref(
        version_project.path(),
        version_cache.path(),
        &version_repo.to_string_lossy(),
        RequestedGitRef::Revision(version_rev.as_str()),
        &[Adapter::Claude],
        &[],
    )
    .unwrap();
    let (resolution, _) = resolve_project_from_existing_lockfile_in_dir(
        version_project.path(),
        version_cache.path(),
        &[Adapter::Claude],
    )
    .unwrap();
    assert_eq!(
        dependency_managed_package_id(&resolution),
        "playbook-ios+0.6.1"
    );
    let package_dirs = fs::read_dir(generated_global_packages_root(version_project.path()))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(package_dirs, vec!["playbook-ios+0.6.1"]);

    let branch_project = TempDir::new().unwrap();
    let branch_cache = cache_dir();
    let branch_repo = TempDir::new().unwrap();
    write_manifest(branch_repo.path(), r#"name = "playbook-ios""#);
    write_skill(&branch_repo.path().join("skills/review"), "Review");
    init_git_repo(branch_repo.path());
    rename_current_branch(branch_repo.path(), "main");

    add_dependency_in_dir_with_adapters(
        branch_project.path(),
        branch_cache.path(),
        &branch_repo.path().to_string_lossy(),
        None,
        &[Adapter::Claude],
        &[],
    )
    .unwrap();
    let (resolution, _) = resolve_project_from_existing_lockfile_in_dir(
        branch_project.path(),
        branch_cache.path(),
        &[Adapter::Claude],
    )
    .unwrap();
    assert_eq!(
        dependency_managed_package_id(&resolution),
        "playbook-ios+main"
    );

    let rev_project = TempDir::new().unwrap();
    let rev_cache = cache_dir();
    let rev_repo = TempDir::new().unwrap();
    write_manifest(rev_repo.path(), r#"name = "playbook-ios""#);
    write_skill(&rev_repo.path().join("skills/review"), "Review");
    init_git_repo(rev_repo.path());
    let revision = crate::git::current_rev(rev_repo.path()).unwrap();

    add_dependency_in_dir_with_git_ref(
        rev_project.path(),
        rev_cache.path(),
        &rev_repo.path().to_string_lossy(),
        RequestedGitRef::Revision(revision.as_str()),
        &[Adapter::Claude],
        &[],
    )
    .unwrap();
    let (resolution, _) = resolve_project_from_existing_lockfile_in_dir(
        rev_project.path(),
        rev_cache.path(),
        &[Adapter::Claude],
    )
    .unwrap();
    assert_eq!(
        dependency_managed_package_id(&resolution),
        format!("playbook-ios+{}", &revision[..4])
    );
}

#[test]
fn git_workspace_members_share_owner_suffix_and_namespaced_names() {
    // Mirrors a consumer that depends on a namespaced multi-package git repo:
    //   ena = { url = "...", branch = "main", members = ["core", "rust"] }
    // Every package from that one repo + commit must line up on a single,
    // stable suffix derived from the pinned branch, and members must carry the
    // workspace namespace so they read `ena-core` / `ena-rust` rather than bare
    // `core` / `rust` that could collide with unrelated packages.
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let repo = TempDir::new().unwrap();
    write_manifest(
        repo.path(),
        r#"
name = "ena"
version = "0.1.0"

[adapters]
enabled = ["claude", "codex"]

[workspace]
members = ["packages/core", "packages/rust"]
namespace = "ena"

[workspace.package.core]
path = "packages/core"
name = "Ena Core"

[workspace.package.rust]
path = "packages/rust"
name = "Ena Rust"
"#,
    );
    write_skill(&repo.path().join("packages/core/skills/plan"), "Plan");
    write_skill(&repo.path().join("packages/rust/skills/checks"), "Checks");
    init_git_repo(repo.path());
    rename_current_branch(repo.path(), "main");

    write_manifest(
        temp.path(),
        &format!(
            r#"
[adapters]
enabled = ["claude", "codex"]

[dependencies]
ena = {{ url = "{}", branch = "main", members = ["core", "rust"] }}
"#,
            toml_path_value(repo.path())
        ),
    );

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let identities = ManagedPackageIdentities::from_resolved_packages(resolution.packages.iter());
    let managed_id = |alias: &str| {
        let package = resolution
            .packages
            .iter()
            .find(|package| package.alias == alias)
            .unwrap_or_else(|| panic!("missing package `{alias}`"));
        identities.managed_package_id(package)
    };

    // The branch wins over the manifest version (0.1.0), and the members inherit
    // the owner's `+main` suffix instead of falling back to their own digests.
    assert_eq!(managed_id("ena"), "ena+main");
    assert_eq!(managed_id("ena_core"), "ena-core+main");
    assert_eq!(managed_id("ena_rust"), "ena-rust+main");
}

#[test]
fn resolve_workspace_root_includes_all_members() {
    let repo = create_workspace_dependency();
    let cache = cache_dir();

    let resolution = resolve_project(repo.path(), cache.path(), ResolveMode::Sync).unwrap();

    assert_eq!(resolution.packages.len(), 3);
    assert!(
        resolution
            .packages
            .iter()
            .any(|package| package.alias == "root")
    );
    assert!(
        resolution
            .packages
            .iter()
            .any(|package| package.alias == "axiom")
    );
    assert!(
        resolution
            .packages
            .iter()
            .any(|package| package.alias == "firebase")
    );
}

#[test]
fn add_dependency_leaves_multi_workspace_members_disabled_by_default() {
    let project = TempDir::new().unwrap();
    let cache = cache_dir();
    let repo = create_workspace_dependency();

    let summary = add_dependency_in_dir_with_adapters(
        project.path(),
        cache.path(),
        &repo.path().to_string_lossy(),
        Some("v0.2.0"),
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    assert_eq!(
        summary
            .dependency_members
            .iter()
            .map(|member| (member.id.as_str(), member.enabled))
            .collect::<Vec<_>>(),
        vec![("axiom", false), ("firebase", false)]
    );
    assert!(!summary.dependency_preview.contains("members = ["));

    let loaded = load_root_from_dir(project.path()).unwrap();
    let dependency = loaded
        .manifest
        .dependencies
        .get(&normalize_alias_from_url(&repo.path().to_string_lossy()).unwrap())
        .unwrap();
    assert!(dependency.members.is_none());
}

#[test]
fn add_dependency_auto_enables_single_workspace_member() {
    let project = TempDir::new().unwrap();
    let cache = cache_dir();
    let repo = TempDir::new().unwrap();
    write_single_workspace_dependency(repo.path());
    init_git_repo(repo.path());
    tag_repo(repo.path(), "v0.2.0");

    let summary = add_dependency_in_dir_with_adapters(
        project.path(),
        cache.path(),
        &repo.path().to_string_lossy(),
        Some("v0.2.0"),
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    assert_eq!(
        summary
            .dependency_members
            .iter()
            .map(|member| (member.id.as_str(), member.enabled))
            .collect::<Vec<_>>(),
        vec![("axiom", true)]
    );
    assert!(summary.dependency_preview.contains("members = [\"axiom\"]"));

    let loaded = load_root_from_dir(project.path()).unwrap();
    let dependency = loaded
        .manifest
        .dependencies
        .get(&normalize_alias_from_url(&repo.path().to_string_lossy()).unwrap())
        .unwrap();
    assert_eq!(
        dependency.members.as_deref(),
        Some(&["axiom".to_string()][..])
    );
}

#[test]
fn add_dependency_accepts_all_workspace_members_when_requested() {
    let project = TempDir::new().unwrap();
    let cache = cache_dir();
    let repo = create_workspace_dependency();

    let summary = add_dependency_in_dir_with_adapters_accept_all(
        project.path(),
        cache.path(),
        &repo.path().to_string_lossy(),
        Some("v0.2.0"),
        &Adapter::ALL,
        &[],
        true,
    )
    .unwrap();

    assert_eq!(
        summary
            .dependency_members
            .iter()
            .map(|member| (member.id.as_str(), member.enabled))
            .collect::<Vec<_>>(),
        vec![("axiom", true), ("firebase", true)]
    );
    assert!(
        summary
            .dependency_preview
            .contains("members = [\"axiom\", \"firebase\"]")
    );

    let loaded = load_root_from_dir(project.path()).unwrap();
    let dependency = loaded
        .manifest
        .dependencies
        .get(&normalize_alias_from_url(&repo.path().to_string_lossy()).unwrap())
        .unwrap();
    assert_eq!(
        dependency.members.as_deref(),
        Some(&["axiom".to_string(), "firebase".to_string()][..])
    );
}

#[test]
fn add_dependency_skips_invalid_workspace_members() {
    let project = TempDir::new().unwrap();
    let cache = cache_dir();
    let repo = TempDir::new().unwrap();
    write_workspace_dependency_with_invalid_member(repo.path());
    init_git_repo(repo.path());
    tag_repo(repo.path(), "v0.2.0");

    let summary = add_dependency_in_dir_with_adapters(
        project.path(),
        cache.path(),
        &repo.path().to_string_lossy(),
        Some("v0.2.0"),
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    assert_eq!(
        summary
            .dependency_members
            .iter()
            .map(|member| (member.id.as_str(), member.enabled))
            .collect::<Vec<_>>(),
        vec![("axiom", true), ("firebase", false)]
    );
    assert!(summary.dependency_preview.contains("members = [\"axiom\"]"));

    let loaded = load_root_from_dir(project.path()).unwrap();
    let dependency = loaded
        .manifest
        .dependencies
        .get(&normalize_alias_from_url(&repo.path().to_string_lossy()).unwrap())
        .unwrap();
    assert_eq!(
        dependency.members.as_deref(),
        Some(&["axiom".to_string()][..])
    );

    let resolution = resolve_project(project.path(), cache.path(), ResolveMode::Sync).unwrap();
    assert!(
        resolution
            .packages
            .iter()
            .any(|package| package.alias == "axiom")
    );
    assert!(
        !resolution
            .packages
            .iter()
            .any(|package| package.alias == "firebase")
    );
    assert!(
        resolution
            .warnings
            .iter()
            .any(|warning| warning.contains("ignoring workspace member `firebase`"))
    );
}

#[test]
fn add_dependency_leaves_multi_marketplace_plugins_disabled_by_default() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let wrapper = TempDir::new().unwrap();
    write_marketplace(
        wrapper.path(),
        r#"{
  "plugins": [
    {
      "name": "Axiom",
      "source": "./plugins/axiom"
    },
    {
      "name": "Firebase",
      "source": "./plugins/firebase"
    }
  ]
}"#,
    );
    write_skill(
        &wrapper.path().join("plugins/axiom/skills/review"),
        "Review",
    );
    write_skill(
        &wrapper.path().join("plugins/firebase/skills/checks"),
        "Checks",
    );
    init_git_repo(wrapper.path());
    rename_current_branch(wrapper.path(), "main");
    let wrapper_alias = normalize_alias_from_url(&wrapper.path().to_string_lossy()).unwrap();

    let summary = add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &wrapper.path().to_string_lossy(),
        None,
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    assert_eq!(
        summary
            .dependency_members
            .iter()
            .map(|member| (member.id.as_str(), member.enabled))
            .collect::<Vec<_>>(),
        vec![("axiom", false), ("firebase", false)]
    );
    assert!(!summary.dependency_preview.contains("members = ["));

    let manifest = load_root_from_dir(temp.path()).unwrap();
    let dependency = manifest.manifest.dependencies.get(&wrapper_alias).unwrap();
    assert!(dependency.members.is_none());

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let wrapper_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == wrapper_alias)
        .unwrap();
    assert!(wrapper_package.dependencies.is_empty());
    assert!(
        !lockfile
            .packages
            .iter()
            .any(|package| package.alias == "axiom" || package.alias == "firebase")
    );
}

#[test]
fn workspace_dependency_without_members_enables_no_member_packages() {
    let project = TempDir::new().unwrap();
    let cache = cache_dir();
    write_workspace_dependency(&project.path().join("vendor/wrapper"));
    write_manifest(
        project.path(),
        r#"
[dependencies.wrapper]
path = "vendor/wrapper"
"#,
    );

    let resolution = resolve_project(project.path(), cache.path(), ResolveMode::Sync).unwrap();

    assert_eq!(resolution.packages.len(), 2);
    assert!(
        resolution
            .packages
            .iter()
            .any(|package| package.alias == "wrapper")
    );
    assert!(
        !resolution
            .packages
            .iter()
            .any(|package| package.alias == "axiom")
    );
    assert!(
        !resolution
            .packages
            .iter()
            .any(|package| package.alias == "firebase")
    );

    let lockfile = resolution
        .to_lockfile(Adapters::from_slice(&Adapter::ALL), project.path())
        .unwrap();
    let wrapper = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "wrapper")
        .unwrap();
    assert!(wrapper.dependencies.is_empty());
}

#[test]
fn workspace_dependency_installs_only_selected_members() {
    let project = TempDir::new().unwrap();
    let cache = cache_dir();
    write_workspace_dependency(&project.path().join("vendor/wrapper"));
    write_manifest(
        project.path(),
        r#"
[dependencies.wrapper]
path = "vendor/wrapper"
members = ["firebase"]
"#,
    );

    let resolution = resolve_project(project.path(), cache.path(), ResolveMode::Sync).unwrap();

    assert_eq!(resolution.packages.len(), 3);
    assert!(
        resolution
            .packages
            .iter()
            .any(|package| package.alias == "wrapper")
    );
    assert!(
        !resolution
            .packages
            .iter()
            .any(|package| package.alias == "axiom")
    );
    assert!(
        resolution
            .packages
            .iter()
            .any(|package| package.alias == "firebase")
    );

    let lockfile = resolution
        .to_lockfile(Adapters::from_slice(&Adapter::ALL), project.path())
        .unwrap();
    let wrapper = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "wrapper")
        .unwrap();
    assert_eq!(wrapper.dependencies, vec!["firebase"]);
}

#[test]
fn sync_generates_claude_workspace_marketplace_files() {
    let repo = create_workspace_dependency();
    let cache = cache_dir();
    let expected_owner_name = repo
        .path()
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap()
        .to_string();

    sync_in_dir_with_adapters(repo.path(), cache.path(), false, false, &Adapter::ALL).unwrap();

    let claude: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(generated_claude_marketplace_path(repo.path())).unwrap(),
    )
    .unwrap();
    let expected_marketplace_name = "nodus";
    assert_eq!(claude["name"].as_str(), Some(expected_marketplace_name));
    assert_eq!(
        claude["owner"]["name"].as_str(),
        Some(expected_owner_name.as_str())
    );
    assert_eq!(claude["plugins"].as_array().unwrap().len(), 2);
    let workspace_plugin_source = claude["plugins"][0]["source"].as_str().unwrap();
    assert!(workspace_plugin_source.ends_with("/plugins/axiom"));
    assert!(
        !Path::new(workspace_plugin_source).is_absolute(),
        "Claude marketplace sources must be relative local paths: {workspace_plugin_source}"
    );

    assert!(generated_codex_marketplace_path(repo.path()).exists());
    let settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert!(
        settings["extraKnownMarketplaces"][expected_marketplace_name]["source"]["path"]
            .as_str()
            .is_some_and(|source| source.ends_with(".nodus-global"))
    );
    assert!(settings.get("enabledPlugins").is_none());

    let lockfile = Lockfile::read(&repo.path().join(LOCKFILE_NAME)).unwrap();
    assert_not_owned(
        &lockfile,
        repo.path(),
        ".nodus-global/.claude-plugin/marketplace.json",
    );
    assert_not_owned(
        &lockfile,
        repo.path(),
        ".nodus-global/.agents/plugins/marketplace.json",
    );
}

#[test]
fn sync_leaves_workspace_codex_user_config_untouched() {
    let repo = create_workspace_dependency();
    let cache = cache_dir();
    let codex_home = TempDir::new().unwrap();
    let codex_config = codex_home.path().join("config.toml");
    let original = r#"# codex user config
model = "gpt-5"
"#;
    write_file(&codex_config, original);

    let reporter = Reporter::silent();
    let install_paths =
        InstallPaths::project(repo.path()).with_codex_user_config(Some(codex_config.clone()));
    super::sync_in_dir_with_adapters_mode(
        &install_paths,
        cache.path(),
        SyncMode::Normal,
        false,
        false,
        &[Adapter::Codex],
        false,
        ExecutionMode::Apply,
        None,
        DependencyFailureMode::Graceful,
        false,
        None,
        &reporter,
    )
    .unwrap();

    let user_config: toml::Value =
        toml::from_str(&fs::read_to_string(&codex_config).unwrap()).unwrap();
    assert_eq!(user_config["model"].as_str(), Some("gpt-5"));
    assert_codex_config_registers_plugins(
        repo.path(),
        &user_config,
        &["Axiom@nodus", "Firebase@nodus"],
    );
    assert!(generated_codex_marketplace_path(repo.path()).exists());
}

#[test]
fn sync_creates_codex_user_config_for_workspace_plugins() {
    let repo = create_workspace_dependency();
    let cache = cache_dir();
    let codex_home = TempDir::new().unwrap();
    let codex_config = codex_home.path().join("config.toml");
    let install_paths =
        InstallPaths::project(repo.path()).with_codex_user_config(Some(codex_config.clone()));
    let reporter = Reporter::silent();

    super::sync_in_dir_with_adapters_mode(
        &install_paths,
        cache.path(),
        SyncMode::Normal,
        false,
        false,
        &[Adapter::Codex],
        false,
        ExecutionMode::Apply,
        None,
        DependencyFailureMode::Graceful,
        false,
        None,
        &reporter,
    )
    .unwrap();

    let buffer = SharedBuffer::default();
    let reporter = Reporter::sink(ColorMode::Never, buffer.clone());
    super::sync_in_dir_with_adapters_mode(
        &install_paths,
        cache.path(),
        SyncMode::Normal,
        false,
        false,
        &[Adapter::Codex],
        false,
        ExecutionMode::Apply,
        None,
        DependencyFailureMode::Graceful,
        false,
        None,
        &reporter,
    )
    .unwrap();

    let output = buffer.contents();
    assert!(codex_config.exists());
    assert!(
        !output.contains("failed"),
        "second sync should keep Codex user config idempotent, got {output}"
    );
}

#[test]
fn sync_emits_claude_native_plugin_layout_for_dependency_package() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
name = "Shared Tools"
version = "1.2.3"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/agents/security.md"),
        "# Security\n",
    );
    write_file(
        &temp.path().join("vendor/shared/commands/build.md"),
        "# Build\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Claude]).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), shared, Adapter::Claude);
    assert!(plugin_root.join("skills/review/SKILL.md").exists());
    assert!(plugin_root.join("agents/security.md").exists());
    assert!(plugin_root.join("commands/build.md").exists());

    let plugin: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(plugin_root.join(".claude-plugin/plugin.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(plugin["name"].as_str(), Some("shared-tools"));
    assert_eq!(plugin["version"].as_str(), Some("1.2.3"));
    assert_eq!(plugin["skills"].as_str(), Some("./skills/"));
    assert_eq!(plugin["agents"][0].as_str(), Some("./agents/security.md"));
    assert_eq!(
        plugin["commands"]["build"]["source"].as_str(),
        Some("./commands/build.md")
    );

    let settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(temp.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    let marketplace: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(generated_claude_marketplace_path(temp.path())).unwrap(),
    )
    .unwrap();
    assert_eq!(
        marketplace["plugins"][0]["name"].as_str(),
        Some("shared-tools")
    );
    let marketplace_source = marketplace["plugins"][0]["source"].as_str().unwrap();
    assert!(marketplace_source.ends_with("/claude-plugin"));
    assert!(
        marketplace_source.starts_with("./packages/"),
        "Claude global marketplace should point at shared payloads with a relative source: {marketplace_source}"
    );
    let plugin_key = format!(
        "shared-tools@{}",
        marketplace["name"].as_str().expect("marketplace name")
    );
    let marketplace_name = marketplace["name"].as_str().expect("marketplace name");
    assert_eq!(
        settings["extraKnownMarketplaces"][marketplace_name]["source"]["source"].as_str(),
        Some("directory")
    );
    assert!(
        settings["extraKnownMarketplaces"][marketplace_name]["source"]["path"]
            .as_str()
            .is_some_and(|source| source.ends_with(".nodus-global"))
    );
    assert_eq!(
        settings["enabledPlugins"]
            .get(&plugin_key)
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let plugin_root_relative = display_path(plugin_root.strip_prefix(temp.path()).unwrap());
    assert_not_owned(&lockfile, temp.path(), &plugin_root_relative);
}

#[test]
fn sync_preserves_claude_native_passthrough_components() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    let package_root = temp.path().join("vendor/shared");
    write_modern_claude_plugin_json(&package_root, "1.0.0");
    write_file(&package_root.join(".lsp.json"), "{ \"servers\": {} }\n");
    write_file(&package_root.join("monitors/status.json"), "{}\n");
    write_file(&package_root.join("bin/run.sh"), "#!/bin/sh\n");
    write_file(&package_root.join("settings.json"), "{}\n");
    write_file(
        &package_root.join("output-styles/default.md"),
        "# Default\n",
    );
    write_file(&package_root.join("themes/dark.json"), "{}\n");

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Claude]).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), shared, Adapter::Claude);
    for relative in [
        ".lsp.json",
        "monitors/status.json",
        "bin/run.sh",
        "settings.json",
        "output-styles/default.md",
        "themes/dark.json",
    ] {
        assert!(
            plugin_root.join(relative).exists(),
            "missing generated plugin file {relative}"
        );
    }
    let plugin: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(plugin_root.join(".claude-plugin/plugin.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(plugin["name"].as_str(), Some("shared"));
    assert_eq!(plugin["version"].as_str(), Some("1.0.0"));

    let marketplace: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(generated_claude_marketplace_path(temp.path())).unwrap(),
    )
    .unwrap();
    assert_eq!(marketplace["plugins"][0]["name"].as_str(), Some("shared"));
    let marketplace_source = marketplace["plugins"][0]["source"].as_str().unwrap();
    assert!(marketplace_source.ends_with("/claude-plugin"));
    assert!(
        marketplace_source.starts_with("./packages/"),
        "Claude global marketplace should point at shared payloads with a relative source: {marketplace_source}"
    );
}

#[test]
fn sync_emits_claude_marketplace_inline_lsp_metadata() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
clangd_lsp = { path = "vendor/market/plugins/clangd-lsp" }
"#,
    );
    write_file(
        &temp
            .path()
            .join("vendor/market/.claude-plugin/marketplace.json"),
        r#"{
  "plugins": [
    {
      "name": "clangd-lsp",
      "source": "./plugins/clangd-lsp",
      "lspServers": {
        "clangd": {
          "command": "clangd",
          "args": ["--background-index"]
        }
      }
    }
  ]
}
"#,
    );
    write_file(
        &temp
            .path()
            .join("vendor/market/plugins/clangd-lsp/README.md"),
        "# clangd\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Claude]).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let package = resolution
        .packages
        .iter()
        .find(|package| package.alias == "clangd_lsp")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), package, Adapter::Claude);
    let plugin: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(plugin_root.join(".claude-plugin/plugin.json")).unwrap(),
    )
    .unwrap();

    assert_eq!(
        plugin["lspServers"]["clangd"]["command"].as_str(),
        Some("clangd")
    );
    assert_eq!(
        plugin["lspServers"]["clangd"]["args"][0].as_str(),
        Some("--background-index")
    );
}

#[test]
fn sync_emits_codex_native_plugin_layout_for_dependency_package() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
name = "Shared Tools"

[mcp_servers.figma]
command = "npx"
args = ["figma-developer-mcp"]
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_codex_agent_toml(
        &temp.path().join("vendor/shared/agents/security.toml"),
        "security",
        "Security reviewer",
        "Audit the code.",
    );
    write_file(
        &temp.path().join("vendor/shared/commands/build.md"),
        "# Build\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), shared, Adapter::Codex);
    assert!(plugin_root.join("skills/review/SKILL.md").exists());
    assert!(temp.path().join(".codex/agents/security.toml").exists());
    assert!(plugin_root.join("skills/__cmd_build/SKILL.md").exists());
    assert!(
        !temp
            .path()
            .join(".codex/skills/__cmd_build/SKILL.md")
            .exists(),
        "dependency commands should be emitted only inside the Codex plugin snapshot"
    );

    assert!(plugin_root.join(".codex-plugin/plugin.json").exists());
    assert!(plugin_root.join(".mcp.json").exists());
    assert!(generated_codex_marketplace_path(temp.path()).exists());
    assert!(!temp.path().join(".codex/config.toml").exists());
    assert_codex_user_config_registers_plugins(temp.path(), &["shared-tools@nodus"]);

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert_owned(&lockfile, temp.path(), &display_path(&plugin_root));
}

#[test]
fn sync_writes_codex_profile_overlay_and_keeps_base_config_clean() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["codex"]

[adapters.codex]
profile = "work"

[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
name = "Shared Tools"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    // The nodus marketplace + plugin enablement is registered in the profile
    // overlay, not the base config the active profile inherits from.
    let overlay = read_codex_user_overlay(temp.path(), "work");
    assert_codex_config_registers_plugins(temp.path(), &overlay, &["shared-tools@nodus"]);
    assert_codex_user_base_has_no_managed_marketplace(temp.path());

    // The lockfile records the profile so a later change forces a re-render.
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert_eq!(lockfile.codex_profile.as_deref(), Some("work"));
}

#[test]
fn sync_moves_codex_registration_back_to_base_when_profile_removed() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let with_profile = r#"
[adapters]
enabled = ["codex"]

[adapters.codex]
profile = "work"

[dependencies]
shared = { path = "vendor/shared" }
"#;
    write_manifest(temp.path(), with_profile);
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
name = "Shared Tools"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();
    assert!(generated_codex_user_overlay_path(temp.path(), "work").exists());

    // Drop the profile and re-sync: the registration returns to the base config
    // and the abandoned overlay is cleaned of the nodus marketplace.
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["codex"]

[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    assert_codex_user_config_registers_plugins(temp.path(), &["shared-tools@nodus"]);
    let overlay_path = generated_codex_user_overlay_path(temp.path(), "work");
    if overlay_path.exists() {
        let overlay: toml::Value =
            toml::from_str(&fs::read_to_string(&overlay_path).unwrap()).unwrap();
        if let Some(marketplaces) = overlay.get("marketplaces").and_then(toml::Value::as_table) {
            assert!(
                !marketplaces.contains_key("nodus"),
                "abandoned overlay should not keep the nodus marketplace; was {overlay:?}"
            );
        }
    }

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert_eq!(lockfile.codex_profile, None);
}

#[test]
fn resolve_codex_profile_validates_and_prefers_override() {
    let manifest = crate::manifest::Manifest::default();
    // Override wins and is returned verbatim once validated.
    assert_eq!(
        super::resolve_codex_profile(&manifest, Some("work"))
            .unwrap()
            .as_deref(),
        Some("work")
    );
    // No override and no manifest profile means no profile.
    assert_eq!(super::resolve_codex_profile(&manifest, None).unwrap(), None);
    // Names that could escape `$CODEX_HOME` are rejected.
    for unsafe_name in ["../evil", "a/b", "a\\b", "..", "."] {
        assert!(
            super::resolve_codex_profile(&manifest, Some(unsafe_name)).is_err(),
            "expected `{unsafe_name}` to be rejected"
        );
    }
}

#[test]
fn sync_rejects_unsafe_codex_profile_name() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["codex"]

[adapters.codex]
profile = "../evil"

[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_manifest(&temp.path().join("vendor/shared"), "name = \"Shared\"\n");
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    let error =
        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex])
            .unwrap_err()
            .to_string();
    assert!(
        error.contains("invalid Codex profile"),
        "expected profile validation error, got: {error}"
    );
}

#[test]
fn sync_prunes_legacy_codex_marketplace_tree() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    // Simulate a store written by a pre-re-root Nodus: the Codex marketplace and
    // its plugin snapshots lived under `<home>/marketplaces/codex` instead of
    // sharing the Nodus home root. The manifest there was never recorded as
    // package-owned, so only the migration cleanup can prune it.
    let legacy_root = temp.path().join(".nodus-global/marketplaces/codex");
    write_file(
        &legacy_root.join(".agents/plugins/marketplace.json"),
        "{\n  \"name\": \"nodus\",\n  \"plugins\": []\n}\n",
    );
    write_file(
        &legacy_root.join("plugins/shared+legacy/.codex-plugin/plugin.json"),
        "{\n  \"name\": \"shared\"\n}\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    // The re-rooted marketplace is written at the home root, and the entire
    // legacy `marketplaces/codex` tree (including its empty parent) is gone.
    assert!(generated_codex_marketplace_path(temp.path()).exists());
    assert!(!temp.path().join(".nodus-global/marketplaces").exists());
}

#[test]
fn sync_uses_workspace_namespace_for_member_alias_and_plugin_name() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
bundle = { path = "vendor/bundle", members = ["core"] }
"#,
    );
    write_namespaced_workspace_dependency(&temp.path().join("vendor/bundle"));

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let member = resolution
        .packages
        .iter()
        .find(|package| package.alias == "ena_core")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), member, Adapter::Codex);
    assert!(plugin_root.join("skills/plan/SKILL.md").exists());
    assert!(
        !temp
            .path()
            .join(".nodus/packages/core/codex-plugin")
            .exists()
    );
    assert!(plugin_root.join(".codex-plugin/plugin.json").exists());
    assert!(generated_codex_marketplace_path(temp.path()).exists());

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert!(
        lockfile
            .packages
            .iter()
            .any(|package| package.alias == "bundle")
    );
    assert!(
        lockfile
            .packages
            .iter()
            .any(|package| package.alias == "ena_core")
    );
    assert!(
        !lockfile
            .packages
            .iter()
            .any(|package| package.alias == "core")
    );
    let bundle = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "bundle")
        .unwrap();
    assert_eq!(bundle.dependencies, vec!["ena_core".to_string()]);
}

#[test]
fn sync_emits_dependency_codex_hooks_in_project_runtime() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["codex"]

[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
[[hooks]]
id = "format-after-write"
event = "post_tool_use"
adapters = ["codex"]

[hooks.matcher]
tool_names = ["bash"]

[hooks.handler]
type = "command"
command = "./scripts/format.sh"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/scripts/format.sh"),
        "#!/bin/sh\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), shared, Adapter::Codex);
    let hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(plugin_root.join("hooks/hooks.json")).unwrap())
            .unwrap();
    assert_eq!(
        hooks["hooks"]["PostToolUse"][0]["matcher"].as_str(),
        Some("Bash")
    );
    let command = hooks["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(
        command.contains("${PLUGIN_ROOT}/hooks/scripts/nodus-hook-format-after-write-"),
        "expected plugin Codex hook command, got `{command}`",
    );
    assert!(
        fs::read_dir(plugin_root.join("hooks/scripts"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("nodus-hook-format-after-write-"))
    );
    assert!(!temp.path().join(".codex/hooks.json").exists());
    assert!(plugin_root.join(".codex-plugin/plugin.json").exists());

    let codex_config: toml::Value =
        toml::from_str(&fs::read_to_string(temp.path().join(".codex/config.toml")).unwrap())
            .unwrap();
    assert_eq!(
        codex_config
            .get("features")
            .and_then(toml::Value::as_table)
            .and_then(|features| features.get("hooks")),
        None
    );
    assert_eq!(
        codex_config["features"]["plugin_hooks"].as_bool(),
        Some(true)
    );

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert_owned(&lockfile, temp.path(), &display_path(&plugin_root));
}

#[test]
fn sync_leaves_codex_user_config_untouched_and_prunes_project_marketplace() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let codex_home = TempDir::new().unwrap();
    let codex_config = codex_home.path().join("config.toml");
    write_manifest(
        temp.path(),
        r#"
name = "Yoki iOS"

[adapters]
enabled = ["codex"]

[dependencies]
grapha = { path = "vendor/grapha" }
playbook_ios = { path = "vendor/playbook-ios" }
"#,
    );
    write_manifest(
        &temp.path().join("vendor/grapha"),
        r#"
name = "Grapha"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/playbook-ios"),
        r#"
name = "Playbook iOS"
"#,
    );
    write_skill(
        &temp.path().join("vendor/grapha/skills/grapha-search"),
        "Grapha Search",
    );
    write_skill(
        &temp.path().join("vendor/playbook-ios/skills/ios-testing"),
        "iOS Testing",
    );
    let original = r#"# codex user config
model = "gpt-5"

[plugins."manual@other"]
enabled = false

[plugins."grapha@yoki-ios"]
enabled = false

[marketplaces.yoki-ios]
source_type = "git"
source = "https://github.com/example/old.git"
ref = "main"
sparse_paths = ["plugins"]
custom = "kept"
"#;
    write_file(&codex_config, original);
    write_file(
        &temp.path().join(".codex/config.toml"),
        &original.replace("codex user config", "codex project config"),
    );

    let reporter = Reporter::silent();
    let install_paths =
        InstallPaths::project(temp.path()).with_codex_user_config(Some(codex_config.clone()));
    super::sync_in_dir_with_adapters_mode(
        &install_paths,
        cache.path(),
        SyncMode::Normal,
        false,
        false,
        &[Adapter::Codex],
        false,
        ExecutionMode::Apply,
        None,
        DependencyFailureMode::Graceful,
        false,
        None,
        &reporter,
    )
    .unwrap();

    let user_config: toml::Value =
        toml::from_str(&fs::read_to_string(&codex_config).unwrap()).unwrap();
    assert_eq!(user_config["model"].as_str(), Some("gpt-5"));
    assert_eq!(
        user_config["plugins"]["manual@other"]["enabled"].as_bool(),
        Some(false)
    );
    assert!(
        user_config
            .get("marketplaces")
            .and_then(toml::Value::as_table)
            .is_none_or(|marketplaces| !marketplaces.contains_key("yoki-ios"))
    );
    assert_codex_config_registers_plugins(
        temp.path(),
        &user_config,
        &["grapha@nodus", "playbook-ios@nodus"],
    );
    assert!(generated_codex_marketplace_path(temp.path()).exists());
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    for alias in ["grapha", "playbook_ios"] {
        let package = resolution
            .packages
            .iter()
            .find(|package| package.alias == alias)
            .unwrap();
        assert!(global_native_plugin_root(temp.path(), package, Adapter::Codex).exists());
    }
    assert!(
        !temp
            .path()
            .join(".nodus/packages/grapha/codex-plugin")
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(".nodus/packages/playbook_ios/codex-plugin")
            .exists()
    );
    let project_config = assert_no_codex_managed_marketplace_config(temp.path(), "yoki-ios");
    assert_eq!(project_config["model"].as_str(), Some("gpt-5"));
    assert_eq!(
        project_config["plugins"]["manual@other"]["enabled"].as_bool(),
        Some(false)
    );
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let codex_config_relative = codex_config
        .strip_prefix(temp.path())
        .map(display_path)
        .unwrap_or_else(|_| display_path(&codex_config));
    assert!(
        !lockfile.packages.iter().any(|package| {
            package
                .owned_files
                .iter()
                .any(|path| path == &codex_config_relative)
                || package
                    .owned_subtrees
                    .iter()
                    .any(|path| path == &codex_config_relative)
        }),
        "codex user config {codex_config_relative} should not appear in any package's ownership view",
    );
    assert_not_owned(&lockfile, temp.path(), ".agents/plugins/marketplace.json");
    assert_owned(&lockfile, temp.path(), ".codex/config.toml");
}

#[test]
fn sync_migrates_claude_and_codex_outputs_to_native_plugins() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["claude", "codex"]

[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();
    simulate_legacy_direct_claude_codex_skill_outputs(temp.path());

    assert!(temp.path().join(".claude/skills/review/SKILL.md").exists());
    assert!(temp.path().join(".codex/skills/review/SKILL.md").exists());

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    assert!(!temp.path().join(".claude/skills/review/SKILL.md").exists());
    assert!(!temp.path().join(".codex/skills/review/SKILL.md").exists());
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let claude_plugin_root = global_native_plugin_root(temp.path(), shared, Adapter::Claude);
    let codex_plugin_root = global_native_plugin_root(temp.path(), shared, Adapter::Codex);
    assert!(claude_plugin_root.join("skills/review/SKILL.md").exists());
    assert!(codex_plugin_root.join("skills/review/SKILL.md").exists());
    assert!(generated_claude_marketplace_path(temp.path()).exists());
    assert!(generated_codex_marketplace_path(temp.path()).exists());

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let claude_plugin_root_relative =
        display_path(claude_plugin_root.strip_prefix(temp.path()).unwrap());
    assert_not_owned(&lockfile, temp.path(), &claude_plugin_root_relative);
    assert_owned(&lockfile, temp.path(), &display_path(&codex_plugin_root));
    assert_not_owned(&lockfile, temp.path(), ".claude/skills/review");
    assert_not_owned(&lockfile, temp.path(), ".codex/skills/review");
}

#[test]
fn sync_locked_and_frozen_reject_native_plugin_migration() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["claude", "codex"]

[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();
    simulate_legacy_direct_claude_codex_skill_outputs(temp.path());
    let legacy_lockfile = fs::read(temp.path().join(LOCKFILE_NAME)).unwrap();

    let locked_error = sync_in_dir(temp.path(), cache.path(), true, false)
        .unwrap_err()
        .to_string();
    assert!(locked_error.contains("nodus.lock is out of date"));
    assert_eq!(
        fs::read(temp.path().join(LOCKFILE_NAME)).unwrap(),
        legacy_lockfile
    );
    assert!(temp.path().join(".codex/skills/review/SKILL.md").exists());
    assert!(
        !temp
            .path()
            .join(".nodus/packages/shared/codex-plugin")
            .exists()
    );

    let frozen_error = sync_in_dir_frozen(temp.path(), cache.path(), false)
        .unwrap_err()
        .to_string();
    assert!(frozen_error.contains("nodus.lock is out of date"));
    assert_eq!(
        fs::read(temp.path().join(LOCKFILE_NAME)).unwrap(),
        legacy_lockfile
    );
    assert!(temp.path().join(".claude/skills/review/SKILL.md").exists());
    assert!(
        !temp
            .path()
            .join(".nodus/packages/shared/claude-plugin")
            .exists()
    );
}

#[test]
fn sync_leaves_unmanaged_files_in_legacy_project_native_plugin_target() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["claude", "codex"]

[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();
    simulate_legacy_direct_claude_codex_skill_outputs(temp.path());
    write_file(
        &temp
            .path()
            .join(".nodus/packages/shared/codex-plugin/skills"),
        "user-owned blocking file\n",
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    assert!(
        global_native_plugin_root(temp.path(), shared, Adapter::Codex)
            .join("skills/review/SKILL.md")
            .exists()
    );
    assert_eq!(
        fs::read_to_string(
            temp.path()
                .join(".nodus/packages/shared/codex-plugin/skills")
        )
        .unwrap(),
        "user-owned blocking file\n"
    );
}

#[test]
fn sync_skips_invalid_workspace_members_in_claude_marketplace_files() {
    let repo = TempDir::new().unwrap();
    let cache = cache_dir();
    write_workspace_dependency_with_invalid_member(repo.path());
    let expected_owner_name = repo
        .path()
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap()
        .to_string();

    sync_in_dir_with_adapters(repo.path(), cache.path(), false, false, &Adapter::ALL).unwrap();

    let claude: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(generated_claude_marketplace_path(repo.path())).unwrap(),
    )
    .unwrap();
    assert_eq!(claude["name"].as_str(), Some("nodus"));
    assert_eq!(
        claude["owner"]["name"].as_str(),
        Some(expected_owner_name.as_str())
    );
    assert_eq!(claude["plugins"].as_array().unwrap().len(), 1);
    assert!(
        claude["plugins"][0]["source"]
            .as_str()
            .is_some_and(|source| source.ends_with("/plugins/axiom"))
    );

    assert!(generated_codex_marketplace_path(repo.path()).exists());
}

#[test]
fn sync_writes_codex_marketplace_for_workspace_members_with_codex_metadata() {
    let repo = TempDir::new().unwrap();
    let cache = cache_dir();
    write_workspace_dependency_with_non_codex_member(repo.path());
    sync_in_dir_with_adapters(repo.path(), cache.path(), false, false, &Adapter::ALL).unwrap();

    let claude: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(generated_claude_marketplace_path(repo.path())).unwrap(),
    )
    .unwrap();
    assert_eq!(claude["name"].as_str(), Some("nodus"));
    assert_eq!(claude["plugins"].as_array().unwrap().len(), 2);

    let codex: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(generated_codex_marketplace_path(repo.path())).unwrap(),
    )
    .unwrap();
    assert_eq!(codex["name"].as_str(), Some("nodus"));
    assert_eq!(codex["plugins"].as_array().unwrap().len(), 1);
}

#[test]
fn sync_uses_root_manifest_name_for_claude_workspace_marketplace_metadata() {
    let repo = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        repo.path(),
        r#"
name = "Workspace Plugins"

[workspace]
members = ["plugins/axiom"]

[workspace.package.axiom]
path = "plugins/axiom"
name = "Axiom"

[workspace.package.axiom.codex]
category = "Productivity"
installation = "AVAILABLE"
authentication = "ON_INSTALL"
"#,
    );
    write_skill(&repo.path().join("plugins/axiom/skills/review"), "Review");

    sync_in_dir_with_adapters(repo.path(), cache.path(), false, false, &Adapter::ALL).unwrap();

    let claude: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(generated_claude_marketplace_path(repo.path())).unwrap(),
    )
    .unwrap();
    assert_eq!(claude["name"].as_str(), Some("nodus"));
    assert_eq!(claude["owner"]["name"].as_str(), Some("Workspace Plugins"));
    assert_eq!(claude["plugins"].as_array().unwrap().len(), 1);
    assert_eq!(claude["plugins"][0]["name"].as_str(), Some("Axiom"));
    assert!(
        claude["plugins"][0]["source"]
            .as_str()
            .is_some_and(|source| source.ends_with("/plugins/axiom"))
    );
}

#[test]
fn sync_uses_workspace_namespace_for_workspace_marketplace_member_names() {
    let repo = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        repo.path(),
        r#"
name = "Ena"

[workspace]
members = ["plugins/core"]
namespace = "ena"

[workspace.package.core]
path = "plugins/core"
name = "Core"

[workspace.package.core.codex]
category = "Productivity"
installation = "AVAILABLE"
authentication = "ON_INSTALL"
"#,
    );
    write_skill(&repo.path().join("plugins/core/skills/review"), "Review");

    sync_in_dir_with_adapters(repo.path(), cache.path(), false, false, &Adapter::ALL).unwrap();

    let claude: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(generated_claude_marketplace_path(repo.path())).unwrap(),
    )
    .unwrap();
    assert_eq!(claude["plugins"][0]["name"].as_str(), Some("ena-core"));
    assert!(
        claude["plugins"][0]["source"]
            .as_str()
            .is_some_and(|source| source.ends_with("/plugins/core"))
    );

    assert!(generated_codex_marketplace_path(repo.path()).exists());
}

#[test]
fn add_dependency_uses_default_branch_when_repo_has_no_tags() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let repo = TempDir::new().unwrap();
    write_skill(&repo.path().join("skills/review"), "Review");
    init_git_repo(repo.path());
    rename_current_branch(repo.path(), "main");

    add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &repo.path().to_string_lossy(),
        None,
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
    assert!(manifest.contains("branch = \"main\""));
}

#[test]
fn add_dependency_tracks_an_explicit_branch() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let repo = TempDir::new().unwrap();
    write_skill(&repo.path().join("skills/review"), "Review");
    init_git_repo(repo.path());
    rename_current_branch(repo.path(), "main");
    tag_repo(repo.path(), "v0.1.0");

    add_dependency_in_dir_with_git_ref(
        temp.path(),
        cache.path(),
        &repo.path().to_string_lossy(),
        RequestedGitRef::Branch("main"),
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
    assert!(manifest.contains("branch = \"main\""));
    assert!(!manifest.contains("tag = "));
}

#[test]
fn add_dependency_pins_an_explicit_revision() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let repo = TempDir::new().unwrap();
    write_skill(&repo.path().join("skills/review"), "Review");
    init_git_repo(repo.path());
    tag_repo(repo.path(), "v0.1.0");
    let revision = crate::git::current_rev(repo.path()).unwrap();

    add_dependency_in_dir_with_git_ref(
        temp.path(),
        cache.path(),
        &repo.path().to_string_lossy(),
        RequestedGitRef::Revision(revision.as_str()),
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
    assert!(manifest.contains(&format!("revision = \"{revision}\"")));
    assert!(!manifest.contains("tag = "));
    assert!(!manifest.contains("branch = "));
}

#[test]
fn add_dependency_rejects_repo_without_supported_directories() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let repo = TempDir::new().unwrap();
    write_file(&repo.path().join("README.md"), "hello\n");
    init_git_repo(repo.path());
    tag_repo(repo.path(), "v0.1.0");

    let error = add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &repo.path().to_string_lossy(),
        Some("v0.1.0"),
        &Adapter::ALL,
        &[],
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("does not match the Nodus package layout"));
}

#[test]
fn add_dependency_accepts_repo_with_symlinked_submodule_skills() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let shared = TempDir::new().unwrap();
    write_skill(&shared.path().join("skills/review"), "Review");
    init_git_repo(shared.path());
    rename_current_branch(shared.path(), "main");

    let repo = TempDir::new().unwrap();
    init_git_repo(repo.path());
    run_git(
        repo.path(),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &shared.path().to_string_lossy(),
            "vendor/shared",
        ],
    );
    run_git(repo.path(), &["add", "."]);
    stage_git_symlink(
        repo.path(),
        Path::new("skills/review"),
        "../vendor/shared/skills/review",
    );
    run_git(repo.path(), &["commit", "-m", "add shared skill"]);
    rename_current_branch(repo.path(), "main");

    add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &repo.path().to_string_lossy(),
        None,
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    let alias = normalize_alias_from_url(&repo.path().to_string_lossy()).unwrap();
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == alias)
        .unwrap();
    assert_eq!(package.skills, vec!["review"]);
}

#[test]
fn sync_uses_snapshot_roots_when_skill_directory_is_placeholder_file() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/package" }
"#,
    );
    let package_root = temp.path().join("vendor/package");
    write_skill(&package_root.join("vendor/shared/skills/review"), "Review");
    write_file(
        &package_root.join("skills/review"),
        "../vendor/shared/skills/review\n",
    );

    sync_all(temp.path(), cache.path());

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    assert_eq!(package.skills, vec!["review"]);

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    assert!(
        global_native_plugin_root(temp.path(), shared, Adapter::Claude)
            .join("skills/review/SKILL.md")
            .exists()
    );
}

#[test]
fn add_dependency_accepts_repo_with_nested_skill_directories() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let repo = TempDir::new().unwrap();
    write_file(
        &repo.path().join("skills/operations-and-lifecycle/.gitkeep"),
        "",
    );
    write_skill(
        &repo
            .path()
            .join("skills/onboarding-and-migrations/molt-fetch"),
        "Molt Fetch",
    );
    write_skill(
        &repo
            .path()
            .join("skills/security-and-governance/configuring-audit-logging"),
        "Audit Logging",
    );
    init_git_repo(repo.path());
    rename_current_branch(repo.path(), "main");

    add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &repo.path().to_string_lossy(),
        None,
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    let alias = normalize_alias_from_url(&repo.path().to_string_lossy()).unwrap();
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == alias)
        .unwrap();
    assert_eq!(
        package.skills,
        vec![
            "onboarding-and-migrations__molt-fetch",
            "security-and-governance__configuring-audit-logging",
        ]
    );

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let package = resolution
        .packages
        .iter()
        .find(|package| package.alias == alias)
        .unwrap();
    let molt_fetch_skill_id = namespaced_skill_id(package, "onboarding-and-migrations__molt-fetch");
    let audit_logging_skill_id = namespaced_skill_id(
        package,
        "security-and-governance__configuring-audit-logging",
    );

    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &molt_fetch_skill_id
    ));
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &audit_logging_skill_id
    ));
}

#[test]
fn add_dependency_accepts_manifest_only_wrapper_repo_and_syncs_transitive_git_plugins() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let leaf = TempDir::new().unwrap();
    write_skill(&leaf.path().join("skills/checks"), "Checks");
    init_git_repo(leaf.path());
    tag_repo(leaf.path(), "v0.1.0");

    let wrapper = TempDir::new().unwrap();
    write_file(
        &wrapper.path().join(MANIFEST_FILE),
        &format!(
            r#"
[dependencies]
leaf = {{ url = "{}", tag = "v0.1.0" }}
"#,
            toml_path_value(leaf.path())
        ),
    );
    init_git_repo(wrapper.path());
    tag_repo(wrapper.path(), "v0.2.0");
    let wrapper_alias = normalize_alias_from_url(&wrapper.path().to_string_lossy()).unwrap();

    add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &wrapper.path().to_string_lossy(),
        Some("v0.2.0"),
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    let manifest = load_root_from_dir(temp.path()).unwrap();
    assert_eq!(manifest.manifest.dependencies.len(), 1);
    assert!(manifest.manifest.dependencies.contains_key(&wrapper_alias));

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert_eq!(lockfile.packages.len(), 3);
    assert!(
        lockfile
            .packages
            .iter()
            .any(|package| package.alias == "root")
    );
    let wrapper_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == wrapper_alias)
        .unwrap();
    assert!(wrapper_package.skills.is_empty());
    assert_eq!(wrapper_package.dependencies, vec!["leaf"]);
    let leaf_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "leaf")
        .unwrap();
    assert_eq!(leaf_package.skills, vec!["checks"]);

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let leaf_package = resolution
        .packages
        .iter()
        .find(|package| package.alias == "leaf")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(leaf_package, "checks");
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
}

#[test]
fn add_dependency_accepts_claude_marketplace_wrapper_and_syncs_plugin_contents() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let wrapper = TempDir::new().unwrap();
    write_marketplace(
        wrapper.path(),
        r#"{
  "plugins": [
    {
      "name": "Axiom",
      "version": "2.34.0",
      "source": "./.claude-plugin/plugins/axiom"
    }
  ]
}"#,
    );
    write_skill(
        &wrapper
            .path()
            .join(".claude-plugin/plugins/axiom/skills/review"),
        "Review",
    );
    write_file(
        &wrapper
            .path()
            .join(".claude-plugin/plugins/axiom/agents/security.md"),
        "# Security\n",
    );
    write_file(
        &wrapper
            .path()
            .join(".claude-plugin/plugins/axiom/commands/build.md"),
        "# Build\n",
    );
    write_claude_plugin_json(
        &wrapper.path().join(".claude-plugin/plugins/axiom"),
        "2.34.0",
    );
    init_git_repo(wrapper.path());
    tag_repo(wrapper.path(), "v0.4.0");
    let wrapper_alias = normalize_alias_from_url(&wrapper.path().to_string_lossy()).unwrap();

    add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &wrapper.path().to_string_lossy(),
        Some("v0.4.0"),
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let wrapper_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == wrapper_alias)
        .unwrap();
    assert_eq!(wrapper_package.version_tag.as_deref(), Some("2.34.0"));
    assert!(wrapper_package.skills.is_empty());
    assert_eq!(wrapper_package.dependencies, vec!["axiom"]);

    let plugin_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "axiom")
        .unwrap();
    assert_eq!(plugin_package.version_tag.as_deref(), Some("2.34.0"));
    assert_eq!(
        plugin_package.source.path.as_deref(),
        Some("./.claude-plugin/plugins/axiom")
    );
    assert_eq!(plugin_package.skills, vec!["review"]);
    assert_eq!(plugin_package.agents, vec!["security"]);
    assert_eq!(plugin_package.commands, vec!["build"]);

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let plugin_package = resolution
        .packages
        .iter()
        .find(|package| package.alias == "axiom")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(plugin_package, "review");
    let managed_agent_file = namespaced_file_name(plugin_package, "security", "md");
    let managed_command_file = namespaced_file_name(plugin_package, "build", "md");
    assert!(
        temp.path()
            .join(format!(".agents/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
    assert!(runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_agent_file
    ));
    assert!(runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_command_file
    ));
}

#[test]
fn add_dependency_accepts_marketplace_plugin_that_points_at_root_claude_plugin_metadata() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let wrapper = TempDir::new().unwrap();
    write_marketplace(
        wrapper.path(),
        r#"{
  "plugins": [
    {
      "name": "atlan",
      "version": "1.0.0",
      "source": "./"
    }
  ]
}"#,
    );
    write_modern_claude_plugin_json(wrapper.path(), "1.0.0");
    write_file(
        &wrapper.path().join(".mcp.json"),
        r#"{
  "mcpServers": {
    "atlan": {
      "type": "http",
      "url": "https://mcp.atlan.com/mcp"
    }
  }
}
"#,
    );
    init_git_repo(wrapper.path());
    rename_current_branch(wrapper.path(), "main");

    add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &wrapper.path().to_string_lossy(),
        None,
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    let wrapper_alias = normalize_alias_from_url(&wrapper.path().to_string_lossy()).unwrap();
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let wrapper_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == wrapper_alias)
        .unwrap();
    assert!(wrapper_package.dependencies.is_empty());
    assert_eq!(wrapper_package.mcp_servers, vec!["atlan"]);

    assert!(
        !temp.path().join(".mcp.json").exists(),
        "Claude native plugin MCP should replace dependency project-level MCP output"
    );
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let resolved_wrapper = resolution
        .packages
        .iter()
        .find(|package| package.alias == wrapper_alias)
        .unwrap();
    let plugin_mcp_path =
        global_native_plugin_root(temp.path(), resolved_wrapper, Adapter::Claude).join(".mcp.json");
    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(plugin_mcp_path).unwrap()).unwrap();
    assert_eq!(
        json["mcpServers"]["atlan"]["url"].as_str(),
        Some("https://mcp.atlan.com/mcp")
    );
    assert_eq!(json["mcpServers"]["atlan"]["type"].as_str(), Some("http"));
    let plugin: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(
            global_native_plugin_root(temp.path(), resolved_wrapper, Adapter::Claude)
                .join(".claude-plugin/plugin.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(plugin["mcpServers"].as_str(), Some("./.mcp.json"));
}

#[test]
fn add_dependency_accepts_modern_claude_plugin_extra_component_paths_and_syncs_contents() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let plugin = TempDir::new().unwrap();
    write_modern_claude_plugin_json_with_fields(
        plugin.path(),
        &[
            r#"  "version": "1.0.0""#,
            r#"  "skills": ["./plugin-skills"]"#,
            r#"  "agents": "./security.md""#,
            r#"  "commands": ["./build.md"]"#,
        ],
    );
    write_skill(&plugin.path().join("plugin-skills/review"), "Review");
    write_file(&plugin.path().join("security.md"), "# Security\n");
    write_file(&plugin.path().join("build.md"), "# Build\n");
    init_git_repo(plugin.path());
    tag_repo(plugin.path(), "v0.4.0");

    add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &plugin.path().to_string_lossy(),
        Some("v0.4.0"),
        &[Adapter::Claude],
        &[],
    )
    .unwrap();

    let alias = normalize_alias_from_url(&plugin.path().to_string_lossy()).unwrap();
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == alias)
        .unwrap();
    assert_eq!(package.version_tag.as_deref(), Some("1.0.0"));
    assert_eq!(package.skills, vec!["review"]);
    assert_eq!(package.agents, vec!["security"]);
    assert_eq!(package.commands, vec!["build"]);

    let package = resolve_project(temp.path(), cache.path(), ResolveMode::Sync)
        .unwrap()
        .packages
        .into_iter()
        .find(|package| package.alias == alias)
        .unwrap();
    let managed_skill_id = namespaced_skill_id(&package, "review");
    let managed_agent_file = namespaced_file_name(&package, "security", "md");
    let managed_command_file = namespaced_file_name(&package, "build", "md");
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
    assert!(runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_agent_file
    ));
    assert!(runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_command_file
    ));
}

#[test]
fn add_dependency_writes_marketplace_version_alongside_default_branch() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let wrapper = TempDir::new().unwrap();
    write_marketplace(
        wrapper.path(),
        r#"{
  "plugins": [
    {
      "name": "Axiom",
      "version": "2.34.0",
      "source": "./.claude-plugin/plugins/axiom"
    }
  ]
}"#,
    );
    write_skill(
        &wrapper
            .path()
            .join(".claude-plugin/plugins/axiom/skills/review"),
        "Review",
    );
    write_claude_plugin_json(
        &wrapper.path().join(".claude-plugin/plugins/axiom"),
        "2.34.0",
    );
    init_git_repo(wrapper.path());
    rename_current_branch(wrapper.path(), "main");

    add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &wrapper.path().to_string_lossy(),
        None,
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    let manifest = load_root_from_dir(temp.path()).unwrap();
    let dependency = manifest.manifest.dependencies.values().next().unwrap();
    assert_eq!(dependency.tag, None);
    assert_eq!(dependency.branch.as_deref(), Some("main"));
    assert!(dependency.version.is_none());
}

#[test]
fn add_dependency_accepts_claude_marketplace_wrapper_with_missing_local_plugin_sources() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let wrapper = TempDir::new().unwrap();
    write_marketplace(
        wrapper.path(),
        r#"{
  "plugins": [
    {
      "name": "Missing",
      "source": "./plugins/missing"
    },
    {
      "name": "Axiom",
      "source": "./plugins/axiom"
    }
  ]
}"#,
    );
    write_skill(
        &wrapper.path().join("plugins/axiom/skills/review"),
        "Review",
    );
    init_git_repo(wrapper.path());
    rename_current_branch(wrapper.path(), "main");

    add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &wrapper.path().to_string_lossy(),
        None,
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    let wrapper_alias = normalize_alias_from_url(&wrapper.path().to_string_lossy()).unwrap();
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let wrapper_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == wrapper_alias)
        .unwrap();
    assert_eq!(wrapper_package.dependencies, vec!["axiom"]);

    let plugin_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "axiom")
        .unwrap();
    assert_eq!(plugin_package.skills, vec!["review"]);
}

#[test]
fn add_dependency_accepts_claude_marketplace_wrapper_with_docs_only_local_plugin_sources() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let wrapper = TempDir::new().unwrap();
    write_marketplace(
        wrapper.path(),
        r#"{
  "plugins": [
    {
      "name": "Docs Only",
      "source": "./plugins/docs"
    },
    {
      "name": "Axiom",
      "source": "./plugins/axiom"
    }
  ]
}"#,
    );
    write_file(
        &wrapper.path().join("plugins/docs/README.md"),
        "# Informational plugin\n",
    );
    write_skill(
        &wrapper.path().join("plugins/axiom/skills/review"),
        "Review",
    );
    init_git_repo(wrapper.path());
    rename_current_branch(wrapper.path(), "main");

    add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &wrapper.path().to_string_lossy(),
        None,
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    let wrapper_alias = normalize_alias_from_url(&wrapper.path().to_string_lossy()).unwrap();
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let wrapper_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == wrapper_alias)
        .unwrap();
    assert_eq!(wrapper_package.dependencies, vec!["axiom"]);

    let plugin_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "axiom")
        .unwrap();
    assert_eq!(plugin_package.skills, vec!["review"]);
}

#[test]
fn add_dependency_accepts_claude_marketplace_wrapper_with_hook_only_plugin_sources() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let wrapper = TempDir::new().unwrap();
    write_marketplace(
        wrapper.path(),
        r#"{
  "plugins": [
    {
      "name": "Hook Only",
      "source": "./plugins/hook-only"
    },
    {
      "name": "Axiom",
      "source": "./plugins/axiom"
    }
  ]
}"#,
    );
    write_modern_claude_plugin_json(&wrapper.path().join("plugins/hook-only"), "1.0.0");
    write_file(
        &wrapper.path().join("plugins/hook-only/hooks/hooks.json"),
        "{\n  \"hooks\": []\n}\n",
    );
    write_skill(
        &wrapper.path().join("plugins/axiom/skills/review"),
        "Review",
    );
    init_git_repo(wrapper.path());
    rename_current_branch(wrapper.path(), "main");

    let summary = add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &wrapper.path().to_string_lossy(),
        None,
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    let wrapper_alias = normalize_alias_from_url(&wrapper.path().to_string_lossy()).unwrap();
    assert_eq!(
        summary
            .dependency_members
            .iter()
            .map(|member| (member.id.as_str(), member.enabled))
            .collect::<Vec<_>>(),
        vec![("axiom", false), ("hook_only", false)]
    );
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let wrapper_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == wrapper_alias)
        .unwrap();
    assert!(wrapper_package.dependencies.is_empty());
    assert!(
        !lockfile
            .packages
            .iter()
            .any(|package| package.alias == "axiom" || package.alias == "hook_only")
    );
}

#[test]
fn sync_emits_claude_plugin_command_hooks_from_dependency_packages() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    write_manifest(
        temp.path(),
        r#"
[dependencies.hook_plugin]
path = "vendor/hook-plugin"
"#,
    );
    write_file(
        &temp
            .path()
            .join("vendor/hook-plugin/.claude-plugin/plugin.json"),
        r#"{
  "name": "hook-plugin"
}
"#,
    );
    write_file(
        &temp.path().join("vendor/hook-plugin/hooks/hooks.json"),
        r#"{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Write|Edit",
        "hooks": [
          {
            "type": "command",
            "command": "${CLAUDE_PLUGIN_ROOT}/scripts/format-code.sh"
          }
        ]
      }
    ]
  }
}
"#,
    );
    write_file(
        &temp
            .path()
            .join("vendor/hook-plugin/scripts/format-code.sh"),
        "#!/usr/bin/env bash\nexit 0\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Claude]).unwrap();

    let settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(temp.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        settings["hooks"]["PostToolUse"][0]["matcher"].as_str(),
        Some("Write|Edit")
    );
    let command = settings["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(command.contains("./.claude/hooks/nodus-plugin-hook-"));

    let wrapper_script = fs::read_to_string(
        temp.path()
            .join(".claude/hooks")
            .read_dir()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path(),
    )
    .unwrap();
    assert!(wrapper_script.contains("CLAUDE_PLUGIN_ROOT"));
    assert!(wrapper_script.contains(".nodus-global/packages/"));
    assert!(wrapper_script.contains("/claude-plugin"));
    assert!(wrapper_script.contains("${CLAUDE_PLUGIN_ROOT}/scripts/format-code.sh"));

    assert!(global_plugin_file_exists(
        temp.path(),
        "claude-plugin",
        "hooks/hooks.json"
    ));
    assert!(global_plugin_file_exists(
        temp.path(),
        "claude-plugin",
        "scripts/format-code.sh"
    ));

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let hook_plugin = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "hook_plugin")
        .unwrap();
    assert!(
        hook_plugin
            .owned_subtrees
            .iter()
            .all(|path| !path.contains("claude-plugin")),
        "Claude hook-compat plugin roots should not be project-owned subtrees; got {:?}",
        hook_plugin.owned_subtrees
    );
    let root = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "root")
        .unwrap();
    assert!(
        root.owned_files
            .iter()
            .all(|path| !path.starts_with(".nodus/packages/hook_plugin/claude-plugin/")),
        "root owned_files should not enumerate package-internal Claude plugin files; got {:?}",
        root.owned_files
    );
}

#[test]
fn sync_emits_claude_plugin_command_hooks_from_manifest_declared_hook_sources() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    write_manifest(
        temp.path(),
        r#"
[dependencies.hook_plugin]
path = "vendor/hook-plugin"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/hook-plugin"),
        r#"
claude_plugin_hooks = ["hooks/hooks.json"]
"#,
    );
    write_file(
        &temp.path().join("vendor/hook-plugin/hooks/hooks.json"),
        r#"{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Write|Edit",
        "hooks": [
          {
            "type": "command",
            "command": "${CLAUDE_PLUGIN_ROOT}/scripts/format-code.sh"
          }
        ]
      }
    ]
  }
}
"#,
    );
    write_file(
        &temp
            .path()
            .join("vendor/hook-plugin/scripts/format-code.sh"),
        "#!/usr/bin/env bash\nexit 0\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Claude]).unwrap();

    let settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(temp.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        settings["hooks"]["PostToolUse"][0]["matcher"].as_str(),
        Some("Write|Edit")
    );
    let command = settings["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(command.contains("./.claude/hooks/nodus-plugin-hook-"));
    assert!(global_plugin_file_exists(
        temp.path(),
        "claude-plugin",
        "hooks/hooks.json"
    ));
    assert!(global_plugin_file_exists(
        temp.path(),
        "claude-plugin",
        "scripts/format-code.sh"
    ));
}

#[test]
fn sync_prefers_native_claude_hooks_over_plugin_hook_compat() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    write_manifest(
        temp.path(),
        r#"
[dependencies.hook_plugin]
path = "vendor/hook-plugin"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/hook-plugin"),
        r#"
[[hooks]]
id = "hook-plugin.format-code"
event = "post_tool_use"
adapters = ["claude"]

[hooks.matcher]
tool_names = ["bash"]

[hooks.handler]
type = "command"
command = "./scripts/format-code.sh"
"#,
    );
    write_file(
        &temp
            .path()
            .join("vendor/hook-plugin/.claude-plugin/plugin.json"),
        r#"{
  "name": "hook-plugin"
}
"#,
    );
    write_file(
        &temp.path().join("vendor/hook-plugin/hooks/hooks.json"),
        r#"{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Write|Edit",
        "hooks": [
          {
            "type": "command",
            "command": "${CLAUDE_PLUGIN_ROOT}/scripts/format-code.sh"
          }
        ]
      }
    ]
  }
}
"#,
    );
    write_file(
        &temp
            .path()
            .join("vendor/hook-plugin/scripts/format-code.sh"),
        "#!/usr/bin/env bash\nexit 0\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Claude]).unwrap();

    // The native `[[hooks]]` win over the plugin-hook compat surface: the
    // dependency's hook ends up inside its Claude plugin's `hooks.json` (not
    // the workspace settings, not the `nodus-plugin-hook-*` compat wrappers).
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let hook_plugin = resolution
        .packages
        .iter()
        .find(|package| package.alias == "hook_plugin")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), hook_plugin, Adapter::Claude);
    assert!(
        plugin_root.exists(),
        "expected Claude plugin folder at {}",
        plugin_root.display(),
    );
    let plugin_hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(plugin_root.join("hooks/hooks.json")).unwrap())
            .unwrap();
    assert_eq!(
        plugin_hooks["hooks"]["PostToolUse"][0]["matcher"].as_str(),
        Some("Bash"),
    );
    let command = plugin_hooks["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(
        command.contains("${CLAUDE_PLUGIN_ROOT}/hooks/scripts/"),
        "expected plugin-root reference, got `{command}`",
    );

    // The workspace `.claude/settings.json` carries no `nodus-hook-` or
    // `nodus-plugin-hook-` entries for this dependency.
    let settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(temp.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    let workspace_commands: Vec<&str> = settings["hooks"]
        .as_object()
        .into_iter()
        .flat_map(|events| events.values())
        .flat_map(|entries| entries.as_array().into_iter().flatten())
        .flat_map(|entry| entry["hooks"].as_array().into_iter().flatten())
        .filter_map(|hook| hook["command"].as_str())
        .filter(|command| {
            command.contains("./.claude/hooks/nodus-hook-")
                || command.contains("./.claude/hooks/nodus-plugin-hook-")
        })
        .collect();
    assert!(
        workspace_commands.is_empty(),
        "workspace settings should not carry dependency hook entries: {workspace_commands:?}",
    );

    // The plugin script uses ${CLAUDE_PLUGIN_ROOT} semantics and runs the
    // user's command as declared.
    let scripts_dir = plugin_root.join("hooks/scripts");
    let wrapper_script_path = fs::read_dir(&scripts_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .next()
        .expect("expected at least one wrapper script");
    let wrapper_script = fs::read_to_string(&wrapper_script_path).unwrap();
    assert!(wrapper_script.contains("./scripts/format-code.sh"));
}

#[test]
fn sync_does_not_emit_claude_plugin_hook_compat_for_non_claude_adapters() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    write_manifest(
        temp.path(),
        r#"
[dependencies.hook_plugin]
path = "vendor/hook-plugin"
"#,
    );
    write_file(
        &temp
            .path()
            .join("vendor/hook-plugin/.claude-plugin/plugin.json"),
        r#"{
  "name": "hook-plugin"
}
"#,
    );
    write_file(
        &temp.path().join("vendor/hook-plugin/hooks/hooks.json"),
        r#"{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "${CLAUDE_PLUGIN_ROOT}/scripts/format-code.sh"
          }
        ]
      }
    ]
  }
}
"#,
    );
    write_file(
        &temp
            .path()
            .join("vendor/hook-plugin/scripts/format-code.sh"),
        "#!/usr/bin/env bash\nexit 0\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    assert!(!temp.path().join(".claude").exists());
    assert!(!temp.path().join(".codex/hooks.json").exists());
    assert!(
        !temp
            .path()
            .join(".nodus/packages/hook_plugin/claude-plugin")
            .exists()
    );
}

#[test]
fn sync_keeps_claude_plugin_hook_compat_disabled_for_codex_startup_hooks() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["codex"]

[launch_hooks]
sync_on_startup = true

[dependencies.hook_plugin]
path = "vendor/hook-plugin"
"#,
    );
    write_file(
        &temp
            .path()
            .join("vendor/hook-plugin/.claude-plugin/plugin.json"),
        r#"{
  "name": "hook-plugin"
}
"#,
    );
    write_file(
        &temp.path().join("vendor/hook-plugin/hooks/hooks.json"),
        r#"{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "${CLAUDE_PLUGIN_ROOT}/scripts/format-code.sh"
          }
        ]
      }
    ]
  }
}
"#,
    );
    write_file(
        &temp
            .path()
            .join("vendor/hook-plugin/scripts/format-code.sh"),
        "#!/usr/bin/env bash\nexit 0\n",
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    assert!(temp.path().join(".codex/hooks.json").exists());
    assert!(!temp.path().join(".claude").exists());
    assert!(
        !temp
            .path()
            .join(".nodus/packages/hook_plugin/claude-plugin")
            .exists()
    );
}

#[test]
fn doctor_accepts_claude_plugin_hook_compat_after_first_sync() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["claude"]

[dependencies.hook_plugin]
path = "vendor/hook-plugin"
"#,
    );
    write_file(
        &temp
            .path()
            .join("vendor/hook-plugin/.claude-plugin/plugin.json"),
        r#"{
  "name": "hook-plugin"
}
"#,
    );
    write_file(
        &temp.path().join("vendor/hook-plugin/hooks/hooks.json"),
        r#"{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "${CLAUDE_PLUGIN_ROOT}/scripts/format-code.sh"
          }
        ]
      }
    ]
  }
}
"#,
    );
    write_file(
        &temp
            .path()
            .join("vendor/hook-plugin/scripts/format-code.sh"),
        "#!/usr/bin/env bash\nexit 0\n",
    );
    write_file(
        &temp.path().join("vendor/hook-plugin/README.md"),
        "# Hook Plugin\n",
    );
    write_file(
        &temp.path().join("vendor/hook-plugin/CONTRIBUTING.md"),
        "Follow the guide.\n",
    );
    write_file(
        &temp
            .path()
            .join("vendor/hook-plugin/references/testing-patterns.md"),
        "Test patterns.\n",
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Check,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Healthy);
    assert!(summary.findings.is_empty());
    assert!(global_plugin_file_exists(
        temp.path(),
        "claude-plugin",
        "hooks/hooks.json"
    ));
}

#[test]
fn add_dependency_accepts_all_claude_marketplace_remote_sources_and_syncs_contents() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let remote_root = TempDir::new().unwrap();
    write_skill(&remote_root.path().join("skills/checks"), "Checks");
    init_git_repo(remote_root.path());
    rename_current_branch(remote_root.path(), "main");

    let remote_subdir = TempDir::new().unwrap();
    write_skill(
        &remote_subdir.path().join("plugins/external/skills/review"),
        "Review",
    );
    write_claude_plugin_json(&remote_subdir.path().join("plugins/external"), "1.2.3");
    init_git_repo(remote_subdir.path());
    rename_current_branch(remote_subdir.path(), "main");

    let wrapper = TempDir::new().unwrap();
    let marketplace = serde_json::json!({
        "plugins": [
            {
                "name": "External Root",
                "source": {
                    "source": "url",
                    "url": remote_root.path().to_string_lossy(),
                }
            },
            {
                "name": "External Subdir",
                "source": {
                    "source": "git-subdir",
                    "url": remote_subdir.path().to_string_lossy(),
                    "path": "plugins/external",
                    "ref": "main"
                }
            }
        ]
    });
    write_marketplace(
        wrapper.path(),
        &serde_json::to_string_pretty(&marketplace).unwrap(),
    );
    init_git_repo(wrapper.path());
    rename_current_branch(wrapper.path(), "main");

    add_dependency_in_dir_with_adapters_accept_all(
        temp.path(),
        cache.path(),
        &wrapper.path().to_string_lossy(),
        None,
        &Adapter::ALL,
        &[],
        true,
    )
    .unwrap();

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let root_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "external_root")
        .unwrap();
    assert_eq!(root_package.source.kind, "git");
    assert_eq!(root_package.source.path, None);
    assert_eq!(root_package.source.branch.as_deref(), Some("main"));

    let subdir_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "external_subdir")
        .unwrap();
    assert_eq!(subdir_package.source.kind, "git");
    assert_eq!(
        subdir_package.source.path.as_deref(),
        Some("plugins/external")
    );
    assert_eq!(subdir_package.source.branch.as_deref(), Some("main"));
    assert_eq!(subdir_package.version_tag.as_deref(), Some("1.2.3"));

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let root_package = resolution
        .packages
        .iter()
        .find(|package| package.alias == "external_root")
        .unwrap();
    let subdir_package = resolution
        .packages
        .iter()
        .find(|package| package.alias == "external_subdir")
        .unwrap();

    assert!(matches!(
        &root_package.source,
        PackageSource::Git { subpath: None, branch, .. } if branch.as_deref() == Some("main")
    ));
    assert!(matches!(
        &subdir_package.source,
        PackageSource::Git { subpath, branch, .. }
            if subpath.as_deref() == Some(Path::new("plugins/external"))
                && branch.as_deref() == Some("main")
    ));

    let root_skill_id = namespaced_skill_id(root_package, "checks");
    let subdir_skill_id = namespaced_skill_id(subdir_package, "review");
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &root_skill_id
    ));
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &subdir_skill_id
    ));
}

#[test]
fn add_dependency_accepts_codex_marketplace_wrapper_and_syncs_plugin_contents() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let wrapper = TempDir::new().unwrap();
    write_codex_marketplace(
        wrapper.path(),
        r#"{
  "plugins": [
    {
      "name": "Axiom",
      "source": {
        "source": "local",
        "path": "./plugins/axiom"
      },
      "policy": {
        "installation": "AVAILABLE",
        "authentication": "ON_INSTALL"
      },
      "category": "Productivity"
    }
  ]
}"#,
    );
    write_skill(
        &wrapper.path().join("plugins/axiom/skills/review"),
        "Review",
    );
    write_codex_mcp_config(&wrapper.path().join("plugins/axiom"));
    write_codex_plugin_json(
        &wrapper.path().join("plugins/axiom"),
        "2.34.0",
        Some("./.mcp.json"),
    );
    init_git_repo(wrapper.path());
    tag_repo(wrapper.path(), "v0.4.0");
    let wrapper_alias = normalize_alias_from_url(&wrapper.path().to_string_lossy()).unwrap();

    add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &wrapper.path().to_string_lossy(),
        Some("v0.4.0"),
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let wrapper_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == wrapper_alias)
        .unwrap();
    assert_eq!(wrapper_package.version_tag.as_deref(), Some("2.34.0"));
    assert!(wrapper_package.skills.is_empty());
    assert_eq!(wrapper_package.dependencies, vec!["axiom"]);

    let plugin_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "axiom")
        .unwrap();
    assert_eq!(plugin_package.version_tag.as_deref(), Some("2.34.0"));
    assert_eq!(
        plugin_package.source.path.as_deref(),
        Some("./plugins/axiom")
    );
    assert_eq!(plugin_package.skills, vec!["review"]);
    assert_eq!(plugin_package.mcp_servers, vec!["figma"]);

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let plugin_package = resolution
        .packages
        .iter()
        .find(|package| package.alias == "axiom")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(plugin_package, "review");
    let codex_plugin_root = global_native_plugin_root(temp.path(), plugin_package, Adapter::Codex);
    assert!(
        codex_plugin_root
            .join(format!("skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(".codex/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );

    assert!(
        !temp.path().join(".mcp.json").exists(),
        "Claude native plugin MCP should replace dependency project-level MCP output"
    );
    let claude_plugin_root =
        global_native_plugin_root(temp.path(), plugin_package, Adapter::Claude);
    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(claude_plugin_root.join(".mcp.json")).unwrap())
            .unwrap();
    assert_eq!(
        json["mcpServers"]["figma"]["url"].as_str(),
        Some("http://127.0.0.1:3845/mcp")
    );
    let codex_mcp: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(codex_plugin_root.join(".mcp.json")).unwrap())
            .unwrap();
    assert_eq!(
        codex_mcp["mcpServers"]["figma"]["url"].as_str(),
        Some("http://127.0.0.1:3845/mcp")
    );
    assert!(!temp.path().join(".codex/config.toml").exists());
    assert_codex_user_config_registers_plugins(temp.path(), &["axiom@nodus"]);
}

#[test]
fn add_dependency_accepts_modern_claude_mcp_only_package_and_syncs_mcp_metadata() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let plugin = TempDir::new().unwrap();
    write_modern_claude_plugin_json(plugin.path(), "2.34.0");
    write_file(
        &plugin.path().join(".mcp.json"),
        r#"{
  "github": {
    "type": "http",
    "url": "https://api.githubcopilot.com/mcp/",
    "headers": {
      "Authorization": "Bearer ${GITHUB_PERSONAL_ACCESS_TOKEN}"
    }
  },
  "discord": {
    "command": "bun",
    "args": ["run", "--cwd", "${CLAUDE_PLUGIN_ROOT}", "--shell=bun", "--silent", "start"]
  }
}
"#,
    );
    init_git_repo(plugin.path());
    tag_repo(plugin.path(), "v0.4.0");

    add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &plugin.path().to_string_lossy(),
        Some("v0.4.0"),
        &[Adapter::Codex],
        &[],
    )
    .unwrap();

    let alias = normalize_alias_from_url(&plugin.path().to_string_lossy()).unwrap();
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == alias)
        .unwrap();
    assert_eq!(package.version_tag.as_deref(), Some("2.34.0"));
    assert_eq!(package.mcp_servers, vec!["discord", "github"]);
    assert!(package.skills.is_empty());

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(
        json["mcpServers"][format!("{alias}__github")]["type"].as_str(),
        Some("http")
    );
    assert_eq!(
        json["mcpServers"][format!("{alias}__github")]["headers"]["Authorization"].as_str(),
        Some("Bearer ${GITHUB_PERSONAL_ACCESS_TOKEN}")
    );
    assert_eq!(
        json["mcpServers"][format!("{alias}__discord")]["command"].as_str(),
        Some("bun")
    );
    let package = resolve_project(temp.path(), cache.path(), ResolveMode::Sync)
        .unwrap()
        .packages
        .into_iter()
        .find(|package| package.alias == alias)
        .unwrap();
    let emitted_cwd = Path::new(
        json["mcpServers"][format!("{alias}__discord")]["cwd"]
            .as_str()
            .unwrap(),
    );
    let emitted_cwd = canonicalize_path(emitted_cwd).unwrap();
    assert_eq!(emitted_cwd, canonicalize_path(&package.root).unwrap());
    assert_eq!(
        json["mcpServers"][format!("{alias}__discord")]["args"],
        serde_json::json!(["run", "--shell=bun", "--silent", "start"])
    );
}

#[test]
fn add_dependency_accepts_manifest_only_hook_package_and_syncs_claude_plugin_hooks() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let package = TempDir::new().unwrap();
    write_file(
        &package.path().join(MANIFEST_FILE),
        r#"
[[hooks]]
id = "fuli.claude.session-start"
event = "session_start"
adapters = ["claude"]

[hooks.matcher]
sources = ["startup", "resume", "clear", "compact"]

[hooks.handler]
type = "command"
command = "fuli integration claude hook session-start"

[[hooks]]
id = "fuli.claude.user-prompt-submit"
event = "user_prompt_submit"
adapters = ["claude"]

[hooks.handler]
type = "command"
command = "fuli integration claude hook user-prompt-submit"

[[hooks]]
id = "fuli.claude.post-tool-use"
event = "post_tool_use"
adapters = ["claude"]

[hooks.handler]
type = "command"
command = "fuli integration claude hook post-tool-use"

[[hooks]]
id = "fuli.claude.stop"
event = "stop"
adapters = ["claude"]

[hooks.handler]
type = "command"
command = "fuli integration claude hook stop"

[[hooks]]
id = "fuli.claude.session-end"
event = "session_end"
adapters = ["claude"]

[hooks.handler]
type = "command"
command = "fuli integration claude hook session-end"
"#,
    );
    init_git_repo(package.path());
    rename_current_branch(package.path(), "main");

    let add_summary = add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &package.path().to_string_lossy(),
        None,
        &[Adapter::Claude],
        &[],
    )
    .unwrap();
    let alias = add_summary.alias;

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Claude]).unwrap();

    // Workspace `.claude/settings.json` no longer carries dependency hook
    // entries — they now live inside the plugin folder.
    let settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(temp.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    let workspace_hook_entries_for_dep: Vec<&str> = settings["hooks"]
        .as_object()
        .into_iter()
        .flat_map(|events| events.values())
        .flat_map(|entries| entries.as_array().into_iter().flatten())
        .flat_map(|entry| entry["hooks"].as_array().into_iter().flatten())
        .filter_map(|hook| hook["command"].as_str())
        .filter(|command| command.contains("./.claude/hooks/nodus-hook-"))
        .collect();
    assert!(
        workspace_hook_entries_for_dep.is_empty(),
        "dependency hooks should not appear in workspace settings: {workspace_hook_entries_for_dep:?}",
    );

    // The plugin is enabled in the workspace marketplace.
    let enabled_plugins = settings["enabledPlugins"]
        .as_object()
        .expect("enabledPlugins object");
    let marketplace: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(generated_claude_marketplace_path(temp.path())).unwrap(),
    )
    .unwrap();
    let plugin_name = marketplace["plugins"][0]["name"].as_str().unwrap();
    assert!(
        enabled_plugins
            .keys()
            .any(|key| key == &format!("{plugin_name}@nodus")),
        "enabledPlugins should reference `{plugin_name}`: {enabled_plugins:?}",
    );

    // The plugin's own `hooks/hooks.json` carries every declared event.
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let package = resolution
        .packages
        .iter()
        .find(|package| package.alias == alias)
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), package, Adapter::Claude);
    let plugin_hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(plugin_root.join("hooks/hooks.json")).unwrap())
            .unwrap();

    let session_start = plugin_hooks["hooks"]["SessionStart"].as_array().unwrap();
    assert!(session_start.iter().any(|entry| {
        entry["matcher"].as_str() == Some("startup|resume|clear|compact")
            && entry["hooks"].as_array().is_some_and(|hooks| {
                hooks.iter().any(|hook| {
                    hook["command"]
                        .as_str()
                        .is_some_and(|command| command.contains("${CLAUDE_PLUGIN_ROOT}"))
                })
            })
    }));
    for event in ["UserPromptSubmit", "PostToolUse", "Stop", "SessionEnd"] {
        let command = plugin_hooks["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap_or_else(|| panic!("missing command for {event}"));
        assert!(
            command.contains("${CLAUDE_PLUGIN_ROOT}/hooks/scripts/"),
            "expected plugin-root reference for {event}, got `{command}`",
        );
    }

    // Claude Code auto-loads the standard `hooks/hooks.json` path. The
    // generated plugin manifest must not reference that same file again.
    let plugin_manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(plugin_root.join(".claude-plugin/plugin.json")).unwrap(),
    )
    .unwrap();
    assert!(
        plugin_manifest.get("hooks").is_none(),
        "standard hooks file should not be duplicated in plugin manifest: {plugin_manifest:?}",
    );

    // Hook scripts live inside the plugin root.
    let scripts_dir = plugin_root.join("hooks/scripts");
    let script_count = fs::read_dir(&scripts_dir).unwrap().count();
    assert_eq!(
        script_count,
        5,
        "expected one script per declared hook in {}",
        scripts_dir.display(),
    );
}

#[test]
fn add_dependency_overlays_modern_claude_plugin_manifest_mcp_servers_and_syncs_metadata() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let plugin = TempDir::new().unwrap();
    write_modern_claude_plugin_json_with_fields(
        plugin.path(),
        &[
            r#"  "version": "0.2.0""#,
            r#"  "mcpServers": ["./config/path.json", { "shared": { "command": "inline" }, "inlineOnly": { "command": "bun", "args": ["run", "--cwd", "${CLAUDE_PLUGIN_ROOT}", "start"] } }, "./config/final.json"]"#,
        ],
    );
    write_file(
        &plugin.path().join(".mcp.json"),
        r#"{
  "shared": {
    "command": "base"
  },
  "baseOnly": {
    "command": "base-only"
  }
}
"#,
    );
    write_file(
        &plugin.path().join("config/path.json"),
        r#"{
  "mcpServers": {
    "shared": {
      "command": "path"
    },
    "pathOnly": {
      "command": "path-only"
    }
  }
}
"#,
    );
    write_file(&plugin.path().join("tools.yaml"), "version: v1\n");
    write_file(
        &plugin.path().join("config/final.json"),
        r#"{
  "shared": {
    "command": "final"
  },
  "rooted": {
    "command": "toolbox",
    "args": ["--tools-file", "${CLAUDE_PLUGIN_ROOT}/tools.yaml", "--stdio"]
  }
}
"#,
    );
    init_git_repo(plugin.path());
    tag_repo(plugin.path(), "v0.4.0");

    add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &plugin.path().to_string_lossy(),
        Some("v0.4.0"),
        &[Adapter::Codex],
        &[],
    )
    .unwrap();

    let alias = normalize_alias_from_url(&plugin.path().to_string_lossy()).unwrap();
    let package = resolve_project(temp.path(), cache.path(), ResolveMode::Sync)
        .unwrap()
        .packages
        .into_iter()
        .find(|package| package.alias == alias)
        .unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(
        json["mcpServers"][format!("{alias}__shared")]["command"].as_str(),
        Some("final")
    );
    assert_eq!(
        json["mcpServers"][format!("{alias}__baseOnly")]["command"].as_str(),
        Some("base-only")
    );
    assert_eq!(
        json["mcpServers"][format!("{alias}__pathOnly")]["command"].as_str(),
        Some("path-only")
    );
    assert_eq!(
        json["mcpServers"][format!("{alias}__inlineOnly")]["command"].as_str(),
        Some("bun")
    );
    assert_eq!(
        json["mcpServers"][format!("{alias}__inlineOnly")]["args"],
        serde_json::json!(["run", "start"])
    );
    assert_eq!(
        canonicalize_path(Path::new(
            json["mcpServers"][format!("{alias}__inlineOnly")]["cwd"]
                .as_str()
                .unwrap(),
        ))
        .unwrap(),
        canonicalize_path(&package.root).unwrap()
    );
    let rooted_args = json["mcpServers"][format!("{alias}__rooted")]["args"]
        .as_array()
        .unwrap();
    assert_eq!(rooted_args[0].as_str(), Some("--tools-file"));
    assert_eq!(rooted_args[2].as_str(), Some("--stdio"));
    assert_eq!(
        canonicalize_path(Path::new(rooted_args[1].as_str().unwrap())).unwrap(),
        canonicalize_path(&package.root.join("tools.yaml")).unwrap()
    );
}

#[test]
fn add_dependency_normalizes_claude_plugin_root_arg_paths_in_mcp_metadata() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let plugin = TempDir::new().unwrap();
    write_modern_claude_plugin_json(plugin.path(), "0.1.1");
    write_file(
        &plugin.path().join(".mcp.json"),
        r#"{
  "mcpServers": {
    "cockroachdb-toolbox": {
      "command": "toolbox",
      "args": ["--tools-file", "${CLAUDE_PLUGIN_ROOT}/tools.yaml", "--stdio"]
    }
  }
}
"#,
    );
    write_file(&plugin.path().join("tools.yaml"), "version: v1\n");
    init_git_repo(plugin.path());
    rename_current_branch(plugin.path(), "main");

    add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &plugin.path().to_string_lossy(),
        None,
        &[Adapter::Codex],
        &[],
    )
    .unwrap();

    let alias = normalize_alias_from_url(&plugin.path().to_string_lossy()).unwrap();
    let package = resolve_project(temp.path(), cache.path(), ResolveMode::Sync)
        .unwrap()
        .packages
        .into_iter()
        .find(|package| package.alias == alias)
        .unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".mcp.json")).unwrap()).unwrap();
    let args = json["mcpServers"][format!("{alias}__cockroachdb-toolbox")]["args"]
        .as_array()
        .unwrap();
    assert_eq!(args[0].as_str(), Some("--tools-file"));
    assert_eq!(args[2].as_str(), Some("--stdio"));
    assert_eq!(
        canonicalize_path(Path::new(args[1].as_str().unwrap())).unwrap(),
        canonicalize_path(&package.root.join("tools.yaml")).unwrap()
    );
}

#[test]
fn add_dependency_syncs_path_dependencies_inside_manifest_only_wrapper_repo() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let wrapper = TempDir::new().unwrap();
    write_file(
        &wrapper.path().join(MANIFEST_FILE),
        r#"
[dependencies]
bundled = { path = "vendor/bundled" }
"#,
    );
    write_skill(
        &wrapper.path().join("vendor/bundled/skills/bundled"),
        "Bundled",
    );
    init_git_repo(wrapper.path());
    tag_repo(wrapper.path(), "v0.3.0");
    let wrapper_alias = normalize_alias_from_url(&wrapper.path().to_string_lossy()).unwrap();

    add_dependency_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        &wrapper.path().to_string_lossy(),
        Some("v0.3.0"),
        &Adapter::ALL,
        &[],
    )
    .unwrap();

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let wrapper_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == wrapper_alias)
        .unwrap();
    assert_eq!(wrapper_package.dependencies, vec!["bundled"]);
    let bundled_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "bundled")
        .unwrap();
    assert_eq!(bundled_package.source.kind, "path");
    assert_eq!(
        bundled_package.source.path.as_deref(),
        Some("vendor/bundled")
    );
    assert_eq!(bundled_package.skills, vec!["bundled"]);

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let bundled_package = resolution
        .packages
        .iter()
        .find(|package| package.alias == "bundled")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(bundled_package, "bundled");
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
}

#[test]
fn root_resolution_includes_dev_dependencies() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dev-dependencies]
tooling = { path = "vendor/tooling" }
"#,
    );
    write_skill(
        &temp.path().join("vendor/tooling/skills/tooling"),
        "Tooling",
    );

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();

    assert!(
        resolution
            .packages
            .iter()
            .any(|package| package.alias == "tooling")
    );
    let lockfile = resolution
        .to_lockfile(Adapters::from_slice(&Adapter::ALL), temp.path())
        .unwrap();
    let root_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "root")
        .unwrap();
    assert_eq!(root_package.dependencies, vec!["tooling"]);
}

#[test]
fn consumed_packages_do_not_export_dev_dependencies() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies]
wrapper = { path = "vendor/wrapper", members = ["shared"] }
"#,
    );
    write_file(
        &temp.path().join("vendor/wrapper/nodus.toml"),
        r#"
[dependencies]
shared = { path = "vendor/shared" }

[dev-dependencies]
tooling = { path = "vendor/tooling" }
"#,
    );
    write_skill(
        &temp
            .path()
            .join("vendor/wrapper/vendor/shared/skills/shared"),
        "Shared",
    );
    write_skill(
        &temp
            .path()
            .join("vendor/wrapper/vendor/tooling/skills/tooling"),
        "Tooling",
    );

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();

    assert!(
        resolution
            .packages
            .iter()
            .any(|package| package.alias == "shared")
    );
    assert!(
        !resolution
            .packages
            .iter()
            .any(|package| package.alias == "tooling")
    );
    let lockfile = resolution
        .to_lockfile(Adapters::from_slice(&Adapter::ALL), temp.path())
        .unwrap();
    let wrapper_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "wrapper")
        .unwrap();
    assert_eq!(wrapper_package.dependencies, vec!["shared"]);
}

#[test]
fn remove_dependency_updates_manifest_and_prunes_managed_files() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let (_repo, url) = create_git_dependency();
    let alias = normalize_alias_from_url(&url).unwrap();

    add_dependency_all(temp.path(), cache.path(), &url, Some("v0.1.0"));

    let manifest_before = load_root_from_dir(temp.path()).unwrap();
    let dependency = resolve_project(temp.path(), cache.path(), ResolveMode::Sync)
        .unwrap()
        .packages
        .into_iter()
        .find(|package| package.alias != "root")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(&dependency, "review");

    assert!(manifest_before.manifest.dependencies.contains_key(&alias));
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));

    remove_dependency_in_dir(temp.path(), cache.path(), &alias).unwrap();

    let manifest_after = load_root_from_dir(temp.path()).unwrap();
    assert!(manifest_after.manifest.dependencies.is_empty());
    assert!(
        !temp
            .path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert_eq!(lockfile.packages.len(), 1);
    assert_eq!(lockfile.packages[0].alias, "root");
}

#[test]
fn remove_dependency_accepts_repository_reference() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let (_repo, url) = create_git_dependency();

    add_dependency_all(temp.path(), cache.path(), &url, Some("v0.1.0"));

    remove_dependency_in_dir(temp.path(), cache.path(), &url).unwrap();

    let manifest = load_root_from_dir(temp.path()).unwrap();
    assert!(manifest.manifest.dependencies.is_empty());
}

#[test]
fn remove_dependency_rejects_unknown_package() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let error = remove_dependency_in_dir(temp.path(), cache.path(), "missing")
        .unwrap_err()
        .to_string();

    assert!(error.contains("dependency `missing` does not exist"));
}

#[test]
fn global_add_installs_to_all_detected_supported_adapters() {
    let store = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let cache = cache_dir();
    let (_repo, url) = create_git_dependency();
    fs::create_dir_all(home.path().join(".codex")).unwrap();
    fs::create_dir_all(home.path().join(".claude")).unwrap();
    fs::create_dir_all(home.path().join(".github/skills")).unwrap();

    let reporter = Reporter::silent();
    let install_paths = InstallPaths::new(
        InstallScope::Global,
        store.path().join("global"),
        home.path().to_path_buf(),
        home.path().to_path_buf(),
    );
    let summary = add_dependency_at_paths_with_adapters(
        &install_paths,
        cache.path(),
        &url,
        AddDependencyOptions {
            git_ref: None,
            version_req: None,
            kind: DependencyKind::Dependency,
            adapters: &[],
            components: &[],
            sync_on_launch: false,
            accept_all_dependencies: false,
        },
        &reporter,
    )
    .unwrap();

    assert_eq!(summary.adapters, vec![Adapter::Claude, Adapter::Codex]);
    let global_root = store.path().join("global");
    assert!(global_root.join(MANIFEST_FILE).exists());
    assert!(global_root.join(LOCKFILE_NAME).exists());

    let manifest = load_root_from_dir(&global_root).unwrap();
    assert_eq!(
        manifest.manifest.enabled_adapters().unwrap(),
        [Adapter::Claude, Adapter::Codex]
    );
    let dependency = resolve_project(&global_root, cache.path(), ResolveMode::Sync)
        .unwrap()
        .packages
        .into_iter()
        .find(|package| package.alias != "root")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(&dependency, "review");

    assert!(runtime_skill_exists(
        home.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
    assert!(runtime_skill_exists(
        home.path(),
        Adapter::Codex,
        &managed_skill_id
    ));
    assert!(
        !home
            .path()
            .join(".github/skills")
            .join(&managed_skill_id)
            .exists()
    );
}

#[test]
fn global_remove_prunes_home_scoped_outputs() {
    let store = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let cache = cache_dir();
    let (_repo, url) = create_git_dependency();
    fs::create_dir_all(home.path().join(".codex")).unwrap();

    let reporter = Reporter::silent();
    let install_paths = InstallPaths::new(
        InstallScope::Global,
        store.path().join("global"),
        home.path().to_path_buf(),
        home.path().to_path_buf(),
    );
    add_dependency_at_paths_with_adapters(
        &install_paths,
        cache.path(),
        &url,
        AddDependencyOptions {
            git_ref: None,
            version_req: None,
            kind: DependencyKind::Dependency,
            adapters: &[],
            components: &[],
            sync_on_launch: false,
            accept_all_dependencies: false,
        },
        &reporter,
    )
    .unwrap();

    let global_root = store.path().join("global");
    let dependency = resolve_project(&global_root, cache.path(), ResolveMode::Sync)
        .unwrap()
        .packages
        .into_iter()
        .find(|package| package.alias != "root")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(&dependency, "review");
    let managed_skill = runtime_skill_path(home.path(), Adapter::Codex, &managed_skill_id);
    assert!(managed_skill.exists());

    let alias = normalize_alias_from_url(&url).unwrap();
    remove_dependency_at_paths(&install_paths, cache.path(), &alias, &reporter).unwrap();

    let manifest = load_root_from_dir(&global_root).unwrap();
    assert!(manifest.manifest.dependencies.is_empty());
    assert!(!managed_skill.exists());
}

#[test]
fn global_add_requires_supported_detected_adapters_when_none_are_explicit() {
    let store = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let cache = cache_dir();
    let (_repo, url) = create_git_dependency();
    fs::create_dir_all(home.path().join(".github/skills")).unwrap();

    let reporter = Reporter::silent();
    let install_paths = InstallPaths::new(
        InstallScope::Global,
        store.path().join("global"),
        home.path().to_path_buf(),
        home.path().to_path_buf(),
    );
    let error = add_dependency_at_paths_with_adapters(
        &install_paths,
        cache.path(),
        &url,
        AddDependencyOptions {
            git_ref: None,
            version_req: None,
            kind: DependencyKind::Dependency,
            adapters: &[],
            components: &[],
            sync_on_launch: false,
            accept_all_dependencies: false,
        },
        &reporter,
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("no supported global adapters found"));
}

#[test]
fn global_add_rejects_sync_on_launch() {
    let store = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let cache = cache_dir();
    let (_repo, url) = create_git_dependency();
    fs::create_dir_all(home.path().join(".codex")).unwrap();

    let reporter = Reporter::silent();
    let install_paths = InstallPaths::new(
        InstallScope::Global,
        store.path().join("global"),
        home.path().to_path_buf(),
        home.path().to_path_buf(),
    );
    let error = add_dependency_at_paths_with_adapters(
        &install_paths,
        cache.path(),
        &url,
        AddDependencyOptions {
            git_ref: None,
            version_req: None,
            kind: DependencyKind::Dependency,
            adapters: &[],
            components: &[],
            sync_on_launch: true,
            accept_all_dependencies: false,
        },
        &reporter,
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("does not support `--sync-on-launch`"));
}

#[test]
fn sync_emits_dependency_outputs_without_mirroring_root_content() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(&temp.path().join("agents/security.md"), "# Security\n");
    write_file(&temp.path().join("rules/default.rules"), "allow = []\n");
    write_file(&temp.path().join("commands/build.txt"), "cargo test\n");
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/checks"), "Checks");
    write_file(
        &temp.path().join("vendor/shared/agents/shared.md"),
        "# Shared\n",
    );
    write_file(
        &temp.path().join("vendor/shared/rules/default.rules"),
        "allow = []\n",
    );
    write_file(
        &temp.path().join("vendor/shared/commands/build.txt"),
        "cargo test\n",
    );
    write_file(&temp.path().join("AGENTS.md"), "user-owned instructions\n");

    sync_all(temp.path(), cache.path());

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "checks");
    let managed_agent_file = namespaced_file_name(dependency, "shared", "md");
    let managed_copilot_agent_file = namespaced_file_name(dependency, "shared", "agent.md");
    let managed_command_file = resolution_file_name(
        &resolution,
        dependency,
        ArtifactKind::Command,
        "build",
        "md",
    );
    let managed_codex_command_skill =
        resolution_codex_command_skill_id(&resolution, dependency, "build");
    let managed_claude_rule_file = namespaced_file_name(dependency, "default", "md");
    let managed_cursor_rule_file = namespaced_file_name(dependency, "default", "mdc");

    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
    assert!(runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_agent_file
    ));
    assert!(runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_command_file
    ));
    assert!(runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_claude_rule_file
    ));
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Codex,
        &managed_skill_id
    ));
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Codex,
        &managed_codex_command_skill
    ));
    assert!(
        temp.path()
            .join(format!(".github/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".github/agents/{managed_copilot_agent_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".agents/commands/{managed_command_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".cursor/commands/{managed_command_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".cursor/rules/{managed_cursor_rule_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".cursor/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/agents/{managed_agent_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/commands/{managed_command_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/rules/{managed_claude_rule_file}"))
            .exists()
    );
    assert!(!runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        "security.md"
    ));
    assert!(!temp.path().join(".opencode/agents/security.md").exists());
    assert!(
        fs::read_to_string(
            temp.path()
                .join(format!(".github/skills/{managed_skill_id}/SKILL.md"))
        )
        .unwrap()
        .contains(&format!("name: {managed_skill_id}"))
    );
    assert!(
        fs::read_to_string(
            temp.path()
                .join(format!(".opencode/skills/{managed_skill_id}/SKILL.md"))
        )
        .unwrap()
        .contains(&format!("name: {managed_skill_id}"))
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("AGENTS.md")).unwrap(),
        "user-owned instructions\n"
    );
}

#[test]
fn sync_filters_github_copilot_outputs_by_selected_components() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["copilot"]

[dependencies]
shared = { path = "vendor/shared", components = ["skills"] }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/agents/shared.md"),
        "# Shared\n",
    );

    sync_all(temp.path(), cache.path());

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    let managed_agent_file = namespaced_file_name(dependency, "shared", "agent.md");

    assert!(
        temp.path()
            .join(format!(".github/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(".github/agents/{managed_agent_file}"))
            .exists()
    );
}

#[test]
fn sync_rewrites_github_copilot_skill_name_to_managed_id() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["copilot"]

[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_file(
        &temp.path().join("vendor/shared/skills/review/SKILL.md"),
        "---\nname: shared-review\ndescription: Example review skill.\n---\n# Review\n",
    );

    sync_all(temp.path(), cache.path());

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");

    assert!(
        fs::read_to_string(
            temp.path()
                .join(format!(".github/skills/{managed_skill_id}/SKILL.md"))
        )
        .unwrap()
        .contains(&format!("name: {managed_skill_id}"))
    );
    assert!(!temp.path().join(".github/.gitignore").exists());
}

#[test]
fn sync_filters_dependency_outputs_by_selected_components() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared", components = ["skills"] }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/agents/shared.md"),
        "# Shared\n",
    );
    write_file(
        &temp.path().join("vendor/shared/rules/default.rules"),
        "allow = []\n",
    );
    write_file(
        &temp.path().join("vendor/shared/commands/build.txt"),
        "cargo test\n",
    );

    sync_all(temp.path(), cache.path());

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    let managed_agent_file = namespaced_file_name(dependency, "shared", "md");
    let managed_command_file = resolution_file_name(
        &resolution,
        dependency,
        ArtifactKind::Command,
        "build",
        "md",
    );
    let managed_claude_rule_file = namespaced_file_name(dependency, "default", "md");

    assert_eq!(
        dependency.selected_components,
        Some(vec![DependencyComponent::Skills])
    );
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
    assert!(
        temp.path()
            .join(format!(".opencode/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(!runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_agent_file
    ));
    assert!(
        !temp
            .path()
            .join(format!(".opencode/agents/{managed_agent_file}"))
            .exists()
    );
    assert!(!runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_command_file
    ));
    assert!(
        !temp
            .path()
            .join(format!(".opencode/commands/{managed_command_file}"))
            .exists()
    );
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Codex,
        &managed_skill_id
    ));
    assert!(!runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_claude_rule_file
    ));
    assert!(
        !temp
            .path()
            .join(format!(".opencode/rules/{managed_claude_rule_file}"))
            .exists()
    );

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let shared = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    assert_eq!(
        shared.selected_components,
        Some(vec![DependencyComponent::Skills])
    );
    let plugin_root = global_native_plugin_root(temp.path(), dependency, Adapter::Claude);
    let plugin_root_relative = display_path(plugin_root.strip_prefix(temp.path()).unwrap());
    assert_not_owned(&lockfile, temp.path(), &plugin_root_relative);
    assert_not_owned(&lockfile, temp.path(), ".claude/agents/shared.md");
}

#[test]
fn sync_detects_existing_codex_root_and_persists_only_codex() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    fs::create_dir_all(temp.path().join(".codex")).unwrap();

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let manifest = load_root_from_dir(temp.path()).unwrap();
    assert_eq!(
        manifest.manifest.enabled_adapters().unwrap(),
        [Adapter::Codex].as_slice()
    );
    assert!(!temp.path().join(".codex/skills").exists());
    assert!(!temp.path().join(".claude/skills").exists());
    assert!(!temp.path().join(".opencode/skills").exists());
}

#[test]
fn sync_does_not_publish_root_assets_by_default() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(&temp.path().join("rules/default.rules"), "allow = []\n");

    sync_all(temp.path(), cache.path());

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let root_package = resolution
        .packages
        .iter()
        .find(|package| matches!(package.source, PackageSource::Root))
        .unwrap();
    let managed_skill_id = namespaced_skill_id(root_package, "review");
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();

    assert!(!runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
    assert!(!runtime_skill_exists(
        temp.path(),
        Adapter::Codex,
        &managed_skill_id
    ));
    assert_not_owned(&lockfile, temp.path(), ".claude/skills/review");
    assert_not_owned(&lockfile, temp.path(), ".codex/skills/review");
}

#[test]
fn sync_publishes_root_assets_when_enabled() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
publish_root = true
"#,
    );
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(&temp.path().join("rules/default.rules"), "allow = []\n");

    sync_all(temp.path(), cache.path());

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let root_package = resolution
        .packages
        .iter()
        .find(|package| matches!(package.source, PackageSource::Root))
        .unwrap();
    let managed_skill_id = namespaced_skill_id(root_package, "review");
    let managed_claude_rule_file = namespaced_file_name(root_package, "default", "md");
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();

    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
    assert!(
        temp.path()
            .join(format!(".cursor/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_claude_rule_file
    ));
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Codex,
        &managed_skill_id
    ));
    assert_owned(&lockfile, temp.path(), ".claude/skills");
    assert_owned(
        &lockfile,
        temp.path(),
        &format!(".codex/skills/{managed_skill_id}"),
    );
}

#[test]
fn sync_publishes_root_codex_agents_without_redundant_owned_files() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
publish_root = true
"#,
    );
    write_codex_agent_toml(
        &temp.path().join("agents/security.toml"),
        "security",
        "Security reviewer",
        "Audit the code.",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    assert!(temp.path().join(".codex/agents/security.toml").exists());
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let root = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "root")
        .unwrap();
    assert!(
        root.owned_subtrees
            .iter()
            .any(|path| path == ".codex/agents")
            || root
                .owned_files
                .iter()
                .any(|path| path == ".codex/agents/security.toml"),
        "root package should own its project-local Codex agent output; files={:?} subtrees={:?}",
        root.owned_files,
        root.owned_subtrees
    );
    assert_owned(&lockfile, temp.path(), ".codex/agents/security.toml");
}

#[test]
fn sync_writes_runtime_gitignores_for_managed_outputs() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/rules/default.rules"),
        "allow = []\n",
    );
    write_file(
        &temp.path().join("vendor/shared/commands/build.txt"),
        "cargo test\n",
    );

    sync_all(temp.path(), cache.path());

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    let managed_command_file = namespaced_file_name(dependency, "build", "md");
    let agents_gitignore = fs::read_to_string(temp.path().join(".agents/.gitignore")).unwrap();
    let cursor_gitignore = fs::read_to_string(temp.path().join(".cursor/.gitignore")).unwrap();

    assert!(agents_gitignore.contains("# Managed by nodus"));
    assert!(agents_gitignore.contains(".gitignore"));
    assert!(agents_gitignore.contains(&format!("skills/{managed_skill_id}")));
    assert!(agents_gitignore.contains(&format!("commands/{managed_command_file}")));
    assert!(
        !temp.path().join(".codex/.gitignore").exists(),
        "Codex dependency skills now live in the global snapshot marketplace"
    );
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Codex,
        &managed_skill_id
    ));
    assert!(cursor_gitignore.contains("# Managed by nodus"));
    assert!(cursor_gitignore.contains(".gitignore"));
    assert!(cursor_gitignore.contains(&format!("skills/{managed_skill_id}")));
    assert!(cursor_gitignore.contains(&format!("commands/{managed_command_file}")));
    assert!(cursor_gitignore.contains("rules/default.mdc"));
}

#[test]
fn sync_emits_codex_command_compatibility_skills_for_command_components() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared", components = ["commands"] }
"#,
    );
    write_file(
        &temp.path().join("vendor/shared/commands/build.txt"),
        "cargo test\n",
    );

    let summary =
        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex])
            .unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_codex_command_skill =
        resolution_codex_command_skill_id(&resolution, dependency, "build");
    let plugin_root = global_native_plugin_root(temp.path(), dependency, Adapter::Codex);

    assert!(
        plugin_root
            .join(format!("skills/{managed_codex_command_skill}/SKILL.md"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(
                ".codex/skills/{managed_codex_command_skill}/SKILL.md"
            ))
            .exists()
    );
    assert_eq!(summary.managed_file_count, 2);

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let shared = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    assert_eq!(
        shared.selected_components,
        Some(vec![DependencyComponent::Commands])
    );
    assert_eq!(shared.commands, vec!["build"]);
    assert_owned(&lockfile, temp.path(), &display_path(&plugin_root));
    assert_not_owned(&lockfile, temp.path(), ".codex/config.toml");
}

#[test]
fn sync_force_overwrites_unmanaged_runtime_skill_output() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(temp.path(), "");
    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let blocking_path = managed_skill_file(temp.path(), Adapter::Codex, dependency, "review")
        .parent()
        .unwrap()
        .to_path_buf();
    write_file(&blocking_path, "user-owned blocking file\n");

    let error =
        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex])
            .unwrap_err()
            .to_string();
    assert!(error.contains("refusing to overwrite unmanaged file"));
    assert!(error.contains("packages") && error.contains("codex-plugin"));

    sync_in_dir_with_adapters_force(temp.path(), cache.path(), false, false, &[Adapter::Codex])
        .unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    let skill = fs::read_to_string(runtime_skill_path(
        temp.path(),
        Adapter::Codex,
        &managed_skill_id,
    ))
    .unwrap();
    assert!(skill.contains("# Review"));
}

#[test]
fn sync_adopts_exact_unmanaged_runtime_skill_output() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(temp.path(), "");
    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let package_roots = resolution
        .packages
        .iter()
        .map(|package| (package.clone(), package.root.clone()))
        .collect::<Vec<_>>();
    let output_plan =
        build_output_plan(temp.path(), &package_roots, Adapters::CODEX, None, false).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_path = managed_skill_file(temp.path(), Adapter::Codex, dependency, "review");
    let managed_skill_contents = output_plan
        .files
        .iter()
        .find(|file| file.path == managed_skill_path)
        .unwrap()
        .contents
        .clone();
    fs::create_dir_all(managed_skill_path.parent().unwrap()).unwrap();
    fs::write(&managed_skill_path, managed_skill_contents).unwrap();

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert!(managed_skill_path.exists());
    assert_owned(
        &lockfile,
        temp.path(),
        &display_path(managed_skill_path.parent().unwrap()),
    );
}

#[test]
fn sync_can_adopt_unmanaged_runtime_command_output() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &Adapter::ALL).unwrap();

    write_file(
        &temp.path().join("vendor/shared/commands/build.txt"),
        "cargo test\n",
    );

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), dependency, Adapter::Claude);
    let managed_command_path = managed_artifact_file(
        &plugin_root,
        Adapter::Claude,
        ArtifactKind::Command,
        dependency,
        "build",
    );
    write_file(&managed_command_path, "user-owned command\n");

    sync_in_dir_with_collision_choice(temp.path(), cache.path(), ManagedCollisionChoice::Adopt)
        .unwrap();

    assert_eq!(
        fs::read_to_string(&managed_command_path).unwrap(),
        "cargo test\n"
    );
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let plugin_root_relative = display_path(plugin_root.strip_prefix(temp.path()).unwrap());
    assert_not_owned(&lockfile, temp.path(), &plugin_root_relative);
}

#[test]
fn sync_overwrites_owned_native_plugin_command_output_without_prompt() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &Adapter::ALL).unwrap();

    write_file(
        &temp.path().join("vendor/shared/commands/build.txt"),
        "cargo test\n",
    );

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), dependency, Adapter::Claude);
    let managed_command_path = managed_artifact_file(
        &plugin_root,
        Adapter::Claude,
        ArtifactKind::Command,
        dependency,
        "build",
    );
    write_file(&managed_command_path, "user-owned command\n");

    sync_in_dir_with_collision_choice(temp.path(), cache.path(), ManagedCollisionChoice::Cancel)
        .unwrap();

    assert_eq!(
        fs::read_to_string(&managed_command_path).unwrap(),
        "cargo test\n"
    );
}

#[test]
fn sync_adopts_branch_tracked_dependency_collisions_without_prompting() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let dep = TempDir::new().unwrap();
    write_manifest(dep.path(), r#"name = "shared""#);
    write_skill(&dep.path().join("skills/review"), "Review");
    write_file(
        &dep.path().join("prompts/review.md"),
        "Use the review prompt.\n",
    );
    init_git_repo(dep.path());
    rename_current_branch(dep.path(), "main");

    // Consumer tracks the dependency on its moving `main` branch and maps one
    // of its files into the repo.
    write_manifest(
        temp.path(),
        &format!(
            r#"
[dependencies.shared]
url = "{url}"
branch = "main"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
            url = toml_path_value(dep.path()),
        ),
    );

    // A user file already occupies the managed target, which would normally
    // force the collision prompt.
    write_file(
        &temp.path().join(".github/prompts/review.md"),
        "user-owned prompt\n",
    );

    // A stable pin would consult the resolver and cancel here. A branch pin is
    // intentionally unstable, so sync adopts the upstream state silently and
    // never consults the resolver.
    sync_in_dir_with_collision_choice(temp.path(), cache.path(), ManagedCollisionChoice::Cancel)
        .unwrap();

    assert_eq!(
        fs::read_to_string(temp.path().join(".github/prompts/review.md")).unwrap(),
        "Use the review prompt.\n"
    );
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert_owned(&lockfile, temp.path(), ".github/prompts/review.md");
}

#[test]
fn sync_still_prompts_for_non_branch_dependency_collisions() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let dep = TempDir::new().unwrap();
    write_manifest(dep.path(), r#"name = "shared""#);
    write_skill(&dep.path().join("skills/review"), "Review");
    write_file(
        &dep.path().join("prompts/review.md"),
        "Use the review prompt.\n",
    );
    init_git_repo(dep.path());
    run_git(dep.path(), &["tag", "v0.1.0"]);

    // The same dependency pinned to a fixed tag instead of a branch: the user
    // never opted into "always take upstream", so the collision must still
    // prompt (and cancel here) rather than silently overwrite their file.
    write_manifest(
        temp.path(),
        &format!(
            r#"
[dependencies.shared]
url = "{url}"
tag = "v0.1.0"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
            url = toml_path_value(dep.path()),
        ),
    );
    write_file(
        &temp.path().join(".github/prompts/review.md"),
        "user-owned prompt\n",
    );

    let error = sync_in_dir_with_collision_choice(
        temp.path(),
        cache.path(),
        ManagedCollisionChoice::Cancel,
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("cancelled `nodus sync`"));
    assert!(error.contains(".github/prompts/review.md"));
    assert_eq!(
        fs::read_to_string(temp.path().join(".github/prompts/review.md")).unwrap(),
        "user-owned prompt\n"
    );
}

#[test]
fn sync_dry_run_force_previews_without_overwriting_unmanaged_files() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(temp.path(), "");
    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    let blocking_path = temp
        .path()
        .join(".nodus/packages/shared/codex-plugin/skills");
    write_file(&blocking_path, "user-owned blocking file\n");

    sync_in_dir_with_adapters_dry_run_force(
        temp.path(),
        cache.path(),
        false,
        false,
        &[Adapter::Codex],
    )
    .unwrap();

    assert_eq!(
        fs::read_to_string(&blocking_path).unwrap(),
        "user-owned blocking file\n"
    );
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    // v10: the plugin subtree itself is owned, but no per-skill owned_file or
    // owned_subtree path should appear under it (the user-blocking file
    // prevented the plugin emission for this dependency).
    assert!(!lockfile.packages.iter().any(|package| {
        package
            .owned_subtrees
            .iter()
            .any(|path| path.starts_with(".nodus/packages/shared/codex-plugin/skills/"))
            || package
                .owned_files
                .iter()
                .any(|path| path.starts_with(".nodus/packages/shared/codex-plugin/skills/"))
    }));
}

#[test]
fn sync_merges_direct_managed_runtime_root_gitignore_with_generated_outputs() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "config/.gitignore"
target = ".cursor/.gitignore"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/config/.gitignore"),
        ".DS_Store\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Cursor]).unwrap();
    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Cursor]).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    let gitignore = fs::read_to_string(temp.path().join(".cursor/.gitignore")).unwrap();
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();

    assert!(gitignore.contains("# Managed by nodus"));
    assert!(gitignore.contains(".gitignore"));
    assert!(gitignore.contains(".DS_Store"));
    assert!(gitignore.contains(&format!("skills/{managed_skill_id}")));
    assert_owned(&lockfile, temp.path(), ".cursor/.gitignore");
}

#[test]
fn sync_merges_existing_unmanaged_runtime_root_gitignore() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join(".cursor/.gitignore"),
        ".gitignore\n# custom\nskills/*_legacy/\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Cursor]).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    let gitignore = fs::read_to_string(temp.path().join(".cursor/.gitignore")).unwrap();
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();

    assert!(gitignore.starts_with("# Managed by nodus\n.gitignore\n"));
    assert!(gitignore.contains("# custom"));
    assert!(gitignore.contains("skills/*_legacy/"));
    assert!(gitignore.contains(&format!("skills/{managed_skill_id}")));
    assert_owned(&lockfile, temp.path(), ".cursor/.gitignore");
}

#[test]
fn sync_emits_mcp_json_from_dependency_manifests() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.firebase]
path = "vendor/firebase"
"#,
    );
    write_file(
        &temp.path().join("vendor/firebase/nodus.toml"),
        r#"
[mcp_servers.firebase]
command = "npx"
args = ["-y", "firebase-tools", "mcp", "--dir", "."]

[mcp_servers.firebase.env]
IS_FIREBASE_MCP = "true"
"#,
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    let mcp_config = fs::read_to_string(temp.path().join(".mcp.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&mcp_config).unwrap();
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let firebase_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "firebase")
        .unwrap();

    assert_eq!(firebase_package.mcp_servers, vec!["firebase"]);
    assert_owned(&lockfile, temp.path(), ".mcp.json");
    assert_eq!(
        json["mcpServers"]["firebase__firebase"]["command"].as_str(),
        Some("npx")
    );
    assert_eq!(
        json["mcpServers"]["firebase__firebase"]["env"]["IS_FIREBASE_MCP"].as_str(),
        Some("true")
    );
}

#[test]
fn sync_prunes_project_mcp_when_claude_native_plugin_owns_mcp_servers() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.firebase]
path = "vendor/firebase"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/firebase"),
        r#"
[mcp_servers.firebase]
command = "npx"
args = ["-y", "firebase-tools", "mcp", "--dir", "."]
"#,
    );
    write_skill(
        &temp.path().join("vendor/firebase/skills/firebase-review"),
        "Firebase Review",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();
    assert!(temp.path().join(".mcp.json").exists());

    sync_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        false,
        false,
        &[Adapter::Claude, Adapter::Codex],
    )
    .unwrap();

    assert!(
        !temp.path().join(".mcp.json").exists(),
        "Claude native plugin MCP should replace the stale project-level MCP file"
    );
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let firebase = resolution
        .packages
        .iter()
        .find(|package| package.alias == "firebase")
        .unwrap();
    let claude_plugin_root = global_native_plugin_root(temp.path(), firebase, Adapter::Claude);
    assert!(claude_plugin_root.join(".mcp.json").exists());
    let plugin: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(claude_plugin_root.join(".claude-plugin/plugin.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(plugin["mcpServers"].as_str(), Some("./.mcp.json"));
    assert!(
        !temp
            .path()
            .join(".nodus/packages/firebase/codex-plugin/.mcp.json")
            .exists()
    );
    let codex_plugin_root = global_native_plugin_root(temp.path(), firebase, Adapter::Codex);
    assert!(codex_plugin_root.join(".mcp.json").exists());
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert_not_owned(&lockfile, temp.path(), ".mcp.json");
}

#[test]
fn sync_omits_mcp_outputs_when_mcp_component_is_not_selected() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.firebase]
path = "vendor/firebase"
components = ["skills", "agents", "rules", "commands"]
"#,
    );
    write_file(
        &temp.path().join("vendor/firebase/nodus.toml"),
        r#"
[mcp_servers.firebase]
command = "npx"
args = ["-y", "firebase-tools", "mcp", "--dir", "."]
"#,
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let firebase_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "firebase")
        .unwrap();

    assert_eq!(firebase_package.mcp_servers, Vec::<String>::new());
    assert_eq!(
        firebase_package.selected_components,
        Some(vec![
            DependencyComponent::Skills,
            DependencyComponent::Agents,
            DependencyComponent::Rules,
            DependencyComponent::Commands
        ])
    );
    assert_not_owned(&lockfile, temp.path(), ".mcp.json");
    assert_not_owned(&lockfile, temp.path(), ".codex/config.toml");
    assert!(!temp.path().join(".mcp.json").exists());
    assert!(!temp.path().join(".codex/config.toml").exists());
}

#[test]
fn sync_emits_codex_project_mcp_from_dependency_manifests() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.firebase]
path = "vendor/firebase"
"#,
    );
    write_file(
        &temp.path().join("vendor/firebase/nodus.toml"),
        r#"
[mcp_servers.firebase]
command = "npx"
args = ["-y", "firebase-tools", "mcp", "--dir", "."]
cwd = "."

[mcp_servers.firebase.env]
IS_FIREBASE_MCP = "true"

[mcp_servers.figma]
url = "https://mcp.figma.com/mcp"

[mcp_servers.figma.headers]
Authorization = "Bearer ${FIGMA_TOKEN}"
X-Figma-Region = "us-east-1"
"#,
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let firebase_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "firebase")
        .unwrap();

    assert_eq!(firebase_package.mcp_servers, vec!["figma", "firebase"]);
    assert_not_owned(&lockfile, temp.path(), ".codex/config.toml");
    assert!(!temp.path().join(".codex/config.toml").exists());
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let firebase = resolution
        .packages
        .iter()
        .find(|package| package.alias == "firebase")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), firebase, Adapter::Codex);
    assert!(plugin_root.join(".mcp.json").exists());
    assert_owned(&lockfile, temp.path(), &display_path(&plugin_root));
    assert_codex_user_config_registers_plugins(temp.path(), &["firebase@nodus"]);
    let config: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(plugin_root.join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(
        config["mcpServers"]["firebase"]["command"].as_str(),
        Some("npx")
    );
    assert_eq!(
        config["mcpServers"]["firebase"]["args"][0].as_str(),
        Some("-y")
    );
    assert_eq!(config["mcpServers"]["firebase"]["cwd"].as_str(), Some("."));
    assert_eq!(
        config["mcpServers"]["firebase"]["env"]["IS_FIREBASE_MCP"].as_str(),
        Some("true")
    );
    assert_eq!(
        config["mcpServers"]["figma"]["url"].as_str(),
        Some("https://mcp.figma.com/mcp")
    );
    assert_eq!(
        config["mcpServers"]["figma"]["headers"]["X-Figma-Region"].as_str(),
        Some("us-east-1")
    );
}

#[test]
fn sync_emits_codex_mcp_through_project_config() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
name = "Yoki iOS"

[dependencies.firebase]
path = "vendor/firebase"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/firebase"),
        r#"
[mcp_servers.firebase]
command = "npx"
args = ["-y", "firebase-tools", "mcp", "--dir", "."]
"#,
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let firebase = resolution
        .packages
        .iter()
        .find(|package| package.alias == "firebase")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), firebase, Adapter::Codex);
    assert!(plugin_root.join(".mcp.json").exists());
    assert!(generated_codex_marketplace_path(temp.path()).exists());
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert_not_owned(&lockfile, temp.path(), ".codex/config.toml");
    assert!(!temp.path().join(".codex/config.toml").exists());
    assert_owned(&lockfile, temp.path(), &display_path(&plugin_root));
    assert_codex_user_config_registers_plugins(temp.path(), &["firebase@nodus"]);
    let codex_config: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(plugin_root.join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(
        codex_config["mcpServers"]["firebase"]["command"].as_str(),
        Some("npx")
    );
}

#[test]
fn sync_emits_opencode_json_from_dependency_manifests() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.firebase]
path = "vendor/firebase"
"#,
    );
    write_file(
        &temp.path().join("vendor/firebase/nodus.toml"),
        r#"
[mcp_servers.firebase]
command = "npx"
args = ["-y", "firebase-tools", "mcp", "--dir", "."]

[mcp_servers.firebase.env]
IS_FIREBASE_MCP = "true"

[mcp_servers.figma]
url = "https://mcp.figma.com/mcp"

[mcp_servers.figma.headers]
Authorization = "Bearer ${FIGMA_TOKEN}"
X-Figma-Region = "us-east-1"
"#,
    );

    sync_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        false,
        false,
        &[Adapter::OpenCode],
    )
    .unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join("opencode.json")).unwrap())
            .unwrap();
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let firebase_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "firebase")
        .unwrap();

    assert_eq!(firebase_package.mcp_servers, vec!["figma", "firebase"]);
    assert_owned(&lockfile, temp.path(), "opencode.json");
    assert_eq!(
        json["mcp"]["firebase__firebase"]["type"].as_str(),
        Some("local")
    );
    assert_eq!(
        json["mcp"]["firebase__firebase"]["command"],
        serde_json::json!(["npx", "-y", "firebase-tools", "mcp", "--dir", "."])
    );
    assert_eq!(
        json["mcp"]["firebase__firebase"]["environment"]["IS_FIREBASE_MCP"].as_str(),
        Some("true")
    );
    assert_eq!(
        json["mcp"]["firebase__figma"]["type"].as_str(),
        Some("remote")
    );
    assert_eq!(
        json["mcp"]["firebase__figma"]["url"].as_str(),
        Some("https://mcp.figma.com/mcp")
    );
    assert_eq!(
        json["mcp"]["firebase__figma"]["headers"]["Authorization"].as_str(),
        Some("Bearer {env:FIGMA_TOKEN}")
    );
    assert_eq!(
        json["mcp"]["firebase__figma"]["headers"]["X-Figma-Region"].as_str(),
        Some("us-east-1")
    );
}

#[test]
fn sync_emits_url_backed_mcp_servers() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.figma]
path = "vendor/figma"
"#,
    );
    write_file(
        &temp.path().join("vendor/figma/nodus.toml"),
        r#"
[mcp_servers.figma]
url = "http://127.0.0.1:3845/mcp"
"#,
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(
        json["mcpServers"]["figma__figma"]["url"].as_str(),
        Some("http://127.0.0.1:3845/mcp")
    );
    assert!(json["mcpServers"]["figma__figma"]["command"].is_null());
}

#[test]
fn sync_omits_disabled_mcp_servers() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.xcode]
path = "vendor/xcode"
"#,
    );
    write_file(
        &temp.path().join("vendor/xcode/nodus.toml"),
        r#"
[mcp_servers.xcode]
command = "xcrun"
args = ["mcpbridge"]
enabled = false
"#,
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    let mcp_path = temp.path().join(".mcp.json");
    assert!(
        mcp_path.exists(),
        ".mcp.json should exist (nodus server auto-registered)"
    );
    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&mcp_path).unwrap()).unwrap();
    assert!(
        json["mcpServers"].get("xcode__xcode").is_none(),
        "disabled xcode server should not be in .mcp.json"
    );
    assert!(
        json["mcpServers"].get("nodus").is_some(),
        "nodus server should be auto-registered"
    );
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let xcode_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "xcode")
        .unwrap();
    assert_eq!(xcode_package.mcp_servers, vec!["xcode"]);
    assert_owned(&lockfile, temp.path(), ".mcp.json");
}

#[test]
fn sync_merges_unmanaged_mcp_entries_with_managed_outputs() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.firebase]
path = "vendor/firebase"
"#,
    );
    write_file(
        &temp.path().join("vendor/firebase/nodus.toml"),
        r#"
[mcp_servers.firebase]
command = "npx"
"#,
    );
    write_file(
        &temp.path().join(".mcp.json"),
        r#"{
  "mcpServers": {
    "local": {
      "command": "node"
    }
  }
}
"#,
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(
        json["mcpServers"]["local"]["command"].as_str(),
        Some("node")
    );
    assert_eq!(
        json["mcpServers"]["firebase__firebase"]["command"].as_str(),
        Some("npx")
    );
}

#[test]
fn sync_prunes_stale_managed_mcp_entries_without_touching_unmanaged_ones() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.firebase]
path = "vendor/firebase"
"#,
    );
    write_file(
        &temp.path().join("vendor/firebase/nodus.toml"),
        r#"
[mcp_servers.firebase]
command = "npx"
"#,
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();
    write_file(
        &temp.path().join(".mcp.json"),
        r#"{
  "mcpServers": {
    "firebase__firebase": {
      "command": "npx"
    },
    "nodus": {
      "command": "nodus",
      "args": ["mcp", "serve"]
    },
    "local": {
      "command": "node"
    }
  }
}
"#,
    );
    write_manifest(temp.path(), "");

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    let mcp_path = temp.path().join(".mcp.json");
    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&mcp_path).unwrap()).unwrap();
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();

    assert!(json["mcpServers"].get("firebase__firebase").is_none());
    assert_eq!(
        json["mcpServers"]["local"]["command"].as_str(),
        Some("node")
    );
    assert!(json["mcpServers"].get("nodus").is_none());
    assert_not_owned(&lockfile, temp.path(), ".mcp.json");
}

#[test]
fn doctor_rejects_invalid_managed_mcp_json() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.firebase]
path = "vendor/firebase"
"#,
    );
    write_file(
        &temp.path().join("vendor/firebase/nodus.toml"),
        r#"
[mcp_servers.firebase]
command = "npx"
"#,
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();
    write_file(&temp.path().join(".mcp.json"), "{");

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Check,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Blocked);
    assert!(summary.applied_actions.is_empty());
    assert!(summary.findings.iter().any(|finding| {
        finding.kind == DoctorFindingKind::SafeAutoFix
            && finding
                .message
                .contains("managed outputs drifted from the declared project state")
    }));
    assert_eq!(
        fs::read_to_string(temp.path().join(".mcp.json")).unwrap(),
        "{"
    );
}

#[test]
fn sync_recreates_missing_lockfile_for_existing_runtime_outputs() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_file(
        &temp.path().join("vendor/shared/rules/default.rules"),
        "allow = []\n",
    );

    sync_all(temp.path(), cache.path());
    fs::remove_file(temp.path().join(LOCKFILE_NAME)).unwrap();

    sync_all(temp.path(), cache.path());

    assert!(temp.path().join(LOCKFILE_NAME).exists());
}

#[test]
fn sync_upgrades_legacy_lockfile_and_prunes_legacy_runtime_outputs() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/agents/security.md"),
        "# Security\n",
    );
    write_file(
        &temp.path().join("vendor/shared/commands/build.txt"),
        "cargo test\n",
    );

    sync_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        false,
        false,
        &[Adapter::Claude, Adapter::OpenCode],
    )
    .unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    let managed_agent_file = namespaced_file_name(dependency, "security", "md");
    let managed_command_file = namespaced_file_name(dependency, "build", "md");
    let legacy_skill_id = crate::adapters::namespaced_skill_id(dependency, "review");
    let legacy_agent_file = crate::adapters::namespaced_file_name(dependency, "security", "md");
    let legacy_command_file = crate::adapters::namespaced_file_name(dependency, "build", "md");

    let managed_claude_agent = managed_artifact_file(
        temp.path(),
        Adapter::Claude,
        ArtifactKind::Agent,
        dependency,
        "security",
    );
    let legacy_claude_agent = temp
        .path()
        .join(format!(".claude/agents/{legacy_agent_file}"));
    fs::create_dir_all(legacy_claude_agent.parent().unwrap()).unwrap();
    fs::rename(&managed_claude_agent, &legacy_claude_agent).unwrap();
    let managed_claude_command = managed_artifact_file(
        temp.path(),
        Adapter::Claude,
        ArtifactKind::Command,
        dependency,
        "build",
    );
    let legacy_claude_command = temp
        .path()
        .join(format!(".claude/commands/{legacy_command_file}"));
    fs::create_dir_all(legacy_claude_command.parent().unwrap()).unwrap();
    fs::rename(&managed_claude_command, &legacy_claude_command).unwrap();
    fs::rename(
        temp.path()
            .join(format!(".opencode/agents/{managed_agent_file}")),
        temp.path()
            .join(format!(".opencode/agents/{legacy_agent_file}")),
    )
    .unwrap();
    fs::rename(
        temp.path()
            .join(format!(".opencode/commands/{managed_command_file}")),
        temp.path()
            .join(format!(".opencode/commands/{legacy_command_file}")),
    )
    .unwrap();
    fs::rename(
        temp.path()
            .join(format!(".opencode/skills/{managed_skill_id}")),
        temp.path()
            .join(format!(".opencode/skills/{legacy_skill_id}")),
    )
    .unwrap();

    let current_lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    // Simulate a v8 lockfile on disk by synthesizing the `legacy_managed_files`
    // entries the user's pre-v10 lockfile would have carried (v10 writes drop
    // the field, but a real-world user upgrading from v8 has these paths in
    // their lockfile and the sync upgrade path needs them to know which
    // legacy-named outputs to clean up).
    let mut legacy_paths = vec![
        format!(".claude/agents/{legacy_agent_file}"),
        format!(".claude/commands/{legacy_command_file}"),
        format!(".opencode/agents/{legacy_agent_file}"),
        format!(".opencode/commands/{legacy_command_file}"),
        format!(".opencode/skills/{legacy_skill_id}"),
    ];
    legacy_paths.sort();
    Lockfile {
        version: 8,
        codex_profile: None,
        packages: current_lockfile.packages,
        legacy_managed_files: legacy_paths,
    }
    .write(&temp.path().join(LOCKFILE_NAME))
    .unwrap();

    sync_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        false,
        false,
        &[Adapter::Claude, Adapter::OpenCode],
    )
    .unwrap();

    let upgraded_lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();

    assert_eq!(upgraded_lockfile.version, Lockfile::current_version());
    assert!(managed_claude_agent.exists());
    assert!(managed_claude_command.exists());
    assert!(
        temp.path()
            .join(format!(".opencode/agents/{managed_agent_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/commands/{managed_command_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/skills/{managed_skill_id}"))
            .exists()
    );
    assert!(!legacy_claude_agent.exists());
    assert!(!legacy_claude_command.exists());
    assert!(
        !temp
            .path()
            .join(format!(".opencode/agents/{legacy_agent_file}"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(".opencode/commands/{legacy_command_file}"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(".opencode/skills/{legacy_skill_id}"))
            .exists()
    );
}

#[test]
fn sync_detects_multiple_adapter_roots_and_persists_them() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    fs::create_dir_all(temp.path().join(".claude")).unwrap();
    fs::create_dir_all(temp.path().join(".opencode")).unwrap();

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let manifest = load_root_from_dir(temp.path()).unwrap();
    assert_eq!(
        manifest.manifest.enabled_adapters().unwrap(),
        [Adapter::Claude, Adapter::OpenCode].as_slice()
    );
    assert!(!temp.path().join(".claude/skills").exists());
    assert!(!temp.path().join(".codex/skills").exists());
    assert!(!temp.path().join(".opencode/skills").exists());
}

#[test]
fn sync_persists_explicit_adapter_selection_when_repo_has_no_roots() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    let manifest = load_root_from_dir(temp.path()).unwrap();
    assert_eq!(
        manifest.manifest.enabled_adapters().unwrap(),
        [Adapter::Codex].as_slice()
    );
    assert!(!temp.path().join(".codex/skills").exists());
    assert!(!temp.path().join(".claude/skills").exists());
    assert!(!temp.path().join(".opencode/skills").exists());
}

#[test]
fn sync_persists_launch_hook_configuration() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");

    let reporter = Reporter::silent();
    super::sync_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        false,
        false,
        false,
        &[Adapter::Codex],
        true,
        &reporter,
    )
    .unwrap();

    let manifest = load_root_from_dir(temp.path()).unwrap();
    assert!(manifest.manifest.sync_on_launch_enabled());
}

#[test]
fn sync_migrates_legacy_launch_hook_config_to_hooks() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["codex"]

[launch_hooks]
sync_on_startup = true
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
    assert!(!manifest.contains("[launch_hooks]"));
    assert!(manifest.contains("[[hooks]]"));
    assert!(manifest.contains("id = \"nodus.sync_on_startup\""));
    assert!(manifest.contains("event = \"session_start\""));
}

#[test]
fn sync_emits_activation_session_start_hooks_for_claude_and_codex() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["claude", "codex"]

[dependencies.shared]
path = "vendor/shared"
components = ["skills"]
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
[activation]
always_context = ["prompts/first-principles.md"]
prefer_skills = ["review"]
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp
            .path()
            .join("vendor/shared/prompts/first-principles.md"),
        "Reason from facts before patterns.\n",
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let claude_plugin_root = global_native_plugin_root(temp.path(), shared, Adapter::Claude);
    let codex_plugin_root = global_native_plugin_root(temp.path(), shared, Adapter::Codex);

    // Claude routes the dependency's activation context through the plugin's
    // `hooks/hooks.json` rather than the workspace settings file.
    let claude_plugin_hooks: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(claude_plugin_root.join("hooks/hooks.json")).unwrap(),
    )
    .unwrap();
    let claude_session_start = claude_plugin_hooks["hooks"]["SessionStart"]
        .as_array()
        .unwrap();
    assert_eq!(claude_session_start.len(), 1);
    assert_eq!(
        claude_session_start[0]["matcher"].as_str(),
        Some("startup|resume"),
    );
    assert!(
        claude_session_start[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("${CLAUDE_PLUGIN_ROOT}/hooks/scripts/nodus-hook-activation-")
    );

    // Workspace `.claude/settings.json` has no SessionStart entries from
    // dependency activation now that the plugin owns them.
    let claude_settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(temp.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert!(
        claude_settings["hooks"]["SessionStart"]
            .as_array()
            .is_none_or(|entries| entries.is_empty()),
        "workspace SessionStart should be empty, got {:?}",
        claude_settings["hooks"]["SessionStart"],
    );

    let codex_hooks: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(codex_plugin_root.join("hooks/hooks.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        codex_hooks["hooks"]["SessionStart"][0]["matcher"].as_str(),
        Some("startup|resume")
    );
    assert!(
        codex_hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("${PLUGIN_ROOT}/hooks/scripts/nodus-hook-activation-")
    );
    assert!(!temp.path().join(".codex/hooks.json").exists());
    assert!(codex_plugin_root.join(".codex-plugin/plugin.json").exists());
    let codex_config: toml::Value =
        toml::from_str(&fs::read_to_string(temp.path().join(".codex/config.toml")).unwrap())
            .unwrap();
    assert_eq!(
        codex_config
            .get("features")
            .and_then(toml::Value::as_table)
            .and_then(|features| features.get("hooks")),
        None
    );
    assert_eq!(
        codex_config["features"]["plugin_hooks"].as_bool(),
        Some(true)
    );

    let claude_script =
        fs::read_to_string(plugin_hook_script_path(temp.path(), "shared", "activation")).unwrap();
    let codex_script_path = fs::read_dir(codex_plugin_root.join("hooks/scripts"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("nodus-hook-activation-"))
        })
        .unwrap();
    let codex_script = fs::read_to_string(codex_script_path).unwrap();
    for context in [
        activation_context_from_script(&claude_script),
        activation_context_from_script(&codex_script),
    ] {
        assert!(context.contains("Nodus package `shared` startup context."));
        assert!(context.contains("--- Nodus activation file: prompts/first-principles.md ---"));
        assert!(context.contains("Reason from facts before patterns."));
        assert!(
            context.contains(
                "Prefer loading these Nodus-managed skills first when relevant: `review`."
            )
        );
        assert!(!context.contains("# Review"));
        assert!(!context.contains("description: Example skill."));
    }
}

#[test]
fn sync_prunes_activation_hooks_when_activation_is_removed() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["codex"]

[dependencies.shared]
path = "vendor/shared"
components = ["skills"]
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
[activation]
always_context = ["prompts/bootstrap.md"]
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/prompts/bootstrap.md"),
        "Bootstrap context.\n",
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), shared, Adapter::Codex);
    let plugin_hooks_json = plugin_root.join("hooks/hooks.json");
    let script = fs::read_dir(plugin_root.join("hooks/scripts"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("nodus-hook-activation-"))
        })
        .unwrap();
    assert!(script.exists());
    assert!(plugin_hooks_json.exists());
    assert!(!temp.path().join(".codex/hooks.json").exists());
    assert!(temp.path().join(".codex/config.toml").exists());
    let first_lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert_owned(&first_lockfile, temp.path(), ".codex/config.toml");

    // The path-dep freshness probe (`path_dep_source_is_newer`) decides drift
    // via file mtime. Many filesystems (HFS+, some Linux configs) round mtime
    // to 1-second granularity, so if the first sync's lockfile write and this
    // manifest rewrite land in the same wall-clock second, the probe sees
    // equal mtimes and treats the dep as unchanged — fast-path triggers and
    // the prune we're asserting never runs. Sleep past the granularity
    // boundary so the manifest's mtime is unambiguously after the lockfile's.
    // Storing a source-manifest digest in the lockfile would be the
    // production-correct fix (tracked as a Slice 5 review follow-up).
    std::thread::sleep(std::time::Duration::from_millis(1100));
    write_manifest(&temp.path().join("vendor/shared"), "");

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    assert!(!script.exists());
    assert!(!plugin_hooks_json.exists());
    assert!(!temp.path().join(".codex/config.toml").exists());
}

#[test]
fn sync_emits_startup_sync_files_for_supported_adapters() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["claude", "codex", "opencode"]

[launch_hooks]
sync_on_startup = true
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    assert!(temp.path().join(".claude/settings.json").exists());
    assert!(temp.path().join(".codex/hooks.json").exists());
    assert!(temp.path().join(".codex/config.toml").exists());
    assert!(
        temp.path()
            .join(".opencode/plugins/nodus-hooks.js")
            .exists()
    );
    assert_eq!(
        fs::read_dir(temp.path().join(".claude/hooks"))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        fs::read_dir(temp.path().join(".codex/hooks"))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        fs::read_dir(temp.path().join(".opencode/scripts"))
            .unwrap()
            .count(),
        1
    );

    let claude_settings = fs::read_to_string(temp.path().join(".claude/settings.json")).unwrap();
    let codex_hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".codex/hooks.json")).unwrap())
            .unwrap();
    let codex_config: toml::Value =
        toml::from_str(&fs::read_to_string(temp.path().join(".codex/config.toml")).unwrap())
            .unwrap();
    let opencode_plugin =
        fs::read_to_string(temp.path().join(".opencode/plugins/nodus-hooks.js")).unwrap();
    let codex_features = codex_config["features"].as_table().unwrap();

    assert!(claude_settings.contains("\"SessionStart\""));
    assert!(claude_settings.contains("\"startup|resume\""));
    assert_eq!(codex_config["features"]["hooks"].as_bool(), Some(true));
    assert!(!codex_features.contains_key("plugin_hooks"));
    assert!(!codex_features.contains_key("codex_hooks"));
    assert_eq!(
        codex_hooks["hooks"]["SessionStart"][0]["matcher"].as_str(),
        Some("startup|resume")
    );
    assert_eq!(
        codex_hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str(),
        Some(
            codex_hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
        )
    );
    assert!(
        codex_hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains(".codex/hooks/nodus-hook-")
    );
    assert!(opencode_plugin.contains(".opencode/scripts/nodus-hook-"));
}

#[test]
fn sync_replaces_deprecated_codex_hooks_feature_when_emitting_hooks() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["codex"]

[launch_hooks]
sync_on_startup = true
"#,
    );
    write_file(
        &temp.path().join(".codex/config.toml"),
        r#"
[features]
codex_hooks = true
preserved_feature = true
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let codex_config: toml::Value =
        toml::from_str(&fs::read_to_string(temp.path().join(".codex/config.toml")).unwrap())
            .unwrap();
    let codex_features = codex_config["features"].as_table().unwrap();
    assert_eq!(
        codex_config["features"]["preserved_feature"].as_bool(),
        Some(true)
    );
    assert_eq!(codex_config["features"]["hooks"].as_bool(), Some(true));
    assert!(!codex_features.contains_key("codex_hooks"));
}

#[test]
fn sync_deduplicates_startup_sync_hook_across_root_and_dependency_packages() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["claude", "codex", "opencode"]

[launch_hooks]
sync_on_startup = true

[dependencies.ena]
path = "vendor/ena"

[dependencies.fuli]
path = "vendor/fuli"
"#,
    );
    write_file(
        &temp.path().join("vendor/ena/nodus.toml"),
        r#"
[[hooks]]
id = "nodus.sync_on_startup"
event = "session_start"

[hooks.matcher]
sources = ["startup", "resume"]

[hooks.handler]
type = "command"
command = "nodus sync"
"#,
    );
    write_file(
        &temp.path().join("vendor/fuli/nodus.toml"),
        r#"
[[hooks]]
id = "nodus.sync_on_startup"
event = "session_start"

[hooks.matcher]
sources = ["startup", "resume"]

[hooks.handler]
type = "command"
command = "nodus sync"
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let claude_hooks = temp.path().join(".claude/hooks");
    let claude_hook_files = fs::read_dir(&claude_hooks)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(claude_hook_files.len(), 1);
    assert!(
        claude_hook_files[0].starts_with("nodus-hook-nodus-sync-on-startup-"),
        "unexpected Claude hook files: {claude_hook_files:?}"
    );

    let codex_hooks = temp.path().join(".codex/hooks");
    let codex_hook_files = fs::read_dir(&codex_hooks)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(codex_hook_files.len(), 1);
    assert!(
        codex_hook_files[0].starts_with("nodus-hook-nodus-sync-on-startup-"),
        "unexpected Codex hook files: {codex_hook_files:?}"
    );

    let opencode_scripts = temp.path().join(".opencode/scripts");
    let opencode_hook_files = fs::read_dir(&opencode_scripts)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(opencode_hook_files.len(), 1);
    assert!(
        opencode_hook_files[0].starts_with("nodus-hook-nodus-sync-on-startup-"),
        "unexpected OpenCode hook files: {opencode_hook_files:?}"
    );

    let claude_settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(temp.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        claude_settings["hooks"]["SessionStart"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let codex_hooks_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".codex/hooks.json")).unwrap())
            .unwrap();
    assert_eq!(
        codex_hooks_json["hooks"]["SessionStart"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let opencode_plugin =
        fs::read_to_string(temp.path().join(".opencode/plugins/nodus-hooks.js")).unwrap();
    assert_eq!(
        opencode_plugin
            .matches(".opencode/scripts/nodus-hook-")
            .count(),
        1
    );
}

#[test]
#[cfg(unix)]
fn resync_does_not_remove_and_recreate_unchanged_managed_skill_directories() {
    use std::os::unix::fs::MetadataExt;

    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(
        &temp.path().join("vendor/alpha/skills/alpha-memory"),
        "alpha-memory",
    );
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["claude", "codex"]

[dependencies.alpha]
path = "vendor/alpha"
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "alpha")
        .unwrap();
    let claude_skill_dir =
        managed_skill_file(temp.path(), Adapter::Claude, dependency, "alpha-memory")
            .parent()
            .unwrap()
            .to_path_buf();
    let codex_skill_dir =
        managed_skill_file(temp.path(), Adapter::Codex, dependency, "alpha-memory")
            .parent()
            .unwrap()
            .to_path_buf();
    let claude_inode = fs::metadata(&claude_skill_dir).unwrap().ino();
    let codex_inode = fs::metadata(&codex_skill_dir).unwrap().ino();

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    assert_eq!(
        fs::metadata(&claude_skill_dir).unwrap().ino(),
        claude_inode,
        "second sync removed and recreated the Claude skill directory"
    );
    assert_eq!(
        fs::metadata(&codex_skill_dir).unwrap().ino(),
        codex_inode,
        "second sync removed and recreated the Codex skill directory"
    );
}

#[test]
fn sync_deduplicates_named_hook_declared_by_both_root_and_dependency() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["claude"]

[[hooks]]
id = "fuli.claude.session-start"
event = "session_start"
adapters = ["claude"]

[hooks.matcher]
sources = ["startup", "resume", "clear", "compact"]

[hooks.handler]
type = "command"
command = "fuli integration claude hook session-start"

[dependencies.fuli]
path = "vendor/fuli"
"#,
    );
    write_file(
        &temp.path().join("vendor/fuli/nodus.toml"),
        r#"
[[hooks]]
id = "fuli.claude.session-start"
event = "session_start"
adapters = ["claude"]

[hooks.matcher]
sources = ["startup", "resume", "clear", "compact"]

[hooks.handler]
type = "command"
command = "fuli integration claude hook session-start"
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let claude_hooks = temp.path().join(".claude/hooks");
    let claude_hook_files = fs::read_dir(&claude_hooks)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("fuli-claude-session-start"))
        .collect::<Vec<_>>();
    assert_eq!(
        claude_hook_files.len(),
        1,
        "expected the root-declared hook to win; got {claude_hook_files:?}"
    );
    assert!(
        claude_hook_files[0].starts_with("nodus-hook-fuli-claude-session-start-"),
        "unexpected Claude hook file: {claude_hook_files:?}"
    );

    let claude_settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(temp.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    let session_start_entries = claude_settings["hooks"]["SessionStart"].as_array().unwrap();
    let fuli_entries = session_start_entries
        .iter()
        .filter(|entry| {
            entry["hooks"]
                .as_array()
                .map(|hooks| {
                    hooks.iter().any(|hook| {
                        hook["command"]
                            .as_str()
                            .is_some_and(|cmd| cmd.contains("fuli-claude-session-start"))
                    })
                })
                .unwrap_or(false)
        })
        .count();
    assert_eq!(fuli_entries, 1);
}

#[test]
fn sync_merges_codex_startup_hook_into_existing_hooks_without_duplicates() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["codex"]

[launch_hooks]
sync_on_startup = true
"#,
    );
    write_file(
        &temp.path().join(".codex/hooks.json"),
        r#"{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup",
        "hooks": [
          {
            "type": "command",
            "command": "./scripts/custom-startup.sh"
          }
        ]
      }
    ]
  }
}
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();
    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".codex/hooks.json")).unwrap())
            .unwrap();
    let session_start = hooks["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(session_start.len(), 2);
    assert!(session_start.iter().any(|entry| {
        entry["matcher"].as_str() == Some("startup")
            && entry["hooks"].as_array().is_some_and(|hooks| {
                hooks.iter().any(|hook| {
                    hook["type"].as_str() == Some("command")
                        && hook["command"].as_str() == Some("./scripts/custom-startup.sh")
                })
            })
    }));
    assert_eq!(
        session_start
            .iter()
            .filter(|entry| entry["matcher"].as_str() == Some("startup|resume"))
            .count(),
        1
    );
    assert_eq!(
        session_start
            .iter()
            .filter(|entry| {
                entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks.iter().any(|hook| {
                        hook["type"].as_str() == Some("command")
                            && hook["command"]
                                .as_str()
                                .is_some_and(|command| command.contains(".codex/hooks/nodus-hook-"))
                    })
                })
            })
            .count(),
        1
    );
}

#[test]
fn sync_merges_claude_startup_hook_into_existing_settings_without_duplicates() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["claude"]

[launch_hooks]
sync_on_startup = true
"#,
    );
    write_file(
        &temp.path().join(".claude/settings.json"),
        r#"{
  "permissions": {
    "allow": ["Bash(git status)"]
  },
  "hooks": {
    "SessionStart": [
      {
        "matcher": "resume",
        "hooks": [
          {
            "type": "command",
            "command": "./scripts/resume.sh"
          }
        ]
      },
      {
        "matcher": "startup",
        "hooks": [
          {
            "type": "command",
            "command": "./scripts/custom-startup.sh"
          }
        ]
      }
    ]
  }
}
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();
    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(temp.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();

    assert_eq!(
        settings["permissions"]["allow"][0].as_str(),
        Some("Bash(git status)")
    );

    let session_start = settings["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(session_start.len(), 3);

    let startup = session_start
        .iter()
        .find(|entry| entry["matcher"].as_str() == Some("startup"))
        .unwrap();
    let startup_hooks = startup["hooks"].as_array().unwrap();
    assert_eq!(startup_hooks.len(), 1);
    assert!(startup_hooks.iter().any(|hook| {
        hook["type"].as_str() == Some("command")
            && hook["command"].as_str() == Some("./scripts/custom-startup.sh")
    }));
    assert_eq!(
        startup_hooks
            .iter()
            .filter(|hook| {
                hook["type"].as_str() == Some("command")
                    && hook["command"]
                        .as_str()
                        .is_some_and(|command| command.contains("./.claude/hooks/nodus-hook-"))
            })
            .count(),
        0
    );
    assert_eq!(
        session_start
            .iter()
            .filter(|entry| entry["matcher"].as_str() == Some("startup|resume"))
            .count(),
        1
    );
}

#[test]
fn sync_gracefully_preserves_user_claude_local_settings_when_hooks_are_enabled_later() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["claude"]
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    write_file(
        &temp.path().join(".claude/settings.local.json"),
        r#"{
  "permissions": {
    "allow": ["Bash(git status)"]
  }
}
"#,
    );
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["claude"]

[launch_hooks]
sync_on_startup = true
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();
    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let local_settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(temp.path().join(".claude/settings.local.json")).unwrap(),
    )
    .unwrap();
    let settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(temp.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();

    assert_eq!(
        local_settings["permissions"]["allow"][0].as_str(),
        Some("Bash(git status)")
    );
    assert_eq!(
        settings["hooks"]["SessionStart"][0]["matcher"].as_str(),
        Some("startup|resume")
    );
    assert_eq!(
        settings["hooks"]["SessionStart"][0]["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|hook| {
                hook["type"].as_str() == Some("command")
                    && hook["command"]
                        .as_str()
                        .is_some_and(|command| command.contains("./.claude/hooks/nodus-hook-"))
            })
            .count(),
        1
    );
}

#[test]
fn sync_emits_explicit_pre_tool_hooks_for_supported_adapters() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["claude", "codex", "opencode"]

[[hooks]]
id = "bash-preflight"
event = "pre_tool_use"

[hooks.matcher]
tool_names = ["bash"]

[hooks.handler]
type = "command"
command = "./scripts/preflight.sh"
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let claude_settings = fs::read_to_string(temp.path().join(".claude/settings.json")).unwrap();
    let codex_hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".codex/hooks.json")).unwrap())
            .unwrap();
    let opencode_plugin =
        fs::read_to_string(temp.path().join(".opencode/plugins/nodus-hooks.js")).unwrap();

    assert!(claude_settings.contains("\"PreToolUse\""));
    assert!(claude_settings.contains("\"Bash\""));
    assert_eq!(
        codex_hooks["hooks"]["PreToolUse"][0]["matcher"].as_str(),
        Some("Bash")
    );
    assert!(
        codex_hooks["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains(".codex/hooks/nodus-hook-")
    );
    assert!(opencode_plugin.contains("\"tool.execute.before\""));
    assert!(opencode_plugin.contains(".opencode/scripts/nodus-hook-"));
}

#[test]
fn sync_filters_tool_hook_matchers_by_adapter_support() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["claude", "codex", "copilot", "opencode"]

[[hooks]]
id = "tool-preflight"
event = "pre_tool_use"

[hooks.matcher]
tool_names = ["bash", "read", "apply_patch"]

[hooks.handler]
type = "command"
command = "./scripts/preflight.sh"

[[hooks]]
id = "codex-read"
event = "pre_tool_use"
adapters = ["codex"]

[hooks.matcher]
tool_names = ["read"]

[hooks.handler]
type = "command"
command = "./scripts/read.sh"
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let claude_settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(temp.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        claude_settings["hooks"]["PreToolUse"][0]["matcher"].as_str(),
        Some("Bash|Read")
    );

    let codex_hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".codex/hooks.json")).unwrap())
            .unwrap();
    let codex_pre_tool = codex_hooks["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(codex_pre_tool.len(), 1);
    assert_eq!(
        codex_pre_tool[0]["matcher"].as_str(),
        Some("Bash|apply_patch")
    );

    let copilot_hooks: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(temp.path().join(".github/hooks/nodus-hooks.json")).unwrap(),
    )
    .unwrap();
    let copilot_script = copilot_hooks["hooks"]["preToolUse"][0]["bash"]
        .as_str()
        .unwrap()
        .trim_start_matches("./");
    let copilot_script = fs::read_to_string(temp.path().join(copilot_script)).unwrap();
    assert!(copilot_script.contains(" bash view "));
    assert!(!copilot_script.contains("apply_patch"));

    let opencode_plugin =
        fs::read_to_string(temp.path().join(".opencode/plugins/nodus-hooks.js")).unwrap();
    assert!(opencode_plugin.contains(r#"toolNames: ["bash", "read", "apply_patch"]"#));
}

#[test]
fn sync_emits_codex_user_prompt_submit_hook() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["codex"]

[[hooks]]
id = "prompt-logger"
event = "user_prompt_submit"

[hooks.handler]
type = "command"
command = "./scripts/log-prompt.sh"
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let codex_hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".codex/hooks.json")).unwrap())
            .unwrap();
    let user_prompt = codex_hooks["hooks"]["UserPromptSubmit"].as_array().unwrap();
    assert_eq!(user_prompt.len(), 1);
    assert!(user_prompt[0].get("matcher").is_none());
    assert!(
        user_prompt[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains(".codex/hooks/nodus-hook-")
    );
}

#[test]
fn sync_emits_codex_apply_patch_edit_and_write_matchers() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["codex"]

[[hooks]]
id = "mutation-audit"
event = "pre_tool_use"

[hooks.matcher]
tool_names = ["apply_patch", "edit", "write"]

[hooks.handler]
type = "command"
command = "./scripts/audit.sh"
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let codex_hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".codex/hooks.json")).unwrap())
            .unwrap();
    assert_eq!(
        codex_hooks["hooks"]["PreToolUse"][0]["matcher"].as_str(),
        Some("apply_patch|Edit|Write")
    );
}

#[test]
fn sync_emits_codex_permission_request_hook() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["codex"]

[[hooks]]
id = "bash-approval"
event = "permission_request"

[hooks.matcher]
tool_names = ["bash"]

[hooks.handler]
type = "command"
command = "./scripts/approve.sh"
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let codex_hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".codex/hooks.json")).unwrap())
            .unwrap();
    let permission = codex_hooks["hooks"]["PermissionRequest"]
        .as_array()
        .unwrap();
    assert_eq!(permission.len(), 1);
    assert_eq!(permission[0]["matcher"].as_str(), Some("Bash"));
    assert!(
        permission[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains(".codex/hooks/nodus-hook-")
    );
}

#[test]
fn sync_emits_copilot_hooks_for_supported_events() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["copilot"]

[[hooks]]
id = "session-memory"
event = "session_start"
timeout_sec = 45

[hooks.matcher]
sources = ["startup", "resume"]

[hooks.handler]
type = "command"
command = "./scripts/session-memory.sh"

[[hooks]]
id = "prompt-logger"
event = "user_prompt_submit"

[hooks.handler]
type = "command"
command = "./scripts/log-prompt.sh"

[[hooks]]
id = "bash-preflight"
event = "pre_tool_use"

[hooks.matcher]
tool_names = ["bash"]

[hooks.handler]
type = "command"
command = "./scripts/preflight.sh"

[[hooks]]
id = "turn-finished"
event = "stop"

[hooks.handler]
type = "command"
command = "./scripts/turn-finished.sh"

[[hooks]]
id = "session-end"
event = "session_end"

[hooks.handler]
type = "command"
command = "./scripts/session-end.sh"

[[hooks]]
id = "subagent-finished"
event = "subagent_stop"

[hooks.handler]
type = "command"
command = "./scripts/subagent-finished.sh"
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let hooks_path = temp.path().join(".github/hooks/nodus-hooks.json");
    let hooks_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
    assert_eq!(hooks_json["version"].as_i64(), Some(1));
    assert_eq!(
        hooks_json["hooks"]["sessionStart"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        hooks_json["hooks"]["sessionStart"][0]["timeoutSec"].as_i64(),
        Some(45)
    );
    assert_eq!(
        hooks_json["hooks"]["userPromptSubmitted"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        hooks_json["hooks"]["preToolUse"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        hooks_json["hooks"]["agentStop"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        hooks_json["hooks"]["sessionEnd"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        hooks_json["hooks"]["subagentStop"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let pre_tool_script = hooks_json["hooks"]["preToolUse"][0]["bash"]
        .as_str()
        .unwrap()
        .trim_start_matches("./");
    let script = fs::read_to_string(temp.path().join(pre_tool_script)).unwrap();
    assert!(script.contains("json_string_field toolName"));
    assert!(script.contains(" bash "));

    let session_script = hooks_json["hooks"]["sessionStart"][0]["bash"]
        .as_str()
        .unwrap()
        .trim_start_matches("./");
    let script = fs::read_to_string(temp.path().join(session_script)).unwrap();
    assert!(script.contains("new|startup"));
    assert!(script.contains(" resume "));
    assert!(script.contains("NODUS_HOOK_TIMEOUT_SEC='45'"));
}

#[test]
fn sync_deduplicates_managed_codex_user_prompt_and_permission_request_hooks() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["codex"]

[[hooks]]
id = "prompt-logger"
event = "user_prompt_submit"

[hooks.handler]
type = "command"
command = "./scripts/log-prompt.sh"

[[hooks]]
id = "bash-approval"
event = "permission_request"

[hooks.matcher]
tool_names = ["bash"]

[hooks.handler]
type = "command"
command = "./scripts/approve.sh"
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();
    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let codex_hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".codex/hooks.json")).unwrap())
            .unwrap();
    assert_eq!(
        codex_hooks["hooks"]["UserPromptSubmit"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        codex_hooks["hooks"]["PermissionRequest"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn sync_rejects_claude_only_declaration_of_codex_permission_request() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["claude"]

[[hooks]]
id = "bash-approval"
event = "permission_request"
adapters = ["claude"]

[hooks.matcher]
tool_names = ["bash"]

[hooks.handler]
type = "command"
command = "./scripts/approve.sh"
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let claude_settings_path = temp.path().join(".claude/settings.json");
    if claude_settings_path.exists() {
        let settings = fs::read_to_string(&claude_settings_path).unwrap();
        assert!(
            !settings.contains("PermissionRequest"),
            "PermissionRequest must not leak into Claude settings"
        );
    }
}

#[test]
fn sync_emits_claude_native_lifecycle_hooks_without_leaking_to_other_adapters() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["claude", "codex", "opencode"]

[[hooks]]
id = "prompt-remember"
event = "user_prompt_submit"
adapters = ["claude"]

[hooks.handler]
type = "command"
command = "./scripts/remember.sh"

[[hooks]]
id = "session-finish"
event = "session_end"
adapters = ["claude"]

[hooks.handler]
type = "command"
command = "./scripts/finish.sh"

[[hooks]]
id = "subagent-finish"
event = "subagent_stop"
adapters = ["claude"]

[hooks.handler]
type = "command"
command = "./scripts/subagent-finish.sh"
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let claude_settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(temp.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();

    assert!(
        claude_settings["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
            .as_str()
            .is_some_and(|command| command.contains("./.claude/hooks/nodus-hook-"))
    );
    assert!(
        claude_settings["hooks"]["SessionEnd"][0]["hooks"][0]["command"]
            .as_str()
            .is_some_and(|command| command.contains("./.claude/hooks/nodus-hook-"))
    );
    assert!(
        claude_settings["hooks"]["SubagentStop"][0]["hooks"][0]["command"]
            .as_str()
            .is_some_and(|command| command.contains("./.claude/hooks/nodus-hook-"))
    );
    assert!(!temp.path().join(".codex/hooks.json").exists());
    assert!(
        !temp
            .path()
            .join(".opencode/plugins/nodus-hooks.js")
            .exists()
    );
}

#[test]
fn sync_emits_claude_clear_and_compact_session_start_sources() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["claude", "codex", "opencode"]

[[hooks]]
id = "session-memory"
event = "session_start"

[hooks.matcher]
sources = ["startup", "clear", "compact"]

[hooks.handler]
type = "command"
command = "./scripts/session-memory.sh"
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let claude_settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(temp.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    let codex_hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".codex/hooks.json")).unwrap())
            .unwrap();

    assert_eq!(
        claude_settings["hooks"]["SessionStart"][0]["matcher"].as_str(),
        Some("startup|clear|compact")
    );
    assert_eq!(
        codex_hooks["hooks"]["SessionStart"][0]["matcher"].as_str(),
        Some("startup|clear")
    );
    assert!(
        temp.path()
            .join(".opencode/plugins/nodus-hooks.js")
            .exists()
    );
}

fn opencode_virtual_plugin_wrappers(
    project_root: &Path,
    package_alias: &str,
    name_fragment: &str,
) -> Vec<PathBuf> {
    let plugins_dir = project_root.join(".opencode/plugins");
    let Ok(entries) = fs::read_dir(&plugins_dir) else {
        return Vec::new();
    };
    let mut wrappers = entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with(&format!("nodus-{package_alias}-"))
                        && name.contains(name_fragment)
                })
        })
        .collect::<Vec<_>>();
    wrappers.sort();
    wrappers
}

fn single_opencode_virtual_plugin_wrapper(
    project_root: &Path,
    package_alias: &str,
    name_fragment: &str,
) -> PathBuf {
    let wrappers = opencode_virtual_plugin_wrappers(project_root, package_alias, name_fragment);
    assert_eq!(
        wrappers.len(),
        1,
        "expected one OpenCode virtual plugin wrapper for `{package_alias}` containing `{name_fragment}`"
    );
    wrappers.into_iter().next().unwrap()
}

#[test]
fn sync_emits_opencode_virtual_plugin_wrappers_for_default_and_named_exports() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
name = "root"
opencode_plugin_hooks = ["hooks/default-plugin.ts", "hooks/named-plugin.ts"]

[adapters]
enabled = ["opencode"]
"#,
    );
    write_file(
        &temp.path().join("hooks/default-plugin.ts"),
        "export default function plugin() { return {}; }\n",
    );
    write_file(
        &temp.path().join("hooks/named-plugin.ts"),
        "export function plugin() { return {}; }\n",
    );
    write_file(
        &temp.path().join("hooks/helper.ts"),
        "export const value = 1;\n",
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    assert!(
        temp.path()
            .join(".nodus/packages/root/opencode-plugin/hooks/default-plugin.ts")
            .exists()
    );
    assert!(
        temp.path()
            .join(".nodus/packages/root/opencode-plugin/hooks/helper.ts")
            .exists()
    );
    let default_wrapper =
        single_opencode_virtual_plugin_wrapper(temp.path(), "root", "default-plugin");
    let default_wrapper = fs::read_to_string(default_wrapper).unwrap();
    assert!(
        default_wrapper
            .contains("../../.nodus/packages/root/opencode-plugin/hooks/default-plugin.ts")
    );
    assert!(default_wrapper.contains("pluginModule.default"));
    assert!(default_wrapper.contains("export * from"));
    assert!(default_wrapper.contains("export default plugin"));

    let named_wrapper = single_opencode_virtual_plugin_wrapper(temp.path(), "root", "named-plugin");
    let named_wrapper = fs::read_to_string(named_wrapper).unwrap();
    assert!(
        named_wrapper.contains("../../.nodus/packages/root/opencode-plugin/hooks/named-plugin.ts")
    );
    assert!(named_wrapper.contains("pluginModule.plugin"));
    assert!(named_wrapper.contains("Object.values(pluginModule)"));
    assert!(!temp.path().join(".claude/settings.json").exists());
    assert!(!temp.path().join(".codex/hooks.json").exists());
}

#[test]
fn sync_installs_opencode_runtime_packages_as_managed_plugins() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_manifest(&temp.path().join("vendor/shared"), r#"name = "shared""#);
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    sync_in_dir_with_adapters_no_fast_path(temp.path(), cache.path(), &[Adapter::OpenCode])
        .unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared_package = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), shared_package, Adapter::OpenCode);
    assert!(
        temp.path()
            .join(".opencode/skills/review/SKILL.md")
            .exists()
    );
    assert!(plugin_root.join("skills/review/SKILL.md").exists());
    assert!(opencode_virtual_plugin_wrappers(temp.path(), "shared", "plugin").is_empty());

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let shared = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let plugin_root_relative = display_path(plugin_root.strip_prefix(temp.path()).unwrap());
    assert_not_owned(&lockfile, temp.path(), &plugin_root_relative);
    assert_eq!(shared.owned_runtime_adapters, vec![Adapter::OpenCode]);
    assert!(
        shared
            .owned_subtrees
            .iter()
            .all(|path| !path.starts_with(".opencode/skills/")),
        "OpenCode direct skill roots should be represented by owned_runtime_adapters, got {:?}",
        shared.owned_subtrees
    );
    assert_owned(&lockfile, temp.path(), ".opencode/skills/review/SKILL.md");
    assert!(
        shared
            .owned_prefixes
            .iter()
            .all(|rule| !(rule.dir == ".opencode/plugins" && rule.prefix == "nodus-shared-"))
    );

    write_manifest(temp.path(), "");
    sync_in_dir_with_adapters_no_fast_path(temp.path(), cache.path(), &[Adapter::OpenCode])
        .unwrap();

    assert!(!temp.path().join(".nodus/packages/shared").exists());
}

#[test]
fn sync_updates_and_prunes_opencode_virtual_plugin_outputs() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
opencode_plugin_hooks = ["hooks/old-plugin.ts"]
"#,
    );
    write_file(
        &temp.path().join("vendor/shared/hooks/old-plugin.ts"),
        "export default function plugin() { return {}; }\n",
    );

    sync_in_dir_with_adapters_no_fast_path(temp.path(), cache.path(), &[Adapter::OpenCode])
        .unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared_package = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let old_plugin_root = global_native_plugin_root(temp.path(), shared_package, Adapter::OpenCode);
    let old_wrapper = single_opencode_virtual_plugin_wrapper(temp.path(), "shared", "old-plugin");
    assert!(old_plugin_root.join("hooks/old-plugin.ts").exists());
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let shared = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let old_plugin_root_relative = display_path(old_plugin_root.strip_prefix(temp.path()).unwrap());
    assert_not_owned(&lockfile, temp.path(), &old_plugin_root_relative);
    assert!(
        shared
            .owned_prefixes
            .iter()
            .any(|rule| rule.dir == ".opencode/plugins" && rule.prefix == "nodus-shared-")
    );

    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
opencode_plugin_hooks = ["hooks/new-plugin.ts"]
"#,
    );
    fs::remove_file(temp.path().join("vendor/shared/hooks/old-plugin.ts")).unwrap();
    write_file(
        &temp.path().join("vendor/shared/hooks/new-plugin.ts"),
        "export default function plugin() { return {}; }\n",
    );
    sync_in_dir_with_adapters_no_fast_path(temp.path(), cache.path(), &[Adapter::OpenCode])
        .unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared_package = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let new_plugin_root = global_native_plugin_root(temp.path(), shared_package, Adapter::OpenCode);
    assert!(!old_wrapper.exists());
    assert!(
        !temp
            .path()
            .join(".nodus/packages/shared/opencode-plugin/hooks/old-plugin.ts")
            .exists()
    );
    assert!(new_plugin_root.join("hooks/new-plugin.ts").exists());
    assert!(single_opencode_virtual_plugin_wrapper(temp.path(), "shared", "new-plugin").exists());

    write_manifest(temp.path(), "");
    sync_in_dir_with_adapters_no_fast_path(temp.path(), cache.path(), &[Adapter::OpenCode])
        .unwrap();

    assert!(!temp.path().join(".nodus/packages/shared").exists());
    assert!(
        opencode_virtual_plugin_wrappers(temp.path(), "shared", "new-plugin").is_empty(),
        "OpenCode virtual plugin wrappers should be pruned when the dependency is removed"
    );
}

#[test]
fn sync_filters_opencode_virtual_plugins_by_adapter_and_component_selection() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
opencode_plugin_hooks = ["hooks/nodus-plugin.ts"]
"#,
    );
    write_file(
        &temp.path().join("vendor/shared/hooks/nodus-plugin.ts"),
        "export default function plugin() { return {}; }\n",
    );

    sync_in_dir_with_adapters_no_fast_path(temp.path(), cache.path(), &[Adapter::Codex]).unwrap();

    assert!(!temp.path().join(".opencode/plugins").exists());
    assert!(
        !temp
            .path()
            .join(".nodus/packages/shared/opencode-plugin")
            .exists()
    );

    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared", components = ["skills"] }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    sync_in_dir_with_adapters_no_fast_path(temp.path(), cache.path(), &[Adapter::OpenCode])
        .unwrap();

    assert!(
        temp.path()
            .join(".opencode/skills/review/SKILL.md")
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(".nodus/packages/shared/opencode-plugin")
            .exists()
    );
    assert!(opencode_virtual_plugin_wrappers(temp.path(), "shared", "nodus-plugin").is_empty());
}

#[test]
fn sync_warns_when_launch_hooks_are_unsupported_for_selected_adapters() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["agents", "cursor"]

[launch_hooks]
sync_on_startup = true
"#,
    );

    let buffer = SharedBuffer::default();
    let reporter = Reporter::sink(ColorMode::Never, buffer.clone());
    super::sync_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        false,
        false,
        false,
        &[],
        false,
        &reporter,
    )
    .unwrap();

    let output = buffer.contents();
    assert!(output.contains("hooks are not emitted for `agents`"));
    assert!(output.contains("hooks are not emitted for `cursor`"));
}

#[test]
fn sync_warns_when_activation_is_unsupported_for_selected_adapters() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["agents", "cursor"]

[dependencies.shared]
path = "vendor/shared"
components = ["skills"]
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
[activation]
always_context = ["prompts/bootstrap.md"]
"#,
    );
    write_file(
        &temp.path().join("vendor/shared/prompts/bootstrap.md"),
        "Bootstrap context.\n",
    );

    let buffer = SharedBuffer::default();
    let reporter = Reporter::sink(ColorMode::Never, buffer.clone());
    super::sync_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        false,
        false,
        false,
        &[],
        false,
        &reporter,
    )
    .unwrap();

    let output = buffer.contents();
    assert!(output.contains("activation context is not emitted for `agents`"));
    assert!(output.contains("activation context is not emitted for `cursor`"));
}

#[test]
fn sync_rejects_launch_hook_persistence_with_locked_flag() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    fs::create_dir_all(temp.path().join(".codex")).unwrap();
    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let reporter = Reporter::silent();
    let error = super::sync_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        true,
        false,
        false,
        &[],
        true,
        &reporter,
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("launch hook configuration"));
}

#[test]
fn sync_force_does_not_bypass_locked_stale_lockfile_checks() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();
    let lockfile_before = fs::read(temp.path().join(LOCKFILE_NAME)).unwrap();

    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    let reporter = Reporter::silent();
    super::sync_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        true,
        false,
        true,
        &[],
        false,
        &reporter,
    )
    .unwrap_err();
    assert_eq!(
        fs::read(temp.path().join(LOCKFILE_NAME)).unwrap(),
        lockfile_before
    );
    assert!(
        !temp
            .path()
            .join(".nodus/packages/shared/codex-plugin")
            .exists()
    );
}

#[test]
fn sync_frozen_requires_existing_lockfile() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["claude"]
"#,
    );

    let error = sync_in_dir_frozen(temp.path(), cache.path(), false)
        .unwrap_err()
        .to_string();

    assert!(error.contains("`--frozen` requires an existing nodus.lock"));
}

#[test]
fn sync_locked_rejects_legacy_launch_hook_config_migration() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["codex"]

[launch_hooks]
sync_on_startup = true
"#,
    );
    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["codex"]

[launch_hooks]
sync_on_startup = true
"#,
    );

    let error = super::sync_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        true,
        false,
        false,
        &[],
        false,
        &Reporter::silent(),
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("launch_hooks.sync_on_startup"));
    assert!(error.contains("rewrite `nodus.toml` with [[hooks]]"));
    assert!(
        fs::read_to_string(temp.path().join(MANIFEST_FILE))
            .unwrap()
            .contains("[launch_hooks]")
    );
}

#[test]
fn sync_warns_and_reuses_locked_cached_revision_when_git_refresh_fails() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let repo = TempDir::new().unwrap();
    write_file(
        &repo.path().join("skills/review/SKILL.md"),
        "---\nname: Review\ndescription: First revision.\n---\n# Review\nfirst\n",
    );
    init_git_repo(repo.path());
    rename_current_branch(repo.path(), "main");

    write_manifest(
        temp.path(),
        &format!(
            r#"
[adapters]
enabled = ["claude"]

[dependencies]
review_pkg = {{ url = "{}", branch = "main" }}
"#,
            toml_path_value(repo.path())
        ),
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let initial_lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let initial_rev = initial_lockfile
        .packages
        .iter()
        .find(|package| package.alias == "review_pkg")
        .and_then(|package| package.source.rev.clone())
        .unwrap();
    let initial_resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let initial_dependency = initial_resolution
        .packages
        .iter()
        .find(|package| package.alias == "review_pkg")
        .unwrap();
    let initial_skill_path =
        managed_skill_file(temp.path(), Adapter::Claude, initial_dependency, "review");

    fs::remove_dir_all(repo.path()).unwrap();

    let buffer = SharedBuffer::default();
    let reporter = Reporter::sink(ColorMode::Never, buffer.clone());
    super::sync_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        false,
        false,
        false,
        &[],
        false,
        &reporter,
    )
    .unwrap();

    let output = buffer.contents();
    assert!(output.contains("warning: dependency `review_pkg` could not be refreshed"));
    assert!(output.contains("reusing locked cached revision"));

    let updated_lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let updated_rev = updated_lockfile
        .packages
        .iter()
        .find(|package| package.alias == "review_pkg")
        .and_then(|package| package.source.rev.clone())
        .unwrap();
    assert_eq!(updated_rev, initial_rev);
    assert_eq!(
        fs::read_to_string(&initial_skill_path).unwrap(),
        "---\nname: Review\ndescription: First revision.\n---\n# Review\nfirst\n"
    );
}

#[test]
fn sync_strict_fails_when_git_refresh_fails() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let repo = TempDir::new().unwrap();
    write_file(
        &repo.path().join("skills/review/SKILL.md"),
        "---\nname: Review\ndescription: First revision.\n---\n# Review\nfirst\n",
    );
    init_git_repo(repo.path());
    rename_current_branch(repo.path(), "main");

    write_manifest(
        temp.path(),
        &format!(
            r#"
[adapters]
enabled = ["claude"]

[dependencies]
review_pkg = {{ url = "{}", branch = "main" }}
"#,
            toml_path_value(repo.path())
        ),
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let initial_lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    fs::remove_dir_all(repo.path()).unwrap();

    let error = sync_in_dir_strict(temp.path(), cache.path(), false, false)
        .unwrap_err()
        .to_string();

    assert!(error.contains("git [\"fetch\""));

    let updated_lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert_eq!(updated_lockfile, initial_lockfile);
}

#[test]
fn sync_frozen_installs_branch_dependencies_from_locked_revision() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    let repo = TempDir::new().unwrap();
    write_file(
        &repo.path().join("skills/review/SKILL.md"),
        "---\nname: Review\ndescription: First revision.\n---\n# Review\nfirst\n",
    );
    init_git_repo(repo.path());
    rename_current_branch(repo.path(), "main");

    write_manifest(
        temp.path(),
        &format!(
            r#"
[adapters]
enabled = ["claude"]

[dependencies]
review_pkg = {{ url = "{}", branch = "main" }}
"#,
            toml_path_value(repo.path())
        ),
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let initial_lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let initial_rev = initial_lockfile
        .packages
        .iter()
        .find(|package| package.alias == "review_pkg")
        .and_then(|package| package.source.rev.clone())
        .unwrap();
    let initial_resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let initial_dependency = initial_resolution
        .packages
        .iter()
        .find(|package| package.alias == "review_pkg")
        .unwrap();
    let initial_skill_id = namespaced_skill_id(initial_dependency, "review");
    let initial_skill_path =
        managed_skill_file(temp.path(), Adapter::Claude, initial_dependency, "review");
    assert!(initial_skill_path.exists());
    assert!(
        fs::read_to_string(&initial_skill_path)
            .unwrap()
            .contains("first")
    );

    write_file(
        &repo.path().join("skills/review/SKILL.md"),
        "---\nname: Review\ndescription: Second revision.\n---\n# Review\nsecond\n",
    );
    commit_all(repo.path(), "advance");

    sync_in_dir_frozen(temp.path(), cache.path(), false).unwrap();

    let frozen_lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let frozen_rev = frozen_lockfile
        .packages
        .iter()
        .find(|package| package.alias == "review_pkg")
        .and_then(|package| package.source.rev.clone())
        .unwrap();
    assert_eq!(frozen_rev, initial_rev);
    assert!(initial_skill_path.exists());
    assert!(
        fs::read_to_string(&initial_skill_path)
            .unwrap()
            .contains("first")
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let updated_lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let updated_rev = updated_lockfile
        .packages
        .iter()
        .find(|package| package.alias == "review_pkg")
        .and_then(|package| package.source.rev.clone())
        .unwrap();
    assert_ne!(updated_rev, initial_rev);

    let updated_resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let updated_dependency = updated_resolution
        .packages
        .iter()
        .find(|package| package.alias == "review_pkg")
        .unwrap();
    let updated_skill_id = namespaced_skill_id(updated_dependency, "review");
    let updated_skill_path =
        managed_skill_file(temp.path(), Adapter::Claude, updated_dependency, "review");
    assert_eq!(updated_skill_id, initial_skill_id);
    assert!(initial_skill_path.exists());
    assert!(updated_skill_path.exists());
    assert!(
        fs::read_to_string(&updated_skill_path)
            .unwrap()
            .contains("second")
    );
}

#[test]
fn sync_requires_explicit_adapter_when_repo_has_no_signals() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");

    let error = sync_in_dir(temp.path(), cache.path(), false, false)
        .unwrap_err()
        .to_string();

    assert!(error.contains("Pass `--adapter"));
}

#[test]
fn sync_prefers_manifest_selection_over_detected_roots() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["codex"]
"#,
    );
    fs::create_dir_all(temp.path().join(".claude")).unwrap();

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    assert!(!temp.path().join(".codex/skills").exists());
    assert!(!temp.path().join(".claude/skills").exists());
}

#[test]
fn sync_prunes_outputs_when_adapter_selection_is_narrowed() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    sync_all(temp.path(), cache.path());
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
    assert!(temp.path().join(".opencode/skills").exists());

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Claude]).unwrap();

    let manifest = load_root_from_dir(temp.path()).unwrap();
    assert_eq!(
        manifest.manifest.enabled_adapters().unwrap(),
        [Adapter::Claude].as_slice()
    );
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
    assert!(
        !temp
            .path()
            .join(format!(".codex/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(!temp.path().join(".opencode/skills").exists());
    assert!(!temp.path().join(".opencode/.gitignore").exists());
}

#[test]
fn sync_prunes_outputs_when_dependency_components_are_narrowed() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/agents/shared.md"),
        "# Shared\n",
    );

    sync_all(temp.path(), cache.path());

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    let managed_agent_file = namespaced_file_name(dependency, "shared", "md");
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
    assert!(runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_agent_file
    ));

    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["claude", "codex", "opencode"]

[dependencies]
shared = { path = "vendor/shared", components = ["skills"] }
"#,
    );

    sync_all(temp.path(), cache.path());

    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
    let narrowed_resolution =
        resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let narrowed_dependency = narrowed_resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), narrowed_dependency, Adapter::Claude);
    let plugin: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(plugin_root.join(".claude-plugin/plugin.json")).unwrap(),
    )
    .unwrap();
    assert!(plugin.get("agents").is_none());
    assert!(
        !temp
            .path()
            .join(format!(".opencode/agents/{managed_agent_file}"))
            .exists()
    );
}

#[test]
fn sync_records_stable_skill_roots_in_lockfile() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(
        &temp.path().join("vendor/shared/skills/iframe-ad"),
        "Iframe Ad",
    );

    sync_all(temp.path(), cache.path());

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), shared, Adapter::Claude);
    let plugin_root_relative = display_path(plugin_root.strip_prefix(temp.path()).unwrap());
    assert_not_owned(&lockfile, temp.path(), &plugin_root_relative);
    assert_owned(&lockfile, temp.path(), ".github/skills/iframe-ad");
    assert_owned(&lockfile, temp.path(), ".opencode/skills/iframe-ad");
    // No per-package owned entry should mention the raw skill id `iframe-ad`
    // (the v10 emission records the disambiguated `<id>` form, but for a
    // single owner with no duplicate the path matches).
}

#[test]
fn sync_records_selected_components_without_supported_outputs() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared", components = ["agents"] }
"#,
    );
    write_file(
        &temp.path().join("vendor/shared/agents/shared.md"),
        "# Shared\n",
    );

    let summary =
        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex])
            .unwrap();
    // The package selects only agents, so MCP config and virtual full-package
    // payloads are not emitted. Codex agents live under the project-level
    // `.codex/agents/` runtime root.
    assert_eq!(summary.managed_file_count, 2);

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let shared = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    assert_eq!(
        shared.selected_components,
        Some(vec![DependencyComponent::Agents])
    );
    assert_owned(&lockfile, temp.path(), ".codex/agents/shared.toml");
    assert_not_owned(&lockfile, temp.path(), ".mcp.json");
    assert!(runtime_file_exists(
        temp.path(),
        Adapter::Codex,
        "shared.toml"
    ));
    assert!(!temp.path().join(".mcp.json").exists());
}

#[test]
fn sync_prefers_codex_specific_toml_agents_for_codex_and_markdown_for_claude() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared", components = ["agents"] }
"#,
    );
    write_file(
        &temp.path().join("vendor/shared/agents/security.md"),
        "# Shared markdown\n",
    );
    write_codex_agent_toml(
        &temp.path().join("vendor/shared/agents/security.codex.toml"),
        "Security reviewer",
        "Codex-specific instructions.",
        "Use codex.",
    );

    sync_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        false,
        false,
        &[Adapter::Claude, Adapter::Codex],
    )
    .unwrap();

    assert_eq!(
        fs::read_to_string(runtime_file_path(
            temp.path(),
            Adapter::Claude,
            "security.md"
        ))
        .unwrap(),
        "# Shared markdown\n"
    );

    let codex = fs::read_to_string(runtime_file_path(
        temp.path(),
        Adapter::Codex,
        "security.toml",
    ))
    .unwrap();
    assert!(codex.contains("name = \"Security reviewer\""));
    assert!(codex.contains("description = \"Codex-specific instructions.\""));
    assert!(codex.contains("developer_instructions = \"Use codex.\""));
}

#[test]
fn sync_merges_metadata_only_codex_agent_toml_with_markdown_body() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared", components = ["agents"] }
"#,
    );
    write_file(
        &temp.path().join("vendor/shared/agents/security.md"),
        "---\ntitle: Security\n---\n# Shared markdown\nReview carefully.\n",
    );
    write_file(
        &temp.path().join("vendor/shared/agents/security.codex.toml"),
        "name = \"Security reviewer\"\n\
description = \"Codex-specific metadata.\"\n\
model = \"gpt-5\"\n",
    );

    sync_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        false,
        false,
        &[
            Adapter::Claude,
            Adapter::Codex,
            Adapter::Copilot,
            Adapter::OpenCode,
        ],
    )
    .unwrap();

    assert_eq!(
        fs::read_to_string(runtime_file_path(
            temp.path(),
            Adapter::Claude,
            "security.md"
        ))
        .unwrap(),
        "---\ntitle: Security\n---\n# Shared markdown\nReview carefully.\n"
    );
    assert_eq!(
        fs::read_to_string(runtime_file_path(
            temp.path(),
            Adapter::Copilot,
            "security.agent.md"
        ))
        .unwrap(),
        "---\ntitle: Security\n---\n# Shared markdown\nReview carefully.\n"
    );
    assert_eq!(
        fs::read_to_string(runtime_file_path(
            temp.path(),
            Adapter::OpenCode,
            "security.md"
        ))
        .unwrap(),
        "---\ntitle: Security\n---\n# Shared markdown\nReview carefully.\n"
    );

    let codex = fs::read_to_string(runtime_file_path(
        temp.path(),
        Adapter::Codex,
        "security.toml",
    ))
    .unwrap();
    assert!(codex.contains("name = \"Security reviewer\""));
    assert!(codex.contains("description = \"Codex-specific metadata.\""));
    assert!(codex.contains("model = \"gpt-5\""));
    assert_eq!(
        crate::agent_format::parse_codex_agent_config(codex.as_bytes(), "emitted")
            .unwrap()
            .developer_instructions,
        "# Shared markdown\nReview carefully.\n"
    );
    assert!(!codex.contains("title: Security"));
}

#[test]
fn sync_emits_codex_agent_toml_from_markdown_fallback() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared", components = ["agents"] }
"#,
    );
    write_file(
        &temp.path().join("vendor/shared/agents/shared.md"),
        "# Shared\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    let codex = fs::read_to_string(runtime_file_path(
        temp.path(),
        Adapter::Codex,
        "shared.toml",
    ))
    .unwrap();
    assert!(codex.contains("name = \"shared\""));
    assert!(codex.contains("description = \"Instructions for the `shared` agent.\""));
    assert!(codex.contains("# Shared"));
}

#[test]
fn sync_writes_direct_managed_file_targets() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/prompts/review.md"),
        "Use the review prompt.\n",
    );

    sync_all(temp.path(), cache.path());

    assert_eq!(
        fs::read_to_string(temp.path().join(".github/prompts/review.md")).unwrap(),
        "Use the review prompt.\n"
    );

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert_owned(&lockfile, temp.path(), ".github/prompts/review.md");
}

#[test]
fn sync_writes_package_managed_exports_under_nodus_packages() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
[[managed_exports]]
source = "learnings"
target = "learnings"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/learnings/review.md"),
        "Use the learning pack.\n",
    );

    sync_all(temp.path(), cache.path());

    assert_eq!(
        fs::read_to_string(
            temp.path()
                .join(".nodus/packages/shared/learnings/review.md")
        )
        .unwrap(),
        "Use the learning pack.\n"
    );
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert_owned(&lockfile, temp.path(), ".nodus/packages/shared/learnings");
}

#[test]
fn sync_uses_dependency_alias_for_package_managed_exports_inside_virtual_plugin_roots() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["opencode"]

[dependencies.metrics_local]
path = "vendor/metrics"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/metrics"),
        r#"
name = "wenext-local-metrics"
opencode_plugin_hooks = [".opencode/plugins/metrics-collector.js"]

[[managed_exports]]
source = ".opencode/metrics"
target = "opencode-plugin/.opencode/metrics"
"#,
    );
    write_file(
        &temp
            .path()
            .join("vendor/metrics/.opencode/plugins/metrics-collector.js"),
        "export default function plugin() {}\n",
    );
    write_file(
        &temp
            .path()
            .join("vendor/metrics/.opencode/metrics/config.json"),
        "{\n  \"enabled\": true\n}\n",
    );

    sync_in_dir_with_adapters_no_fast_path(temp.path(), cache.path(), &[Adapter::OpenCode])
        .unwrap();

    assert_eq!(
        fs::read_to_string(
            temp.path().join(
                ".nodus/packages/metrics_local/opencode-plugin/.opencode/metrics/config.json"
            )
        )
        .unwrap(),
        "{\n  \"enabled\": true\n}\n"
    );
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let metrics_package = resolution
        .packages
        .iter()
        .find(|package| package.alias == "metrics_local")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), metrics_package, Adapter::OpenCode);
    assert!(plugin_root.join(".opencode/metrics/config.json").exists());
    assert!(
        !temp
            .path()
            .join(".nodus/packages/wenext-local-metrics")
            .exists(),
        "package-managed exports should use the dependency alias root, not the package name"
    );

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let metrics = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "metrics_local")
        .unwrap();
    assert!(
        metrics
            .owned_subtrees
            .iter()
            .any(|path| path == ".nodus/packages/metrics_local/opencode-plugin/.opencode/metrics"),
        "package-managed export subtree should remain project-owned; got {:?}",
        metrics.owned_subtrees
    );
    let plugin_root_relative = display_path(plugin_root.strip_prefix(temp.path()).unwrap());
    assert_not_owned(&lockfile, temp.path(), &plugin_root_relative);
    assert!(
        metrics
            .owned_subtrees
            .iter()
            .all(|path| path != ".nodus/packages/metrics_local/opencode-plugin"),
        "global virtual plugin roots should not be project-owned; got {:?}",
        metrics.owned_subtrees
    );
}

#[test]
fn sync_writes_project_scoped_package_managed_exports() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
[[managed_exports]]
source = "learnings"
target = "learnings"
placement = "project"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/learnings/review.md"),
        "Project-root learning.\n",
    );

    sync_all(temp.path(), cache.path());

    assert_eq!(
        fs::read_to_string(temp.path().join("learnings/review.md")).unwrap(),
        "Project-root learning.\n"
    );

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    // v10: project-placement managed_exports track the ownership root as an
    // exact owned file (so cleanup can prune extra files inside it) without
    // enumerating each individual planned file. Per-package coverage extends
    // from the root via `starts_with` semantics, so `learnings/review.md` is
    // still considered owned even though it isn't an explicit `owned_files`
    // entry.
    assert_owned(&lockfile, temp.path(), "learnings");
    let shared = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    assert!(
        !shared
            .owned_files
            .iter()
            .any(|path| path == "learnings/review.md"),
        "individual files inside a project-placement export root should not be enumerated separately; got owned_files = {:?}",
        shared.owned_files,
    );
}

#[test]
fn sync_prunes_stale_files_inside_project_scoped_managed_export_root() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
[[managed_exports]]
source = "learnings"
target = "learnings"
placement = "project"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/learnings/review.md"),
        "Project-root learning.\n",
    );
    write_file(
        &temp.path().join("vendor/shared/learnings/nested/tips.md"),
        "tips\n",
    );

    sync_all(temp.path(), cache.path());
    write_file(&temp.path().join("learnings/extra.md"), "stale\n");
    fs::remove_file(temp.path().join("vendor/shared/learnings/nested/tips.md")).unwrap();

    sync_all(temp.path(), cache.path());

    assert!(temp.path().join("learnings/review.md").exists());
    assert!(!temp.path().join("learnings/nested/tips.md").exists());
    assert!(!temp.path().join("learnings/extra.md").exists());
}

#[test]
fn sync_writes_package_managed_exports_from_export_only_dependency() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.metrics]
path = "vendor/metrics"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/metrics"),
        r#"
name = "wenext-local-metrics"

[[managed_exports]]
source = "plugins/metrics-collector.js"
target = ".opencode/plugins/metrics-collector.js"
placement = "project"

[[managed_exports]]
source = "metrics-config.json"
target = "metrics-config.json"
placement = "project"
"#,
    );
    write_file(
        &temp
            .path()
            .join("vendor/metrics/plugins/metrics-collector.js"),
        "export default function plugin() {}\n",
    );
    write_file(
        &temp.path().join("vendor/metrics/metrics-config.json"),
        "{\n  \"enabled\": true\n}\n",
    );

    sync_all(temp.path(), cache.path());

    assert_eq!(
        fs::read_to_string(temp.path().join(".opencode/plugins/metrics-collector.js")).unwrap(),
        "export default function plugin() {}\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("metrics-config.json")).unwrap(),
        "{\n  \"enabled\": true\n}\n"
    );
}

#[test]
fn sync_emits_transitive_package_managed_exports() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.wrapper]
path = "vendor/wrapper"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/wrapper"),
        r#"
[dependencies.leaf]
path = "vendor/leaf"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/wrapper/vendor/leaf"),
        r#"
[[managed_exports]]
source = "learnings"
target = "learnings"
"#,
    );
    write_skill(
        &temp.path().join("vendor/wrapper/skills/wrapper"),
        "Wrapper",
    );
    write_skill(
        &temp.path().join("vendor/wrapper/vendor/leaf/skills/leaf"),
        "Leaf",
    );
    write_file(
        &temp
            .path()
            .join("vendor/wrapper/vendor/leaf/learnings/review.md"),
        "Transitive learning.\n",
    );

    sync_all(temp.path(), cache.path());

    assert_eq!(
        fs::read_to_string(temp.path().join(".nodus/packages/leaf/learnings/review.md")).unwrap(),
        "Transitive learning.\n"
    );
}

#[test]
fn sync_writes_and_prunes_direct_managed_directory_targets() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "templates"
target = "docs/templates"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/templates/review.md"),
        "review template\n",
    );
    write_file(
        &temp.path().join("vendor/shared/templates/nested/tips.md"),
        "tips\n",
    );
    write_file(&temp.path().join("docs/templates/user.md"), "keep me\n");

    sync_all(temp.path(), cache.path());

    assert_eq!(
        fs::read_to_string(temp.path().join("docs/templates/review.md")).unwrap(),
        "review template\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("docs/templates/nested/tips.md")).unwrap(),
        "tips\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("docs/templates/user.md")).unwrap(),
        "keep me\n"
    );

    fs::remove_file(temp.path().join("vendor/shared/templates/nested/tips.md")).unwrap();
    sync_all(temp.path(), cache.path());

    assert!(temp.path().join("docs/templates/review.md").exists());
    assert!(!temp.path().join("docs/templates/nested/tips.md").exists());
    assert_eq!(
        fs::read_to_string(temp.path().join("docs/templates/user.md")).unwrap(),
        "keep me\n"
    );
}

#[test]
fn sync_migrates_subset_legacy_managed_paths_to_package_exports() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "learnings"
target = "learnings"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
[[managed_exports]]
source = "learnings"
target = "learnings"
placement = "project"

[[managed_exports]]
source = "prompts"
target = "prompts"
placement = "project"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/learnings/review.md"),
        "Migrated learning.\n",
    );
    write_file(
        &temp.path().join("vendor/shared/prompts/review.md"),
        "Migrated prompt.\n",
    );

    sync_all(temp.path(), cache.path());

    let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
    assert!(!manifest.contains("[[dependencies.shared.managed]]"));
    assert_eq!(
        fs::read_to_string(temp.path().join("learnings/review.md")).unwrap(),
        "Migrated learning.\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("prompts/review.md")).unwrap(),
        "Migrated prompt.\n"
    );
}

#[test]
fn sync_rejects_non_subset_legacy_managed_paths_when_package_exports_exist() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "learnings"
target = "docs/learnings"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
[[managed_exports]]
source = "learnings"
target = "learnings"
placement = "project"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/learnings/review.md"),
        "Mismatch.\n",
    );

    let error = sync_all_result(temp.path(), cache.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("managed_exports"));
    assert!(error.contains("remove the legacy root mappings"));
}

#[test]
fn sync_prunes_direct_managed_targets_when_mapping_is_removed() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/prompts/review.md"),
        "Use the review prompt.\n",
    );
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
    );

    sync_all(temp.path(), cache.path());
    assert!(temp.path().join(".github/prompts/review.md").exists());

    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    sync_all(temp.path(), cache.path());

    assert!(!temp.path().join(".github/prompts/review.md").exists());
}

#[test]
fn sync_rejects_unmanaged_collision_on_direct_managed_target() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/prompts/review.md"),
        "Use the review prompt.\n",
    );
    write_file(
        &temp.path().join(".github/prompts/review.md"),
        "user-owned prompt\n",
    );

    let error = sync_all_result(temp.path(), cache.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("refusing to overwrite unmanaged file"));
    assert!(error.contains(".github/prompts/review.md"));
    assert!(error.contains("remove the managed mapping from `nodus.toml`"));
}

#[test]
fn sync_can_adopt_unmanaged_collision_on_direct_managed_target() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/prompts/review.md"),
        "Use the review prompt.\n",
    );
    write_file(
        &temp.path().join(".github/prompts/review.md"),
        "user-owned prompt\n",
    );

    sync_in_dir_with_collision_choice(temp.path(), cache.path(), ManagedCollisionChoice::Adopt)
        .unwrap();

    assert_eq!(
        fs::read_to_string(temp.path().join(".github/prompts/review.md")).unwrap(),
        "Use the review prompt.\n"
    );
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert_owned(&lockfile, temp.path(), ".github/prompts/review.md");
}

#[test]
fn sync_force_overwrites_unmanaged_collision_on_direct_managed_target() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/prompts/review.md"),
        "Use the review prompt.\n",
    );
    write_file(
        &temp.path().join(".github/prompts/review.md"),
        "user-owned prompt\n",
    );

    sync_all_force_result(temp.path(), cache.path()).unwrap();

    assert_eq!(
        fs::read_to_string(temp.path().join(".github/prompts/review.md")).unwrap(),
        "Use the review prompt.\n"
    );
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert_owned(&lockfile, temp.path(), ".github/prompts/review.md");
}

#[test]
fn sync_can_remove_managed_mapping_after_unmanaged_collision() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/prompts/review.md"),
        "Use the review prompt.\n",
    );
    write_file(
        &temp.path().join(".github/prompts/review.md"),
        "user-owned prompt\n",
    );

    sync_in_dir_with_collision_choice(
        temp.path(),
        cache.path(),
        ManagedCollisionChoice::RemoveMapping,
    )
    .unwrap();

    assert_eq!(
        fs::read_to_string(temp.path().join(".github/prompts/review.md")).unwrap(),
        "user-owned prompt\n"
    );
    let manifest = fs::read_to_string(temp.path().join(MANIFEST_FILE)).unwrap();
    assert!(!manifest.contains("[[dependencies.shared.managed]]"));
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert_not_owned(&lockfile, temp.path(), ".github/prompts/review.md");
}

#[test]
fn sync_can_cancel_after_unmanaged_collision_prompt() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/prompts/review.md"),
        "Use the review prompt.\n",
    );
    write_file(
        &temp.path().join(".github/prompts/review.md"),
        "user-owned prompt\n",
    );

    let error = sync_in_dir_with_collision_choice(
        temp.path(),
        cache.path(),
        ManagedCollisionChoice::Cancel,
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("cancelled `nodus sync`"));
    assert!(error.contains(".github/prompts/review.md"));
}

#[test]
fn sync_rejects_overlapping_direct_managed_targets() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts"
target = "docs/prompts"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = "docs/prompts/review.md"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/prompts/review.md"),
        "Use the review prompt.\n",
    );

    let error = sync_all_result(temp.path(), cache.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("overlapping target roots"));
}

#[test]
fn sync_rejects_nested_dependency_managed_paths() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
wrapper = { path = "vendor/wrapper" }
"#,
    );
    write_file(
        &temp.path().join("vendor/wrapper/nodus.toml"),
        r#"
[dependencies.leaf]
path = "vendor/leaf"

[[dependencies.leaf.managed]]
source = "prompts/review.md"
target = "docs/review.md"
"#,
    );
    write_skill(
        &temp.path().join("vendor/wrapper/skills/wrapper"),
        "Wrapper",
    );
    write_skill(
        &temp.path().join("vendor/wrapper/vendor/leaf/skills/leaf"),
        "Leaf",
    );
    write_file(
        &temp
            .path()
            .join("vendor/wrapper/vendor/leaf/prompts/review.md"),
        "Use the review prompt.\n",
    );

    let error = sync_all_result(temp.path(), cache.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("supported only for direct dependencies in the root manifest"));
}

#[test]
fn sync_frozen_keeps_direct_managed_files_at_locked_git_revision() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let repo = TempDir::new().unwrap();
    write_skill(&repo.path().join("skills/review"), "Review");
    write_file(&repo.path().join("prompts/review.md"), "first revision\n");
    init_git_repo(repo.path());
    rename_current_branch(repo.path(), "main");

    write_manifest(
        temp.path(),
        &format!(
            r#"
[adapters]
enabled = ["codex"]

[dependencies.review_pkg]
url = "{}"
branch = "main"

[[dependencies.review_pkg.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
"#,
            toml_path_value(repo.path())
        ),
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join(".github/prompts/review.md")).unwrap(),
        "first revision\n"
    );

    write_file(&repo.path().join("prompts/review.md"), "second revision\n");
    commit_all(repo.path(), "advance");

    sync_in_dir_frozen(temp.path(), cache.path(), false).unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join(".github/prompts/review.md")).unwrap(),
        "first revision\n"
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join(".github/prompts/review.md")).unwrap(),
        "second revision\n"
    );
}

#[test]
fn doctor_detects_lockfile_drift_when_only_components_change() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/agents/shared.md"),
        "# Shared\n",
    );

    sync_all(temp.path(), cache.path());

    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["claude", "codex", "opencode"]

[dependencies]
shared = { path = "vendor/shared", components = ["skills"] }
"#,
    );

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Check,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Blocked);
    assert!(summary.findings.iter().any(|finding| {
        finding.kind == DoctorFindingKind::SafeAutoFix
            && finding.message.contains("run `nodus sync`")
    }));
}

#[test]
fn sync_prunes_disabled_dependencies_from_outputs_and_lockfile() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    sync_all(temp.path(), cache.path());

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_path = managed_skill_file(temp.path(), Adapter::Claude, dependency, "review");
    assert!(managed_skill_path.exists());

    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared", enabled = false }
"#,
    );

    sync_all(temp.path(), cache.path());

    assert!(managed_skill_path.exists());
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert!(
        !lockfile
            .packages
            .iter()
            .any(|package| package.alias == "shared")
    );
}

#[test]
fn sync_unions_component_selection_for_duplicate_package_references() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared_agents = { path = "vendor/shared", components = ["agents"] }
shared_skills = { path = "vendor/shared", components = ["skills"] }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/agents/shared.md"),
        "# Shared\n",
    );

    sync_all(temp.path(), cache.path());

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    assert_eq!(resolution.packages.len(), 2);
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias != "root")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    let managed_agent_file = namespaced_file_name(dependency, "shared", "md");
    assert_eq!(
        dependency.selected_components,
        Some(vec![
            DependencyComponent::Skills,
            DependencyComponent::Agents,
        ])
    );
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
    assert!(runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_agent_file
    ));

    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let shared = lockfile
        .packages
        .iter()
        .find(|package| package.alias != "root")
        .unwrap();
    assert_eq!(
        shared.selected_components,
        Some(vec![
            DependencyComponent::Skills,
            DependencyComponent::Agents,
        ])
    );
}

#[test]
fn sync_keeps_transitive_dependencies_when_parent_components_are_narrowed() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
wrapper = { path = "vendor/wrapper", components = ["skills"] }
"#,
    );
    write_file(
        &temp.path().join("vendor/wrapper/nodus.toml"),
        r#"
[dependencies]
leaf = { path = "vendor/leaf" }
"#,
    );
    write_file(
        &temp.path().join("vendor/wrapper/agents/wrapper.md"),
        "# Wrapper\n",
    );
    write_skill(
        &temp.path().join("vendor/wrapper/vendor/leaf/skills/checks"),
        "Checks",
    );

    sync_all(temp.path(), cache.path());

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let wrapper = resolution
        .packages
        .iter()
        .find(|package| package.alias == "wrapper")
        .unwrap();
    let leaf = resolution
        .packages
        .iter()
        .find(|package| package.alias == "leaf")
        .unwrap();
    let managed_wrapper_agent_file = namespaced_file_name(wrapper, "wrapper", "md");
    let managed_leaf_skill_id = namespaced_skill_id(leaf, "checks");

    assert_eq!(
        wrapper.selected_components,
        Some(vec![DependencyComponent::Skills])
    );
    assert!(!runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_wrapper_agent_file
    ));
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_leaf_skill_id
    ));
}

#[test]
fn sync_requires_opt_in_for_high_sensitivity_capabilities() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[[capabilities]]
id = "shell.exec"
sensitivity = "high"

[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    let error = sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &Adapter::ALL)
        .unwrap_err()
        .to_string();
    assert!(error.contains("--allow-high-sensitivity"));

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, true, &Adapter::ALL).unwrap();
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
}

#[test]
fn sync_keeps_unique_dependency_skill_ids_unsuffixed() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let (_repo, url) = create_git_dependency();

    add_dependency_all(temp.path(), cache.path(), &url, Some("v0.1.0"));
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| matches!(package.source, PackageSource::Git { .. }))
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");

    sync_all(temp.path(), cache.path());

    assert_eq!(managed_skill_id, "review");
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Claude,
        &managed_skill_id
    ));
}

#[test]
fn sync_prunes_stale_managed_files() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();

    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/agents/security.md"),
        "# Security\n",
    );
    write_file(
        &temp.path().join("vendor/shared/rules/default.rules"),
        "allow = []\n",
    );
    write_file(
        &temp.path().join("vendor/shared/commands/build.txt"),
        "cargo test\n",
    );

    sync_all(temp.path(), cache.path());
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_agent_file = namespaced_file_name(dependency, "security", "md");
    let managed_command_file = namespaced_file_name(dependency, "build", "md");
    let managed_rule_file = namespaced_file_name(dependency, "default", "md");
    assert!(runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_agent_file
    ));
    assert!(runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_command_file
    ));
    assert!(runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_rule_file
    ));
    assert!(
        temp.path()
            .join(format!(".opencode/agents/{managed_agent_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/rules/{managed_rule_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/commands/{managed_command_file}"))
            .exists()
    );

    fs::remove_file(temp.path().join("vendor/shared/agents/security.md")).unwrap();
    fs::remove_dir(temp.path().join("vendor/shared/agents")).unwrap();
    fs::remove_file(temp.path().join("vendor/shared/rules/default.rules")).unwrap();
    fs::remove_dir(temp.path().join("vendor/shared/rules")).unwrap();
    fs::remove_file(temp.path().join("vendor/shared/commands/build.txt")).unwrap();
    fs::remove_dir(temp.path().join("vendor/shared/commands")).unwrap();
    sync_all(temp.path(), cache.path());

    let updated_resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let updated_dependency = updated_resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), updated_dependency, Adapter::Claude);
    let plugin: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(plugin_root.join(".claude-plugin/plugin.json")).unwrap(),
    )
    .unwrap();
    assert!(plugin.get("agents").is_none());
    assert!(plugin.get("commands").is_none());
    assert!(plugin.get("rules").is_none());
    assert!(
        !temp
            .path()
            .join(format!(".opencode/agents/{managed_agent_file}"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(".opencode/rules/{managed_rule_file}"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(".opencode/commands/{managed_command_file}"))
            .exists()
    );
}

#[test]
fn recover_runtime_owned_paths_from_disk_requires_existing_matching_path_state() {
    let temp = TempDir::new().unwrap();
    let project_root = temp.path();
    let skill_dir = project_root.join(".claude/skills/review_abc123");
    let github_skill_dir = project_root.join(".github/skills/review_abc123");
    let github_agent_file = project_root.join(".github/agents/security_abc123.agent.md");
    let prompt_file = project_root.join(".github/prompts/review.md");
    let desired_paths = [
        skill_dir.clone(),
        github_skill_dir.clone(),
        github_agent_file.clone(),
        prompt_file.clone(),
    ]
    .into_iter()
    .collect::<HashSet<_>>();
    write_file(&skill_dir.join("SKILL.md"), "# Review\n");
    write_file(&github_skill_dir.join("SKILL.md"), "# Review\n");
    write_file(&github_agent_file, "# Security\n");

    let planned_files = vec![
        ManagedFile {
            path: skill_dir.join("SKILL.md"),
            contents: b"# Review\n".to_vec(),
        },
        ManagedFile {
            path: github_skill_dir.join("SKILL.md"),
            contents: b"# Review\n".to_vec(),
        },
        ManagedFile {
            path: github_agent_file.clone(),
            contents: b"# Security\n".to_vec(),
        },
    ];

    let recovered = super::support::recover_runtime_owned_paths_from_disk(
        project_root,
        &desired_paths,
        &planned_files,
    );

    assert!(recovered.contains(&skill_dir));
    assert!(recovered.contains(&github_skill_dir));
    assert!(recovered.contains(&github_agent_file));
    assert!(!recovered.contains(&prompt_file));
}

#[test]
fn recover_runtime_owned_paths_from_disk_rejects_partial_directory_matches() {
    let temp = TempDir::new().unwrap();
    let project_root = temp.path();
    let learnings_dir = project_root.join(".nodus/packages/shared/learnings");
    let desired_paths = [learnings_dir.clone()].into_iter().collect::<HashSet<_>>();
    write_file(&learnings_dir.join("review.md"), "Use the learning pack.\n");
    write_file(&learnings_dir.join("tips.md"), "user-authored override\n");

    let planned_files = vec![
        ManagedFile {
            path: learnings_dir.join("review.md"),
            contents: b"Use the learning pack.\n".to_vec(),
        },
        ManagedFile {
            path: learnings_dir.join("tips.md"),
            contents: b"Use the tips pack.\n".to_vec(),
        },
    ];

    let recovered = super::support::recover_runtime_owned_paths_from_disk(
        project_root,
        &desired_paths,
        &planned_files,
    );

    assert!(!recovered.contains(&learnings_dir));
}

#[test]
fn recover_runtime_owned_paths_from_disk_rejects_extra_empty_subdirectories() {
    let temp = TempDir::new().unwrap();
    let project_root = temp.path();
    let learnings_dir = project_root.join(".nodus/packages/shared/learnings");
    let desired_paths = [learnings_dir.clone()].into_iter().collect::<HashSet<_>>();
    write_file(&learnings_dir.join("review.md"), "Use the learning pack.\n");
    write_file(&learnings_dir.join("tips.md"), "Use the tips pack.\n");
    fs::create_dir_all(learnings_dir.join("extra")).unwrap();

    let planned_files = vec![
        ManagedFile {
            path: learnings_dir.join("review.md"),
            contents: b"Use the learning pack.\n".to_vec(),
        },
        ManagedFile {
            path: learnings_dir.join("tips.md"),
            contents: b"Use the tips pack.\n".to_vec(),
        },
    ];

    let recovered = super::support::recover_runtime_owned_paths_from_disk(
        project_root,
        &desired_paths,
        &planned_files,
    );

    assert!(!recovered.contains(&learnings_dir));
}

#[test]
fn recover_runtime_owned_paths_from_disk_rejects_symlinked_candidates() {
    let temp = TempDir::new().unwrap();
    let project_root = temp.path();

    let file_target = project_root.join("real-review.md");
    write_file(&file_target, "Use the learning pack.\n");
    let symlinked_file = project_root.join(".nodus/packages/shared/learnings/review.md");
    fs::create_dir_all(symlinked_file.parent().unwrap()).unwrap();
    if !create_symlink(&file_target, &symlinked_file) {
        return;
    }

    let dir_target = project_root.join("real-learnings");
    write_file(&dir_target.join("review.md"), "Use the learning pack.\n");
    write_file(&dir_target.join("tips.md"), "Use the tips pack.\n");
    let symlinked_dir = project_root.join(".nodus/packages/other/learnings");
    fs::create_dir_all(symlinked_dir.parent().unwrap()).unwrap();
    if !create_symlink(&dir_target, &symlinked_dir) {
        return;
    }

    let desired_paths = [symlinked_file.clone(), symlinked_dir.clone()]
        .into_iter()
        .collect::<HashSet<_>>();
    let planned_files = vec![
        ManagedFile {
            path: symlinked_file.clone(),
            contents: b"Use the learning pack.\n".to_vec(),
        },
        ManagedFile {
            path: symlinked_dir.join("review.md"),
            contents: b"Use the learning pack.\n".to_vec(),
        },
        ManagedFile {
            path: symlinked_dir.join("tips.md"),
            contents: b"Use the tips pack.\n".to_vec(),
        },
    ];

    let recovered = super::support::recover_runtime_owned_paths_from_disk(
        project_root,
        &desired_paths,
        &planned_files,
    );

    assert!(!recovered.contains(&symlinked_file));
    assert!(!recovered.contains(&symlinked_dir));
}

#[test]
fn recover_runtime_owned_paths_from_disk_rejects_candidates_under_symlinked_parents() {
    let temp = TempDir::new().unwrap();
    let project_root = temp.path();

    let real_dir = project_root.join("real-learnings");
    write_file(&real_dir.join("review.md"), "Use the learning pack.\n");
    let symlinked_parent = project_root.join(".nodus/packages/shared/learnings");
    fs::create_dir_all(symlinked_parent.parent().unwrap()).unwrap();
    if !create_symlink(&real_dir, &symlinked_parent) {
        return;
    }

    let managed_file = symlinked_parent.join("review.md");
    let desired_paths = [managed_file.clone()].into_iter().collect::<HashSet<_>>();
    let planned_files = vec![ManagedFile {
        path: managed_file.clone(),
        contents: b"Use the learning pack.\n".to_vec(),
    }];

    let recovered = super::support::recover_runtime_owned_paths_from_disk(
        project_root,
        &desired_paths,
        &planned_files,
    );

    assert!(!recovered.contains(&managed_file));
}

#[test]
fn recover_runtime_owned_paths_from_disk_accepts_exact_package_export_directories() {
    let temp = TempDir::new().unwrap();
    let project_root = temp.path();
    let learnings_dir = project_root.join(".nodus/packages/shared/learnings");
    let desired_paths = [learnings_dir.clone()].into_iter().collect::<HashSet<_>>();
    write_file(&learnings_dir.join("review.md"), "Use the learning pack.\n");
    write_file(&learnings_dir.join("tips.md"), "Use the tips pack.\n");

    let planned_files = vec![
        ManagedFile {
            path: learnings_dir.join("review.md"),
            contents: b"Use the learning pack.\n".to_vec(),
        },
        ManagedFile {
            path: learnings_dir.join("tips.md"),
            contents: b"Use the tips pack.\n".to_vec(),
        },
    ];

    let recovered = super::support::recover_runtime_owned_paths_from_disk(
        project_root,
        &desired_paths,
        &planned_files,
    );

    assert!(recovered.contains(&learnings_dir));
}

#[test]
fn recover_runtime_owned_paths_from_disk_accepts_exact_single_file_outputs() {
    let temp = TempDir::new().unwrap();
    let project_root = temp.path();
    let managed_file = project_root.join(".nodus/packages/shared/learnings/review.md");
    let desired_paths = [managed_file.clone()].into_iter().collect::<HashSet<_>>();
    write_file(&managed_file, "Use the learning pack.\n");

    let planned_files = vec![ManagedFile {
        path: managed_file.clone(),
        contents: b"Use the learning pack.\n".to_vec(),
    }];

    let recovered = super::support::recover_runtime_owned_paths_from_disk(
        project_root,
        &desired_paths,
        &planned_files,
    );

    assert!(recovered.contains(&managed_file));
}

#[test]
fn recover_runtime_owned_paths_from_disk_accepts_exact_file_in_mixed_runtime_directory() {
    let temp = TempDir::new().unwrap();
    let project_root = temp.path();
    let build_file = project_root.join(".claude/commands/build.md");
    let review_file = project_root.join(".claude/commands/review.md");
    let plan_file = project_root.join(".claude/commands/plan.md");
    let desired_paths = [build_file.clone(), review_file.clone(), plan_file.clone()]
        .into_iter()
        .collect::<HashSet<_>>();
    write_file(&build_file, "# Build\n");
    write_file(&review_file, "# Review\n");
    write_file(&plan_file, "user-authored plan\n");
    write_file(
        &project_root.join(".claude/commands/index.md"),
        "user-owned index\n",
    );

    let planned_files = vec![
        ManagedFile {
            path: build_file.clone(),
            contents: b"# Build\n".to_vec(),
        },
        ManagedFile {
            path: review_file.clone(),
            contents: b"# Review\n".to_vec(),
        },
        ManagedFile {
            path: plan_file.clone(),
            contents: b"# Plan\n".to_vec(),
        },
    ];

    let recovered = super::support::recover_runtime_owned_paths_from_disk(
        project_root,
        &desired_paths,
        &planned_files,
    );

    assert!(recovered.contains(&build_file));
    assert!(recovered.contains(&review_file));
    assert!(!recovered.contains(&plan_file));
}

#[test]
fn prune_empty_parent_dirs_stops_at_github_root() {
    let temp = TempDir::new().unwrap();
    let skill_dir = temp.path().join(".github/skills/review_abc123");
    let skill_file = skill_dir.join("SKILL.md");
    write_file(&skill_file, "# Review\n");

    fs::remove_file(&skill_file).unwrap();
    prune_empty_parent_dirs(&skill_file, temp.path()).unwrap();

    assert!(temp.path().join(".github").exists());
    assert!(!temp.path().join(".github/skills").exists());
    assert!(!skill_dir.exists());
}

#[test]
fn sync_preserves_user_owned_root_instruction_files() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_file(
        &temp.path().join("vendor/shared/rules/default.rules"),
        "allow = []\n",
    );
    write_file(&temp.path().join("CLAUDE.md"), "user-owned memory\n");
    write_file(&temp.path().join("AGENTS.md"), "user-owned agents\n");

    sync_all(temp.path(), cache.path());

    assert_eq!(
        fs::read_to_string(temp.path().join("CLAUDE.md")).unwrap(),
        "user-owned memory\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("AGENTS.md")).unwrap(),
        "user-owned agents\n"
    );
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_rule_file = namespaced_file_name(dependency, "default", "md");
    assert!(runtime_file_exists(
        temp.path(),
        Adapter::Claude,
        &managed_rule_file
    ));
}

#[test]
fn sync_namespaces_duplicate_opencode_skill_ids_across_packages() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
other = { path = "vendor/other" }
"#,
    );
    write_file(
        &temp.path().join("vendor/shared/skills/review/SKILL.md"),
        "---\nname: Shared Review\ndescription: Different review skill.\n---\n# Shared Review\n",
    );
    write_file(
        &temp.path().join("vendor/other/skills/review/SKILL.md"),
        "---\nname: Other Review\ndescription: Another review skill.\n---\n# Other Review\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &Adapter::ALL).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let other = resolution
        .packages
        .iter()
        .find(|package| package.alias == "other")
        .unwrap();
    let shared_skill_id = resolution_skill_id(&resolution, shared, "review");
    let other_skill_id = resolution_skill_id(&resolution, other, "review");

    assert_ne!(shared_skill_id, other_skill_id);
    assert!(
        temp.path()
            .join(format!(".github/skills/{shared_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".github/skills/{other_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/skills/{shared_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/skills/{other_skill_id}/SKILL.md"))
            .exists()
    );
}

#[test]
fn sync_namespaces_duplicate_file_ids_across_packages() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
other = { path = "vendor/other" }
"#,
    );
    write_file(
        &temp.path().join("vendor/shared/agents/security.md"),
        "# Shared Security\n",
    );
    write_file(
        &temp.path().join("vendor/shared/rules/default.rules"),
        "allow = []\n",
    );
    write_file(
        &temp.path().join("vendor/shared/commands/build.txt"),
        "cargo test\n",
    );
    write_file(
        &temp.path().join("vendor/other/agents/security.md"),
        "# Other Security\n",
    );
    write_file(
        &temp.path().join("vendor/other/rules/default.rules"),
        "deny = []\n",
    );
    write_file(
        &temp.path().join("vendor/other/commands/build.txt"),
        "cargo check\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &Adapter::ALL).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let other = resolution
        .packages
        .iter()
        .find(|package| package.alias == "other")
        .unwrap();

    let shared_agent_file =
        resolution_file_name(&resolution, shared, ArtifactKind::Agent, "security", "md");
    let other_agent_file =
        resolution_file_name(&resolution, other, ArtifactKind::Agent, "security", "md");
    let shared_copilot_agent_file = resolution_file_name(
        &resolution,
        shared,
        ArtifactKind::Agent,
        "security",
        "agent.md",
    );
    let other_copilot_agent_file = resolution_file_name(
        &resolution,
        other,
        ArtifactKind::Agent,
        "security",
        "agent.md",
    );
    let shared_command_file =
        resolution_file_name(&resolution, shared, ArtifactKind::Command, "build", "md");
    let other_command_file =
        resolution_file_name(&resolution, other, ArtifactKind::Command, "build", "md");
    let shared_codex_command_skill =
        resolution_codex_command_skill_id(&resolution, shared, "build");
    let other_codex_command_skill = resolution_codex_command_skill_id(&resolution, other, "build");
    let shared_claude_rule_file =
        resolution_file_name(&resolution, shared, ArtifactKind::Rule, "default", "md");
    let other_claude_rule_file =
        resolution_file_name(&resolution, other, ArtifactKind::Rule, "default", "md");

    assert_ne!(shared_agent_file, other_agent_file);
    assert_ne!(shared_copilot_agent_file, other_copilot_agent_file);
    assert_ne!(shared_command_file, other_command_file);
    assert_eq!(
        shared_codex_command_skill, other_codex_command_skill,
        "Codex command skill ids are local to each global plugin snapshot"
    );
    assert_ne!(shared_claude_rule_file, other_claude_rule_file);

    assert_eq!(
        runtime_file_paths(temp.path(), Adapter::Claude, "security.md").len(),
        2
    );
    assert_eq!(
        runtime_file_paths(temp.path(), Adapter::Claude, "build.md").len(),
        2
    );
    assert_eq!(
        runtime_file_paths(temp.path(), Adapter::Claude, "default.md").len(),
        2
    );
    assert!(
        temp.path()
            .join(format!(".github/agents/{shared_copilot_agent_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".github/agents/{other_copilot_agent_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/agents/{shared_agent_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/agents/{other_agent_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/commands/{shared_command_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/commands/{other_command_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/rules/{shared_claude_rule_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/rules/{other_claude_rule_file}"))
            .exists()
    );
    assert!(runtime_skill_exists(
        temp.path(),
        Adapter::Codex,
        &shared_codex_command_skill
    ));
    assert_eq!(
        runtime_skill_paths(temp.path(), Adapter::Codex, &other_codex_command_skill).len(),
        2
    );
}

#[test]
fn sync_prunes_old_skill_directories_when_digest_changes() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    sync_all(temp.path(), cache.path());

    let first_resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let first_dependency = first_resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let first_skill_id = namespaced_skill_id(first_dependency, "review");
    let first_skill_dir =
        managed_skill_file(temp.path(), Adapter::Claude, first_dependency, "review")
            .parent()
            .unwrap()
            .to_path_buf();
    assert!(first_skill_dir.exists());

    write_file(
        &temp.path().join("vendor/shared/skills/review/SKILL.md"),
        "---\nname: Review\ndescription: Updated review skill.\n---\n# Review\nchanged\n",
    );

    sync_all(temp.path(), cache.path());

    let second_resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let second_dependency = second_resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let second_skill_id = namespaced_skill_id(second_dependency, "review");
    let second_skill_dir =
        managed_skill_file(temp.path(), Adapter::Claude, second_dependency, "review")
            .parent()
            .unwrap()
            .to_path_buf();

    assert_eq!(first_skill_id, second_skill_id);
    assert!(second_skill_dir.exists());
    assert!(first_skill_dir.exists());
}

#[test]
fn doctor_detects_missing_file_inside_managed_skill_directory() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    sync_all(temp.path(), cache.path());

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_path = managed_skill_file(temp.path(), Adapter::Claude, dependency, "review");
    fs::remove_file(&managed_skill_path).unwrap();

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Check,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Blocked);
    assert!(summary.findings.iter().any(|finding| {
        finding.kind == DoctorFindingKind::SafeAutoFix
            && finding
                .message
                .contains("managed file is missing from disk")
    }));
}

#[test]
fn doctor_check_mode_reports_read_only_status() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    sync_all(temp.path(), cache.path());

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Check,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Healthy);
    assert!(summary.applied_actions.is_empty());
}

#[test]
fn doctor_check_mode_keeps_missing_managed_file_as_unfixed_finding() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    sync_all(temp.path(), cache.path());
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_path = managed_skill_file(temp.path(), Adapter::Claude, dependency, "review");
    fs::remove_file(&managed_skill_path).unwrap();

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Check,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Blocked);
    assert!(summary.findings.iter().any(|finding| {
        finding.kind == DoctorFindingKind::SafeAutoFix
            && finding
                .message
                .contains("managed file is missing from disk")
    }));
    assert!(!managed_skill_path.exists());
}

#[test]
fn doctor_repairs_missing_file_inside_managed_skill_directory() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    sync_all(temp.path(), cache.path());
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_path = managed_skill_file(temp.path(), Adapter::Claude, dependency, "review");
    fs::remove_file(&managed_skill_path).unwrap();

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Fixed);
    assert!(managed_skill_path.exists());
}

#[test]
fn doctor_check_and_repair_missing_codex_virtual_package_output() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["codex"]

[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let plugin_root = global_native_plugin_root(temp.path(), shared, Adapter::Codex);
    fs::remove_dir_all(&plugin_root).unwrap();

    let check = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Check,
        &Reporter::silent(),
    )
    .unwrap();
    assert_eq!(check.status, DoctorStatus::Blocked);
    assert!(check.findings.iter().any(|finding| {
        let message = finding.message.replace('\\', "/");
        finding.kind == DoctorFindingKind::SafeAutoFix
            && message.contains(".nodus-global/packages/")
            && message.contains("codex-plugin")
    }));
    assert!(!plugin_root.exists());

    let repair = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();
    assert_eq!(repair.status, DoctorStatus::Fixed);
    assert!(plugin_root.exists());
}

#[test]
fn doctor_repairs_invalid_managed_mcp_json_when_it_owns_the_file() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        "[dependencies.firebase]\npath = \"vendor/firebase\"\n",
    );
    write_file(
        &temp.path().join("vendor/firebase/nodus.toml"),
        "[mcp_servers.firebase]\ncommand = \"npx\"\n",
    );
    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    write_file(&temp.path().join(".mcp.json"), "{");

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Fixed);
    assert!(
        summary
            .applied_actions
            .iter()
            .any(|action| action.message.contains("rewrote managed output"))
    );
}

#[test]
fn doctor_repairs_invalid_managed_claude_settings_when_it_owns_the_file() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["claude"]

[launch_hooks]
sync_on_startup = true
"#,
    );

    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    write_file(&temp.path().join(".claude/settings.json"), "{");

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Fixed);
    assert!(
        summary
            .applied_actions
            .iter()
            .any(|action| action.message.contains("rewrote managed output"))
    );
}

#[test]
fn doctor_repairs_invalid_managed_codex_config_when_it_owns_the_file() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["codex"]

[launch_hooks]
sync_on_startup = true
"#,
    );
    sync_in_dir(temp.path(), cache.path(), false, false).unwrap();

    write_file(&temp.path().join(".codex/config.toml"), "[mcp_servers");

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Fixed);
    assert!(
        summary
            .applied_actions
            .iter()
            .any(|action| action.message.contains("rewrote managed output"))
    );
}

#[test]
fn doctor_repairs_invalid_managed_opencode_config_when_it_owns_the_file() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        "[dependencies.firebase]\npath = \"vendor/firebase\"\n",
    );
    write_file(
        &temp.path().join("vendor/firebase/nodus.toml"),
        "[mcp_servers.firebase]\ncommand = \"npx\"\n",
    );
    sync_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        false,
        false,
        &[Adapter::OpenCode],
    )
    .unwrap();

    write_file(&temp.path().join("opencode.json"), "{");

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Fixed);
    assert!(
        summary
            .applied_actions
            .iter()
            .any(|action| action.message.contains("rewrote managed output"))
    );
}

#[test]
fn doctor_missing_lockfile_with_unmanaged_collision_still_blocks_repair() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        "[dependencies.firebase]\npath = \"vendor/firebase\"\n",
    );
    write_file(
        &temp.path().join("vendor/firebase/nodus.toml"),
        "[mcp_servers.firebase]\ncommand = \"npx\"\n",
    );
    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    fs::remove_file(temp.path().join(LOCKFILE_NAME)).unwrap();
    write_file(
        &temp.path().join(".mcp.json"),
        r#"{
  "mcpServers": {
    "local": {
      "command": "node"
    }
  }
}
"#,
    );

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Blocked);
    assert!(summary.findings.iter().any(|finding| {
        finding.kind == DoctorFindingKind::RiskyFix && finding.message.contains(".mcp.json")
    }));
    assert!(!temp.path().join(LOCKFILE_NAME).exists());
}

#[test]
fn doctor_missing_lockfile_with_partial_multi_file_managed_directory_blocks_repair() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
[[managed_exports]]
source = "learnings"
target = "learnings"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/learnings/review.md"),
        "Use the learning pack.\n",
    );
    write_file(
        &temp.path().join("vendor/shared/learnings/tips.md"),
        "Use the tips pack.\n",
    );

    sync_all(temp.path(), cache.path());

    fs::remove_file(temp.path().join(LOCKFILE_NAME)).unwrap();
    write_file(
        &temp.path().join(".nodus/packages/shared/learnings/tips.md"),
        "user-authored override\n",
    );

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Blocked);
    assert!(
        summary
            .findings
            .iter()
            .any(|finding| finding.kind == DoctorFindingKind::RiskyFix)
    );
    assert!(!temp.path().join(LOCKFILE_NAME).exists());
    assert_eq!(
        fs::read_to_string(temp.path().join(".nodus/packages/shared/learnings/tips.md")).unwrap(),
        "user-authored override\n"
    );
}

#[test]
fn doctor_missing_lockfile_rewrites_global_workspace_marketplace() {
    let repo = create_workspace_dependency();
    let cache = cache_dir();

    sync_in_dir_with_adapters(repo.path(), cache.path(), false, false, &[Adapter::Claude]).unwrap();
    fs::remove_file(repo.path().join(LOCKFILE_NAME)).unwrap();
    write_file(
        &generated_claude_marketplace_path(repo.path()),
        "user-authored marketplace\n",
    );

    let summary = doctor_in_dir_with_mode(
        repo.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Fixed);
    assert!(repo.path().join(LOCKFILE_NAME).exists());
    assert_ne!(
        fs::read_to_string(generated_claude_marketplace_path(repo.path())).unwrap(),
        "user-authored marketplace\n"
    );
}

#[test]
fn doctor_recovers_exact_match_workspace_marketplace_after_lockfile_loss() {
    let repo = create_workspace_dependency();
    let cache = cache_dir();

    sync_in_dir_with_adapters(repo.path(), cache.path(), false, false, &[Adapter::Claude]).unwrap();
    let expected_marketplace =
        fs::read_to_string(generated_claude_marketplace_path(repo.path())).unwrap();
    fs::remove_file(repo.path().join(LOCKFILE_NAME)).unwrap();

    let summary = doctor_in_dir_with_mode(
        repo.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Fixed);
    assert!(repo.path().join(LOCKFILE_NAME).exists());
    assert_eq!(
        fs::read_to_string(generated_claude_marketplace_path(repo.path())).unwrap(),
        expected_marketplace
    );
}

#[test]
fn doctor_missing_lockfile_with_extra_empty_subdir_in_managed_directory_blocks_repair() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
[[managed_exports]]
source = "learnings"
target = "learnings"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/learnings/review.md"),
        "Use the learning pack.\n",
    );
    write_file(
        &temp.path().join("vendor/shared/learnings/tips.md"),
        "Use the tips pack.\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();
    fs::remove_file(temp.path().join(LOCKFILE_NAME)).unwrap();
    let _ = fs::remove_dir_all(temp.path().join(".codex"));
    let _ = fs::remove_dir_all(temp.path().join(".nodus/packages/shared/codex-plugin"));
    let _ = fs::remove_dir_all(temp.path().join(".agents"));
    fs::create_dir_all(temp.path().join(".nodus/packages/shared/learnings/extra")).unwrap();

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Blocked);
    assert!(
        summary
            .findings
            .iter()
            .any(|finding| finding.kind == DoctorFindingKind::RiskyFix)
    );
    assert!(!temp.path().join(LOCKFILE_NAME).exists());
    assert!(
        temp.path()
            .join(".nodus/packages/shared/learnings/extra")
            .exists()
    );
}

#[test]
fn doctor_missing_lockfile_with_symlinked_managed_file_blocks_repair() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
[[managed_exports]]
source = "learnings/review.md"
target = "learnings/review.md"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/learnings/review.md"),
        "Use the learning pack.\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();
    fs::remove_file(temp.path().join(LOCKFILE_NAME)).unwrap();
    let _ = fs::remove_dir_all(temp.path().join(".codex"));
    let _ = fs::remove_dir_all(temp.path().join(".nodus/packages/shared/codex-plugin"));
    let _ = fs::remove_dir_all(temp.path().join(".agents"));

    let real_target = temp.path().join("real-review.md");
    write_file(&real_target, "Use the learning pack.\n");
    let managed_path = temp
        .path()
        .join(".nodus/packages/shared/learnings/review.md");
    fs::remove_file(&managed_path).unwrap();
    if !create_symlink(&real_target, &managed_path) {
        return;
    }

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Blocked);
    assert!(
        summary
            .findings
            .iter()
            .any(|finding| finding.kind == DoctorFindingKind::RiskyFix)
    );
    assert!(!temp.path().join(LOCKFILE_NAME).exists());
    assert!(
        fs::symlink_metadata(&managed_path)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
    );
}

#[test]
fn doctor_missing_lockfile_with_managed_file_under_symlinked_parent_blocks_repair() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
[[managed_exports]]
source = "learnings/review.md"
target = "learnings/review.md"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/learnings/review.md"),
        "Use the learning pack.\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();
    fs::remove_file(temp.path().join(LOCKFILE_NAME)).unwrap();
    let _ = fs::remove_dir_all(temp.path().join(".codex"));

    let real_dir = temp.path().join("real-learnings");
    write_file(&real_dir.join("review.md"), "Use the learning pack.\n");
    let managed_parent = temp.path().join(".nodus/packages/shared/learnings");
    fs::remove_dir_all(&managed_parent).unwrap();
    if !create_symlink(&real_dir, &managed_parent) {
        return;
    }

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Blocked);
    assert!(
        summary
            .findings
            .iter()
            .any(|finding| finding.kind == DoctorFindingKind::RiskyFix)
    );
    assert!(!temp.path().join(LOCKFILE_NAME).exists());
    assert!(
        fs::symlink_metadata(&managed_parent)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
    );
}

#[test]
fn doctor_recovers_exact_match_package_export_directory_after_lockfile_loss() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
[[managed_exports]]
source = "learnings"
target = "learnings"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/learnings/review.md"),
        "Use the learning pack.\n",
    );
    write_file(
        &temp.path().join("vendor/shared/learnings/tips.md"),
        "Use the tips pack.\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();
    fs::remove_file(temp.path().join(LOCKFILE_NAME)).unwrap();
    let _ = fs::remove_dir_all(temp.path().join(".codex"));
    let _ = fs::remove_dir_all(temp.path().join(".nodus/packages/shared/codex-plugin"));
    let _ = fs::remove_dir_all(temp.path().join(".agents"));

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Fixed);
    assert!(temp.path().join(LOCKFILE_NAME).exists());
    assert_eq!(
        fs::read_to_string(
            temp.path()
                .join(".nodus/packages/shared/learnings/review.md")
        )
        .unwrap(),
        "Use the learning pack.\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join(".nodus/packages/shared/learnings/tips.md")).unwrap(),
        "Use the tips pack.\n"
    );
}

#[test]
fn doctor_recovers_exact_match_package_export_file_after_lockfile_loss() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.shared]
path = "vendor/shared"
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
[[managed_exports]]
source = "learnings/review.md"
target = "learnings/review.md"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/learnings/review.md"),
        "Use the learning pack.\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();
    fs::remove_file(temp.path().join(LOCKFILE_NAME)).unwrap();
    let _ = fs::remove_dir_all(temp.path().join(".codex"));
    let _ = fs::remove_dir_all(temp.path().join(".nodus/packages/shared/codex-plugin"));
    let _ = fs::remove_dir_all(temp.path().join(".agents"));

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Fixed);
    assert!(temp.path().join(LOCKFILE_NAME).exists());
    assert_eq!(
        fs::read_to_string(
            temp.path()
                .join(".nodus/packages/shared/learnings/review.md")
        )
        .unwrap(),
        "Use the learning pack.\n"
    );
}

#[test]
fn doctor_blocks_invalid_mcp_json_without_lockfile_when_ownership_is_ambiguous() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        "[dependencies.firebase]\npath = \"vendor/firebase\"\n",
    );
    write_file(
        &temp.path().join("vendor/firebase/nodus.toml"),
        "[mcp_servers.firebase]\ncommand = \"npx\"\n",
    );
    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    fs::remove_file(temp.path().join(LOCKFILE_NAME)).unwrap();
    write_file(&temp.path().join(".mcp.json"), "{");

    let error = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("failed to parse MCP config"));
    assert!(!temp.path().join(LOCKFILE_NAME).exists());
}

#[test]
fn doctor_check_mode_reports_risky_cleanup_without_deleting_anything() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();
    fs::remove_file(temp.path().join(LOCKFILE_NAME)).unwrap();
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let codex_plugin_skills = managed_skill_file(temp.path(), Adapter::Codex, shared, "review")
        .parent()
        .unwrap()
        .to_path_buf();
    fs::remove_dir_all(&codex_plugin_skills).unwrap();
    write_file(&codex_plugin_skills, "user-owned file\n");

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Check,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Blocked);
    assert!(
        summary
            .findings
            .iter()
            .any(|finding| finding.kind == DoctorFindingKind::RiskyFix)
    );
    assert!(codex_plugin_skills.is_file());
}

#[test]
fn doctor_force_mode_applies_risky_cleanup_without_prompt() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();
    fs::remove_file(temp.path().join(LOCKFILE_NAME)).unwrap();
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let shared = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let codex_plugin_skills = managed_skill_file(temp.path(), Adapter::Codex, shared, "review")
        .parent()
        .unwrap()
        .to_path_buf();
    fs::remove_dir_all(&codex_plugin_skills).unwrap();
    write_file(&codex_plugin_skills, "user-owned file\n");

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Force,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Fixed);
    assert!(summary.applied_actions.iter().any(|action| {
        action
            .message
            .contains("removed conflicting managed subtree")
    }));
    assert!(!codex_plugin_skills.is_file());
    assert!(codex_plugin_skills.join("SKILL.md").exists());
    assert!(temp.path().join(LOCKFILE_NAME).exists());
}

#[test]
fn doctor_detects_lockfile_drift() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    sync_all(temp.path(), cache.path());

    write_skill(&temp.path().join("skills/renamed"), "Renamed");

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Check,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Blocked);
    assert!(summary.findings.iter().any(|finding| {
        finding.kind == DoctorFindingKind::SafeAutoFix
            && finding.message.contains("run `nodus sync`")
    }));
}

#[test]
fn existing_lockfile_resolution_accepts_lockfile_drift_for_baseline_checks() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    sync_all(temp.path(), cache.path());

    write_skill(&temp.path().join("skills/renamed"), "Renamed");

    let (resolution, lockfile) =
        resolve_project_from_existing_lockfile_in_dir(temp.path(), cache.path(), &Adapter::ALL)
            .unwrap();
    assert!(!resolution.packages.is_empty());
    assert!(!lockfile.packages.is_empty());
}

#[test]
fn doctor_accepts_legacy_detected_adapter_roots_without_manifest_config() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    fs::create_dir_all(temp.path().join(".codex")).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let package_roots = resolution
        .packages
        .iter()
        .map(|package| (package.clone(), package.root.clone()))
        .collect::<Vec<_>>();
    let output_plan =
        build_output_plan(temp.path(), &package_roots, Adapters::CODEX, None, false).unwrap();
    write_managed_files(&output_plan.files).unwrap();
    resolution
        .to_lockfile(Adapters::CODEX, temp.path())
        .unwrap()
        .write(&temp.path().join(LOCKFILE_NAME))
        .unwrap();

    doctor_in_dir(temp.path(), cache.path()).unwrap();
}

#[test]
fn shared_cache_is_reused_across_multiple_projects() {
    let cache = cache_dir();
    let project_one = TempDir::new().unwrap();
    let project_two = TempDir::new().unwrap();
    let (_repo, url) = create_git_dependency();

    add_dependency_all(project_one.path(), cache.path(), &url, Some("v0.1.0"));
    add_dependency_all(project_two.path(), cache.path(), &url, Some("v0.1.0"));

    let mirror_path = shared_repository_path(cache.path(), &url).unwrap();
    let rev = git_output(&mirror_path, &["rev-parse", "v0.1.0^{commit}"]);
    let checkout_path = shared_checkout_path(cache.path(), &url, &rev).unwrap();
    assert!(mirror_path.exists());
    assert!(checkout_path.exists());
    assert_eq!(
        canonicalize_git_path_output(git_output(
            &checkout_path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"]
        )),
        canonicalize_path(&mirror_path).unwrap()
    );
    let resolution_one =
        resolve_project(project_one.path(), cache.path(), ResolveMode::Sync).unwrap();
    let resolution_two =
        resolve_project(project_two.path(), cache.path(), ResolveMode::Sync).unwrap();
    let canonical_checkout_path = canonicalize_path(&checkout_path).unwrap();
    assert_eq!(
        resolution_one
            .packages
            .iter()
            .find(|package| matches!(package.source, PackageSource::Git { .. }))
            .unwrap()
            .root,
        canonical_checkout_path
    );
    assert_eq!(
        resolution_two
            .packages
            .iter()
            .find(|package| matches!(package.source, PackageSource::Git { .. }))
            .unwrap()
            .root,
        canonical_checkout_path
    );
}

#[test]
fn custom_cache_root_routes_shared_repositories_into_the_override_directory() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let (_repo, url) = create_git_dependency();

    add_dependency_all(temp.path(), cache.path(), &url, Some("v0.1.0"));

    assert!(shared_repository_path(cache.path(), &url).unwrap().exists());
}

#[test]
fn doctor_accepts_shared_mirror_backed_checkouts() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    let (_repo, url) = create_git_dependency();

    add_dependency_all(temp.path(), cache.path(), &url, Some("v0.1.0"));

    doctor_in_dir(temp.path(), cache.path()).unwrap();
}

#[test]
fn root_manifest_can_be_missing() {
    let temp = TempDir::new().unwrap();
    write_skill(&temp.path().join("skills/review"), "Review");

    let loaded = load_root_from_dir(temp.path()).unwrap();
    assert!(loaded.manifest.dependencies.is_empty());
    assert_eq!(loaded.discovered.skills[0].id, "review");
}

// ---------------------------------------------------------------------------
// Slice 3 (lockfile v9 → v10 emission) regression coverage. These tests pin
// the v10-specific behavior of `Resolution::to_lockfile_with_options`:
// per-package ownership rules, hook-prefix collapse, `install_digest` stability,
// and the assertion that `legacy_managed_files` is empty on a v10 write.
// ---------------------------------------------------------------------------

fn slice3_resolve_for_lockfile(
    project_root: &Path,
    cache_root: &Path,
    adapters: &[Adapter],
) -> (Resolution, Lockfile) {
    sync_in_dir_with_adapters(project_root, cache_root, false, false, adapters).unwrap();
    let resolution = resolve_project(project_root, cache_root, ResolveMode::Sync).unwrap();
    let lockfile = resolution
        .to_lockfile_with_options(Adapters::from_slice(adapters), project_root, false)
        .unwrap();
    (resolution, lockfile)
}

#[test]
fn to_lockfile_no_longer_populates_legacy_managed_files_on_v10_write() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    let (_resolution, lockfile) =
        slice3_resolve_for_lockfile(temp.path(), cache.path(), &[Adapter::Claude]);

    assert_eq!(lockfile.version, Lockfile::current_version());
    assert!(
        lockfile.legacy_managed_files.is_empty(),
        "v10 writes must leave `legacy_managed_files` empty; got {:?}",
        lockfile.legacy_managed_files
    );
}

#[test]
fn to_lockfile_emits_per_package_owned_subtree_for_native_plugin_packages() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    let (_resolution, lockfile) = slice3_resolve_for_lockfile(
        temp.path(),
        cache.path(),
        &[Adapter::Claude, Adapter::Cursor],
    );

    let shared = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .expect("shared package present");

    assert!(
        shared
            .owned_subtrees
            .iter()
            .all(|path| path != ".nodus/packages/shared/claude-plugin"),
        "shared.owned_subtrees should not claim the global claude plugin folder; got {:?}",
        shared.owned_subtrees
    );
    assert!(
        shared
            .owned_subtrees
            .iter()
            .any(|path| path == ".cursor/skills/review"),
        "shared.owned_subtrees should cover the direct cursor skill dir; got {:?}",
        shared.owned_subtrees
    );
}

#[test]
fn to_lockfile_collapses_multiple_hook_files_into_one_owned_prefix_rule() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_manifest(
        &temp.path().join("vendor/shared"),
        r#"
[[hooks]]
id = "session.start.hello"
event = "session_start"
adapters = ["claude"]
handler = { type = "command", command = "echo hello" }

[[hooks]]
id = "tool.pre.guard"
event = "pre_tool_use"
adapters = ["claude"]
handler = { type = "command", command = "echo guard" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    let (_resolution, lockfile) =
        slice3_resolve_for_lockfile(temp.path(), cache.path(), &[Adapter::Claude]);

    let shared = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .expect("shared package present");

    let prefix_rule = shared
        .owned_prefixes
        .iter()
        .find(|rule| rule.dir == ".claude/hooks" && rule.prefix == "nodus-hook-shared-")
        .expect(
            "expected exactly one (.claude/hooks, nodus-hook-shared-) prefix rule for non-root hooks",
        );
    let matching_rules = shared
        .owned_prefixes
        .iter()
        .filter(|rule| rule.dir == ".claude/hooks" && rule.prefix == "nodus-hook-shared-")
        .count();
    assert_eq!(
        matching_rules, 1,
        "multiple hooks for the same package+dir must collapse into one prefix rule; got {:?}",
        shared.owned_prefixes
    );
    let _ = prefix_rule;
}

#[test]
fn to_lockfile_stamps_install_digest_for_every_package() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    let (_resolution, lockfile) =
        slice3_resolve_for_lockfile(temp.path(), cache.path(), &[Adapter::Claude]);

    for package in &lockfile.packages {
        let digest = package
            .install_digest
            .as_deref()
            .unwrap_or_else(|| panic!("package `{}` is missing install_digest", package.alias));
        assert!(
            digest.starts_with("blake3:"),
            "install_digest for `{}` must be a blake3 hex string; got `{digest}`",
            package.alias
        );
    }
}

#[test]
fn to_lockfile_install_digest_changes_when_planned_bytes_change() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    let (_, first_lockfile) =
        slice3_resolve_for_lockfile(temp.path(), cache.path(), &[Adapter::Codex]);
    let first_shared_digest = first_lockfile
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap()
        .install_digest
        .clone()
        .unwrap();

    // Change the skill contents. The planned bytes change, so the install
    // digest for the `shared` package should change too.
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Updated");

    let (_, second_lockfile) =
        slice3_resolve_for_lockfile(temp.path(), cache.path(), &[Adapter::Codex]);
    let second_shared_digest = second_lockfile
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap()
        .install_digest
        .clone()
        .unwrap();

    assert_ne!(
        first_shared_digest, second_shared_digest,
        "install_digest should change when planned bytes change"
    );
}

#[test]
fn to_lockfile_install_digest_stable_across_equivalent_resolutions() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    let (_, lockfile_a) =
        slice3_resolve_for_lockfile(temp.path(), cache.path(), &[Adapter::Claude]);
    let resolution_b = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let lockfile_b = resolution_b
        .to_lockfile_with_options(Adapters::CLAUDE, temp.path(), false)
        .unwrap();

    for (package_a, package_b) in lockfile_a.packages.iter().zip(lockfile_b.packages.iter()) {
        assert_eq!(package_a.alias, package_b.alias);
        assert_eq!(
            package_a.install_digest, package_b.install_digest,
            "install_digest for `{}` must be stable across equivalent resolutions",
            package_a.alias
        );
    }
}

// =====================================================================
// Slice 4: install_digest drift fast-path
// =====================================================================
//
// The fast-path lets `nodus sync` short-circuit when the v10 lockfile
// agrees with the disk on every package's `install_digest`. These tests
// exercise the gate conditions, the disk-walk helper, and the
// `--frozen` strict-mode error surface.

/// Sync a path-dep workspace, then read the resulting v10 lockfile.
/// Used by Slice 4 inline tests that need a populated v10 lockfile to
/// exercise `install_digest_from_disk` and the fast-path evaluator.
fn slice4_sync_and_read_lockfile(project_root: &Path, cache_root: &Path) -> Lockfile {
    sync_in_dir_with_adapters(
        project_root,
        cache_root,
        false,
        false,
        &[Adapter::Claude, Adapter::Codex, Adapter::OpenCode],
    )
    .unwrap();
    Lockfile::read(&project_root.join(LOCKFILE_NAME)).unwrap()
}

/// Build a path-dep workspace with one shared skill — the smallest
/// fixture that produces non-trivial per-package owned outputs.
fn slice4_make_workspace() -> TempDir {
    let temp = TempDir::new().unwrap();
    write_manifest(
        temp.path(),
        r#"
[adapters]
enabled = ["claude", "codex", "opencode"]

[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    temp
}

#[test]
fn slice4_install_digest_from_disk_matches_recorded_digest_for_unchanged_install() {
    let temp = slice4_make_workspace();
    let cache = cache_dir();
    let lockfile = slice4_sync_and_read_lockfile(temp.path(), cache.path());

    for package in &lockfile.packages {
        let recorded = package
            .install_digest
            .clone()
            .expect("v10 emission always stamps install_digest");
        let disk = super::install_digest::install_digest_from_disk(temp.path(), &lockfile, package)
            .unwrap()
            .expect("clean install means no owned files are missing");
        assert_eq!(
            disk, recorded,
            "package `{}` disk digest must match recorded digest after a clean sync",
            package.alias
        );
    }
}

/// Regression: `install_digest` must be hashed from the bytes actually written
/// to disk (the merged output plan), not the merge-free ownership plan. When a
/// consumer has pre-existing MCP entries, the rendered `.mcp.json` merges them
/// in, so the written file differs from the merge-free plan. Hashing the latter
/// made `nodus sync --frozen` report perpetual "disk drift" on every
/// merge-target config file (`.mcp.json`, `.codex/config.toml`,
/// `.claude/settings.json`, `opencode.json`).
#[test]
fn slice4_install_digest_matches_disk_for_merged_mcp_config() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies.firebase]
path = "vendor/firebase"
"#,
    );
    write_file(
        &temp.path().join("vendor/firebase/nodus.toml"),
        r#"
[mcp_servers.firebase]
command = "npx"
"#,
    );
    // Pre-existing user MCP entry, present on disk before sync. The rendered
    // `.mcp.json` merges it with the managed `firebase__firebase` server, so the
    // written bytes diverge from the merge-free ownership plan.
    write_file(
        &temp.path().join(".mcp.json"),
        r#"{
  "mcpServers": {
    "local": {
      "command": "node"
    }
  }
}
"#,
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();

    // The merge actually happened: the user entry survives on disk alongside the
    // managed one. Without divergence this test could not catch the bug.
    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(
        json["mcpServers"]["local"]["command"].as_str(),
        Some("node")
    );
    assert_eq!(
        json["mcpServers"]["firebase__firebase"]["command"].as_str(),
        Some("npx")
    );

    for package in &lockfile.packages {
        let recorded = package
            .install_digest
            .clone()
            .expect("v10 emission always stamps install_digest");
        let disk = super::install_digest::install_digest_from_disk(temp.path(), &lockfile, package)
            .unwrap()
            .expect("clean install means no owned files are missing");
        assert_eq!(
            disk, recorded,
            "package `{}` disk digest must match recorded digest after a clean sync \
             that merged pre-existing MCP config",
            package.alias
        );
    }
}

#[test]
fn slice4_install_digest_from_disk_returns_none_when_owned_file_is_missing() {
    let temp = slice4_make_workspace();
    let cache = cache_dir();
    let lockfile = slice4_sync_and_read_lockfile(temp.path(), cache.path());

    // Find any package with at least one owned_files entry and remove
    // that file from disk to simulate user deletion.
    let (target_package, target_file) = lockfile
        .packages
        .iter()
        .find_map(|package| package.owned_files.first().map(|file| (package, file)))
        .expect("test workspace should produce at least one owned_files entry");
    fs::remove_file(temp.path().join(target_file)).unwrap();

    let result =
        super::install_digest::install_digest_from_disk(temp.path(), &lockfile, target_package)
            .unwrap();
    assert!(
        result.is_none(),
        "removing an owned file should yield Ok(None) drift signal; got {result:?}"
    );
}

#[test]
fn slice4_install_digest_from_disk_differs_when_owned_file_content_changes() {
    let temp = slice4_make_workspace();
    let cache = cache_dir();
    let lockfile = slice4_sync_and_read_lockfile(temp.path(), cache.path());

    let (target_package, target_file) = lockfile
        .packages
        .iter()
        .find_map(|package| package.owned_files.first().map(|file| (package, file)))
        .expect("test workspace should produce at least one owned_files entry");
    let absolute = temp.path().join(target_file);
    fs::write(&absolute, b"--- user override ---").unwrap();

    let mutated =
        super::install_digest::install_digest_from_disk(temp.path(), &lockfile, target_package)
            .unwrap()
            .expect("file still exists, just changed");
    let recorded = target_package.install_digest.as_deref().unwrap();
    assert_ne!(
        mutated, recorded,
        "mutating an owned file's bytes must change the disk digest"
    );
}

#[test]
fn slice4_fast_path_skipped_for_path_dependencies() {
    // Path deps disable the fast-path because local source content can
    // change at any time without the lockfile noticing. A second sync of
    // the same path-dep workspace must still issue a `Resolving` status
    // line, proving the loop ran.
    let temp = slice4_make_workspace();
    let cache = cache_dir();
    sync_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        false,
        false,
        &[Adapter::Claude, Adapter::Codex, Adapter::OpenCode],
    )
    .unwrap();

    let buffer = SharedBuffer::default();
    let reporter = Reporter::sink(ColorMode::Never, buffer.clone());
    super::sync_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        false,
        false,
        false,
        &[Adapter::Claude, Adapter::Codex, Adapter::OpenCode],
        false,
        &reporter,
    )
    .unwrap();

    let output = buffer.contents();
    assert!(
        output.contains("Resolving"),
        "second sync with path deps must run the full loop; got: {output}"
    );
    assert!(
        !output.contains("is in sync; no work to do"),
        "fast-path note must not appear for a path-dep workspace; got: {output}"
    );
}

#[test]
fn slice4_fast_path_skipped_for_global_plugin_payload_adapters() {
    let package = LockedPackage {
        alias: "shared".into(),
        name: "shared".into(),
        version_tag: Some("v0.1.0".into()),
        source: LockedSource {
            kind: "git".into(),
            path: None,
            url: Some("https://example.invalid/shared".into()),
            tag: Some("v0.1.0".into()),
            branch: None,
            rev: Some("abc123".into()),
        },
        digest: "blake3:abc".into(),
        selected_components: None,
        skills: vec!["review".into()],
        agents: vec![],
        rules: vec![],
        commands: vec![],
        mcp_servers: vec![],
        dependencies: vec![],
        capabilities: vec![],
        owned_subtrees: vec![],
        owned_prefixes: vec![],
        owned_runtime_adapters: vec![],
        owned_files: vec![],
        install_digest: Some(crate::hashing::content_digest(&[])),
    };
    let lockfile = Lockfile::new(vec![package]);
    let temp = TempDir::new().unwrap();

    let outcome = super::evaluate_fast_path(
        &lockfile,
        temp.path(),
        super::SyncMode::Normal,
        temp.path(),
        Adapters::CLAUDE,
    )
    .unwrap();
    match outcome {
        super::FastPathOutcome::Miss(reason) => {
            assert!(
                reason.contains("global package payloads"),
                "expected miss reason to mention global payloads; got: {reason}"
            );
        }
        super::FastPathOutcome::Hit => panic!("expected Miss for global plugin payload adapter"),
    }
}

#[test]
fn slice4_fast_path_skipped_when_install_digest_is_none() {
    // Construct a synthetic v10 lockfile where one package's
    // install_digest is None (a hand-edit could produce this).
    // `evaluate_fast_path` must report a Miss describing the gap.
    let mut package = LockedPackage {
        alias: "foo".into(),
        name: "foo".into(),
        version_tag: None,
        source: LockedSource {
            kind: "git".into(),
            path: None,
            url: Some("https://example.invalid/foo".into()),
            tag: Some("v0.1.0".into()),
            branch: None,
            rev: Some("abc123".into()),
        },
        digest: "blake3:abc".into(),
        selected_components: None,
        skills: vec![],
        agents: vec![],
        rules: vec![],
        commands: vec![],
        mcp_servers: vec![],
        dependencies: vec![],
        capabilities: vec![],
        owned_subtrees: vec![],
        owned_prefixes: vec![],
        owned_runtime_adapters: vec![],
        owned_files: vec![],
        install_digest: None,
    };
    package.install_digest = None;
    let lockfile = Lockfile::new(vec![package]);

    let temp = TempDir::new().unwrap();
    let outcome = super::evaluate_fast_path(
        &lockfile,
        temp.path(),
        super::SyncMode::Normal,
        temp.path(),
        Adapters::NONE,
    )
    .unwrap();
    match outcome {
        super::FastPathOutcome::Miss(reason) => {
            assert!(
                reason.contains("install_digest"),
                "expected miss reason to mention install_digest; got: {reason}"
            );
        }
        super::FastPathOutcome::Hit => panic!("expected Miss, got Hit"),
    }
}

#[test]
fn slice4_fast_path_skipped_for_branch_pinned_deps_in_normal_mode() {
    // A v10 lockfile with a git source that records a `branch` cannot
    // safely fast-path under normal sync — upstream may have moved.
    let package = LockedPackage {
        alias: "foo".into(),
        name: "foo".into(),
        version_tag: None,
        source: LockedSource {
            kind: "git".into(),
            path: None,
            url: Some("https://example.invalid/foo".into()),
            tag: None,
            branch: Some("main".into()),
            rev: Some("abc123".into()),
        },
        digest: "blake3:abc".into(),
        selected_components: None,
        skills: vec![],
        agents: vec![],
        rules: vec![],
        commands: vec![],
        mcp_servers: vec![],
        dependencies: vec![],
        capabilities: vec![],
        owned_subtrees: vec![],
        owned_prefixes: vec![],
        owned_runtime_adapters: vec![],
        owned_files: vec![],
        install_digest: Some(crate::hashing::content_digest(&[])),
    };
    let lockfile = Lockfile::new(vec![package]);

    let temp = TempDir::new().unwrap();
    let outcome = super::evaluate_fast_path(
        &lockfile,
        temp.path(),
        super::SyncMode::Normal,
        temp.path(),
        Adapters::NONE,
    )
    .unwrap();
    match outcome {
        super::FastPathOutcome::Miss(reason) => {
            assert!(
                reason.contains("tracks branch"),
                "expected miss reason to mention branch tracking; got: {reason}"
            );
        }
        super::FastPathOutcome::Hit => panic!("expected Miss for branch-tracked dep"),
    }
}

#[test]
fn slice4_fast_path_allows_branch_pinned_deps_under_frozen() {
    // Same lockfile shape as the previous test, but evaluated under
    // `--frozen`. Frozen bypasses the freshness gate, so the empty
    // package digest matches the (empty) on-disk state and the
    // evaluator should report a hit.
    let package = LockedPackage {
        alias: "foo".into(),
        name: "foo".into(),
        version_tag: None,
        source: LockedSource {
            kind: "git".into(),
            path: None,
            url: Some("https://example.invalid/foo".into()),
            tag: None,
            branch: Some("main".into()),
            rev: Some("abc123".into()),
        },
        digest: "blake3:abc".into(),
        selected_components: None,
        skills: vec![],
        agents: vec![],
        rules: vec![],
        commands: vec![],
        mcp_servers: vec![],
        dependencies: vec![],
        capabilities: vec![],
        owned_subtrees: vec![],
        owned_prefixes: vec![],
        owned_runtime_adapters: vec![],
        owned_files: vec![],
        install_digest: Some(crate::hashing::content_digest(&[])),
    };
    let lockfile = Lockfile::new(vec![package]);

    let temp = TempDir::new().unwrap();
    // Cache-presence gate is the last gate to fire; seed the snapshot dir
    // so it doesn't masquerade as a freshness/integrity miss here.
    let snapshot_path = crate::store::snapshot_path(temp.path(), "blake3:abc").unwrap();
    fs::create_dir_all(&snapshot_path).unwrap();
    let outcome = super::evaluate_fast_path(
        &lockfile,
        temp.path(),
        super::SyncMode::Frozen,
        temp.path(),
        Adapters::NONE,
    )
    .unwrap();
    assert!(
        matches!(outcome, super::FastPathOutcome::Hit),
        "frozen mode must bypass the freshness gate and accept branch-tracked deps"
    );
}

#[test]
fn slice4_fast_path_miss_on_drift_under_frozen_reports_lockfile_out_of_date() {
    // Build a path-dep workspace, do a clean sync, then delete an owned
    // file and re-run with `--frozen`. The frozen invocation must bail
    // before the resolve loop and the error string must include the
    // "out of date" phrase so existing diagnostics still apply. Path
    // deps normally skip the fast-path, but we set `legacy_managed_files`
    // to nothing and check the actual error surfaces from the fast-path
    // by removing an owned file.
    //
    // For this test we synthesize a v10 lockfile manually so we can
    // exercise the frozen-fail path even though the workspace is path-
    // backed. The lockfile claims to own a file that doesn't exist.
    let temp = TempDir::new().unwrap();
    write_manifest(temp.path(), "");
    let lockfile = Lockfile::new(vec![LockedPackage {
        alias: "ghost".into(),
        name: "ghost".into(),
        version_tag: None,
        source: LockedSource {
            kind: "git".into(),
            path: None,
            url: Some("https://example.invalid/ghost".into()),
            tag: Some("v0.1.0".into()),
            branch: None,
            rev: Some("abc123".into()),
        },
        digest: "blake3:abc".into(),
        selected_components: None,
        skills: vec![],
        agents: vec![],
        rules: vec![],
        commands: vec![],
        mcp_servers: vec![],
        dependencies: vec![],
        capabilities: vec![],
        owned_subtrees: vec![],
        owned_prefixes: vec![],
        owned_runtime_adapters: vec![],
        owned_files: vec!["ghost.md".into()],
        // Pretend we recorded a digest for a file that doesn't exist on
        // disk.
        install_digest: Some("blake3:deadbeef".into()),
    }]);
    lockfile.write(&temp.path().join(LOCKFILE_NAME)).unwrap();

    let outcome = super::evaluate_fast_path(
        &Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap(),
        temp.path(),
        super::SyncMode::Frozen,
        temp.path(),
        Adapters::NONE,
    )
    .unwrap();
    match outcome {
        super::FastPathOutcome::Miss(reason) => {
            assert!(
                reason.contains("ghost"),
                "miss reason should name the failing package; got: {reason}"
            );
        }
        super::FastPathOutcome::Hit => panic!("expected Miss for missing owned file"),
    }
}

#[test]
fn slice4_count_owned_files_sums_per_package_views() {
    let mut alpha = LockedPackage {
        alias: "alpha".into(),
        name: "alpha".into(),
        version_tag: None,
        source: LockedSource {
            kind: "path".into(),
            path: Some(".".into()),
            url: None,
            tag: None,
            branch: None,
            rev: None,
        },
        digest: "blake3:abc".into(),
        selected_components: None,
        skills: vec![],
        agents: vec![],
        rules: vec![],
        commands: vec![],
        mcp_servers: vec![],
        dependencies: vec![],
        capabilities: vec![],
        owned_subtrees: vec![".nodus/packages/alpha".into()],
        owned_prefixes: vec![],
        owned_runtime_adapters: vec![],
        owned_files: vec![".claude/settings.json".into()],
        install_digest: Some(crate::hashing::content_digest(&[])),
    };
    let mut beta = alpha.clone();
    beta.alias = "beta".into();
    beta.name = "beta".into();
    beta.owned_subtrees = vec![];
    beta.owned_prefixes = vec![crate::lockfile::OwnedPrefix {
        dir: ".claude/hooks".into(),
        prefix: "nodus-hook-".into(),
    }];
    beta.owned_files = vec![];
    alpha.owned_subtrees.sort();
    beta.owned_prefixes.sort_by(|left, right| {
        left.dir
            .cmp(&right.dir)
            .then(left.prefix.cmp(&right.prefix))
    });
    let lockfile = Lockfile::new(vec![alpha, beta]);

    // alpha: 1 subtree + 1 file = 2; beta: 1 prefix = 1; total = 3
    assert_eq!(super::count_owned_files(&lockfile), 3);
}

/// Regression guard for the Slice 5 review HIGH finding: with overlapping
/// ownership claims across packages, `attribute_file_to_package` must
/// resolve in a stable order so `install_digest` distribution doesn't
/// silently shift across runs. The fix routes the inner map through
/// `BTreeMap` to get alphabetical iteration; this test fails if it
/// regresses to `HashMap`.
#[test]
fn slice5_attribute_file_to_package_is_alphabetically_deterministic() {
    use std::collections::BTreeMap;
    use std::path::Path;

    use crate::adapters::PackageOwnedPaths;
    use crate::lockfile::OwnedPrefix;

    // Two packages with deliberately overlapping subtrees. `aaa` claims `.x`
    // wholesale; `zzz` claims the nested `.x/y`. A naive iteration order
    // (HashMap) could attribute `.x/y/foo` to either; the BTreeMap fix pins
    // it to `aaa` (alphabetically first).
    let mut map: BTreeMap<String, PackageOwnedPaths> = BTreeMap::new();
    map.insert(
        "aaa".into(),
        PackageOwnedPaths {
            alias: "aaa".into(),
            subtrees: vec![".x".into()],
            prefixes: vec![],
            files: vec![],
        },
    );
    map.insert(
        "zzz".into(),
        PackageOwnedPaths {
            alias: "zzz".into(),
            subtrees: vec![".x/y".into()],
            prefixes: vec![],
            files: vec![],
        },
    );

    // Call attribute_file_to_package many times; under HashMap iteration
    // order this would flip between "aaa" and "zzz" run-to-run. Under
    // BTreeMap it is always "aaa".
    let path = Path::new(".x/y/foo.txt");
    for _ in 0..256 {
        assert_eq!(
            super::attribute_file_to_package(path, &map),
            Some("aaa".to_string()),
            "alphabetically earlier alias must win when ownership overlaps"
        );
    }

    // Same property for prefix-vs-subtree overlap. `aaa` owns subtree
    // `.claude/hooks`; `zzz` declares a prefix rule on the same dir.
    // Subtree wins (per the priority order), but the loser pool itself
    // must iterate deterministically — assert by inserting an exact-file
    // overlap and watching attribution stay stable across iterations.
    let mut map2: BTreeMap<String, PackageOwnedPaths> = BTreeMap::new();
    map2.insert(
        "bbb".into(),
        PackageOwnedPaths {
            alias: "bbb".into(),
            subtrees: vec![],
            prefixes: vec![OwnedPrefix {
                dir: ".claude/hooks".into(),
                prefix: "shared-".into(),
            }],
            files: vec![],
        },
    );
    map2.insert(
        "aaa".into(),
        PackageOwnedPaths {
            alias: "aaa".into(),
            subtrees: vec![],
            prefixes: vec![OwnedPrefix {
                dir: ".claude/hooks".into(),
                prefix: "shared-".into(),
            }],
            files: vec![],
        },
    );
    let path2 = Path::new(".claude/hooks/shared-thing.sh");
    for _ in 0..256 {
        assert_eq!(
            super::attribute_file_to_package(path2, &map2),
            Some("aaa".to_string()),
            "prefix-rule overlap must resolve alphabetically too"
        );
    }
}

/// Workspace marketplace JSON now lives in the shared global Nodus home, so it
/// must not be attributed to the project lockfile's root package. The project
/// install digest should still match the project-owned files on disk.
#[test]
fn slice5_workspace_mode_install_digest_ignores_global_marketplace_files() {
    let repo = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        repo.path(),
        r#"
name = "Workspace Plugins"

[workspace]
members = ["plugins/axiom"]

[workspace.package.axiom]
path = "plugins/axiom"
name = "Axiom"

[workspace.package.axiom.codex]
category = "Productivity"
installation = "AVAILABLE"
authentication = "ON_INSTALL"
"#,
    );
    write_skill(&repo.path().join("plugins/axiom/skills/review"), "Review");

    sync_in_dir_with_adapters(repo.path(), cache.path(), false, false, &Adapter::ALL).unwrap();

    let claude_marketplace = generated_claude_marketplace_path(repo.path());
    assert!(
        claude_marketplace.exists(),
        "test fixture didn't write the claude workspace marketplace JSON; \
         the test is no longer exercising the bug it's guarding against"
    );

    let lockfile = Lockfile::read(&repo.path().join(LOCKFILE_NAME)).unwrap();
    let root_package = lockfile
        .packages
        .iter()
        .find(|pkg| pkg.alias == "root")
        .expect("root package must appear in v10 lockfile");

    assert!(
        root_package
            .owned_files
            .iter()
            .all(|f| !f.contains("marketplace.json")),
        "expected root package owned_files to omit global marketplace JSON; \
         got: {:?}",
        root_package.owned_files
    );

    let recorded = root_package
        .install_digest
        .as_deref()
        .expect("v10 lockfile must stamp install_digest on every package");
    let from_disk =
        super::install_digest::install_digest_from_disk(repo.path(), &lockfile, root_package)
            .unwrap()
            .expect("root package owns existing files on disk");

    assert_eq!(
        from_disk, recorded,
        "recorded install_digest must include workspace marketplace file bytes; \
         otherwise the drift fast-path misses on every sync in workspace mode"
    );
}
