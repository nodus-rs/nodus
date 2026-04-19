use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

use super::*;
use crate::adapters::{Adapter, Adapters, ArtifactKind, ManagedArtifactNames};
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

fn normalize_workspace_marketplace_name(value: &str) -> String {
    let mut normalized = String::new();

    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            normalized.push(character.to_ascii_lowercase());
        } else if !normalized.ends_with('-') {
            normalized.push('-');
        }
    }

    let normalized = normalized.trim_matches('-').to_string();
    if normalized.is_empty() {
        String::from("agentpack")
    } else {
        normalized
    }
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
    super::resolve_project(root, cache_root, mode, &reporter, None, None)
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
    assert!(
        !lockfile
            .managed_files
            .contains(&".claude/skills/review".into())
    );
    assert!(
        lockfile
            .managed_files
            .contains(&".claude/skills/checks".into())
    );
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

    assert!(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
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
    assert!(!lockfile.managed_files.is_empty());
    let dependency_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias != "root")
        .unwrap();
    assert_eq!(dependency_package.version_tag.as_deref(), Some("v0.1.0"));

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias != "root")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    assert!(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
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
fn sync_generates_workspace_marketplace_files() {
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
        &fs::read_to_string(repo.path().join(".claude-plugin/marketplace.json")).unwrap(),
    )
    .unwrap();
    let expected_marketplace_name = normalize_workspace_marketplace_name(&expected_owner_name);
    assert_eq!(
        claude["name"].as_str(),
        Some(expected_marketplace_name.as_str())
    );
    assert_eq!(
        claude["owner"]["name"].as_str(),
        Some(expected_owner_name.as_str())
    );
    assert_eq!(claude["plugins"].as_array().unwrap().len(), 2);
    assert_eq!(
        claude["plugins"][0]["source"].as_str(),
        Some("plugins/axiom")
    );

    let codex: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo.path().join(".agents/plugins/marketplace.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        codex["name"].as_str(),
        Some(expected_marketplace_name.as_str())
    );
    assert_eq!(codex["plugins"].as_array().unwrap().len(), 2);
    assert_eq!(
        codex["plugins"][0]["source"]["path"].as_str(),
        Some("./plugins/axiom")
    );
    assert_eq!(
        codex["plugins"][0]["policy"]["installation"].as_str(),
        Some("AVAILABLE")
    );

    let lockfile = Lockfile::read(&repo.path().join(LOCKFILE_NAME)).unwrap();
    assert!(
        lockfile
            .managed_files
            .contains(&String::from(".claude-plugin/marketplace.json"))
    );
    assert!(
        lockfile
            .managed_files
            .contains(&String::from(".agents/plugins/marketplace.json"))
    );
}

#[test]
fn sync_skips_invalid_workspace_members_in_marketplace_files() {
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
        &fs::read_to_string(repo.path().join(".claude-plugin/marketplace.json")).unwrap(),
    )
    .unwrap();
    let expected_marketplace_name = normalize_workspace_marketplace_name(&expected_owner_name);
    assert_eq!(
        claude["name"].as_str(),
        Some(expected_marketplace_name.as_str())
    );
    assert_eq!(
        claude["owner"]["name"].as_str(),
        Some(expected_owner_name.as_str())
    );
    assert_eq!(claude["plugins"].as_array().unwrap().len(), 1);
    assert_eq!(
        claude["plugins"][0]["source"].as_str(),
        Some("plugins/axiom")
    );

    let codex: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo.path().join(".agents/plugins/marketplace.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        codex["name"].as_str(),
        Some(expected_marketplace_name.as_str())
    );
    assert_eq!(codex["plugins"].as_array().unwrap().len(), 1);
    assert_eq!(codex["plugins"][0]["name"].as_str(), Some("Axiom"));
    assert_eq!(
        codex["plugins"][0]["source"]["path"].as_str(),
        Some("./plugins/axiom")
    );
}

#[test]
fn sync_emits_codex_marketplace_for_only_workspace_members_with_codex_metadata() {
    let repo = TempDir::new().unwrap();
    let cache = cache_dir();
    write_workspace_dependency_with_non_codex_member(repo.path());
    let expected_owner_name = repo
        .path()
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap()
        .to_string();

    sync_in_dir_with_adapters(repo.path(), cache.path(), false, false, &Adapter::ALL).unwrap();

    let claude: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo.path().join(".claude-plugin/marketplace.json")).unwrap(),
    )
    .unwrap();
    let expected_marketplace_name = normalize_workspace_marketplace_name(&expected_owner_name);
    assert_eq!(
        claude["name"].as_str(),
        Some(expected_marketplace_name.as_str())
    );
    assert_eq!(claude["plugins"].as_array().unwrap().len(), 2);

    let codex: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo.path().join(".agents/plugins/marketplace.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        codex["name"].as_str(),
        Some(expected_marketplace_name.as_str())
    );
    assert_eq!(codex["plugins"].as_array().unwrap().len(), 1);
    assert_eq!(codex["plugins"][0]["name"].as_str(), Some("Axiom"));
    assert_eq!(
        codex["plugins"][0]["source"]["path"].as_str(),
        Some("./plugins/axiom")
    );
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
        &fs::read_to_string(repo.path().join(".claude-plugin/marketplace.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(claude["name"].as_str(), Some("workspace-plugins"));
    assert_eq!(claude["owner"]["name"].as_str(), Some("Workspace Plugins"));
    assert_eq!(claude["plugins"].as_array().unwrap().len(), 1);
    assert_eq!(claude["plugins"][0]["name"].as_str(), Some("Axiom"));
    assert_eq!(
        claude["plugins"][0]["source"].as_str(),
        Some("plugins/axiom")
    );
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

    assert!(
        temp.path()
            .join(format!(".claude/skills/{molt_fetch_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/skills/{audit_logging_skill_id}/SKILL.md"))
            .exists()
    );
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
    assert!(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
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
    assert!(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/agents/{managed_agent_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/commands/{managed_command_file}"))
            .exists()
    );
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

    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(
        json["mcpServers"][format!("{wrapper_alias}__atlan")]["url"].as_str(),
        Some("https://mcp.atlan.com/mcp")
    );
    assert_eq!(
        json["mcpServers"][format!("{wrapper_alias}__atlan")]["type"].as_str(),
        Some("http")
    );
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
    assert!(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/agents/{managed_agent_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/commands/{managed_command_file}"))
            .exists()
    );
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
    assert!(wrapper_script.contains(".nodus/packages/hook_plugin/claude-plugin"));
    assert!(wrapper_script.contains("${CLAUDE_PLUGIN_ROOT}/scripts/format-code.sh"));

    assert!(
        temp.path()
            .join(".nodus/packages/hook_plugin/claude-plugin/hooks/hooks.json")
            .exists()
    );
    assert!(
        temp.path()
            .join(".nodus/packages/hook_plugin/claude-plugin/scripts/format-code.sh")
            .exists()
    );
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
    assert!(
        temp.path()
            .join(format!(".claude/skills/{root_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/skills/{subdir_skill_id}/SKILL.md"))
            .exists()
    );
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
    assert!(
        temp.path()
            .join(format!(".codex/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );

    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(
        json["mcpServers"]["axiom__figma"]["url"].as_str(),
        Some("http://127.0.0.1:3845/mcp")
    );
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
    assert!(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
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
    assert!(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );

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

    assert!(
        home.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        home.path()
            .join(format!(".codex/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
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
    let managed_skill = home
        .path()
        .join(format!(".codex/skills/{managed_skill_id}/SKILL.md"));
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
    let managed_command_file = namespaced_file_name(dependency, "build", "md");
    let managed_claude_rule_file = namespaced_file_name(dependency, "default", "md");
    let managed_cursor_rule_file = namespaced_file_name(dependency, "default", "mdc");

    assert!(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/agents/{managed_agent_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/commands/{managed_command_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/rules/{managed_claude_rule_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".codex/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
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
    assert!(!temp.path().join(".claude/agents/security.md").exists());
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
    let managed_command_file = namespaced_file_name(dependency, "build", "md");
    let managed_claude_rule_file = namespaced_file_name(dependency, "default", "md");

    assert_eq!(
        dependency.selected_components,
        Some(vec![DependencyComponent::Skills])
    );
    assert!(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".opencode/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(".claude/agents/{managed_agent_file}"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(".opencode/agents/{managed_agent_file}"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(".claude/commands/{managed_command_file}"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(".opencode/commands/{managed_command_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".codex/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(".claude/rules/{managed_claude_rule_file}"))
            .exists()
    );
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
    assert!(
        lockfile
            .managed_files
            .contains(&".claude/skills/review".into())
    );
    assert!(
        !lockfile
            .managed_files
            .contains(&".claude/agents/shared.md".into())
    );
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

    assert!(
        !temp
            .path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(".codex/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        !lockfile
            .managed_files
            .contains(&".claude/skills/review".into())
    );
    assert!(
        !lockfile
            .managed_files
            .contains(&".codex/skills/review".into())
    );
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

    assert!(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
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
            .join(format!(".claude/rules/{managed_claude_rule_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".codex/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        lockfile
            .managed_files
            .contains(&".claude/skills/review".into())
    );
    assert!(
        lockfile
            .managed_files
            .contains(&".codex/skills/review".into())
    );
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
    let codex_gitignore = fs::read_to_string(temp.path().join(".codex/.gitignore")).unwrap();
    let agents_gitignore = fs::read_to_string(temp.path().join(".agents/.gitignore")).unwrap();
    let cursor_gitignore = fs::read_to_string(temp.path().join(".cursor/.gitignore")).unwrap();

    assert!(codex_gitignore.contains("# Managed by nodus"));
    assert!(codex_gitignore.contains(".gitignore"));
    assert!(codex_gitignore.contains(&format!("skills/{managed_skill_id}")));
    assert!(agents_gitignore.contains("# Managed by nodus"));
    assert!(agents_gitignore.contains(".gitignore"));
    assert!(agents_gitignore.contains(&format!("skills/{managed_skill_id}")));
    assert!(agents_gitignore.contains(&format!("commands/{managed_command_file}")));
    assert!(cursor_gitignore.contains("# Managed by nodus"));
    assert!(cursor_gitignore.contains(".gitignore"));
    assert!(cursor_gitignore.contains(&format!("skills/{managed_skill_id}")));
    assert!(cursor_gitignore.contains(&format!("commands/{managed_command_file}")));
    assert!(cursor_gitignore.contains("rules/default.mdc"));
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
    write_file(
        &temp.path().join(".codex/skills"),
        "user-owned blocking file\n",
    );

    let error =
        sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex])
            .unwrap_err()
            .to_string();
    assert!(error.contains("refusing to overwrite unmanaged file"));
    assert!(error.contains(".codex/skills"));

    sync_in_dir_with_adapters_force(temp.path(), cache.path(), false, false, &[Adapter::Codex])
        .unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    let skill = fs::read_to_string(
        temp.path()
            .join(format!(".codex/skills/{managed_skill_id}/SKILL.md")),
    )
    .unwrap();
    assert!(skill.contains("# Review"));
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
    write_file(
        &temp.path().join(".codex/skills"),
        "user-owned blocking file\n",
    );

    sync_in_dir_with_adapters_dry_run_force(
        temp.path(),
        cache.path(),
        false,
        false,
        &[Adapter::Codex],
    )
    .unwrap();

    assert_eq!(
        fs::read_to_string(temp.path().join(".codex/skills")).unwrap(),
        "user-owned blocking file\n"
    );
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    assert!(
        !lockfile
            .managed_files
            .iter()
            .any(|path| path.starts_with(".codex/skills/"))
    );
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
target = ".claude/.gitignore"
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    write_file(
        &temp.path().join("vendor/shared/config/.gitignore"),
        ".DS_Store\n",
    );

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Claude]).unwrap();
    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Claude]).unwrap();

    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    let gitignore = fs::read_to_string(temp.path().join(".claude/.gitignore")).unwrap();
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();

    assert!(gitignore.contains("# Managed by nodus"));
    assert!(gitignore.contains(".gitignore"));
    assert!(gitignore.contains(".DS_Store"));
    assert!(gitignore.contains(&format!("skills/{managed_skill_id}")));
    assert!(
        lockfile
            .managed_files
            .contains(&".claude/.gitignore".into())
    );
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
    assert!(lockfile.managed_files.contains(&String::from(".mcp.json")));
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
fn sync_emits_codex_config_toml_from_dependency_manifests() {
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

    let config: toml::Value =
        toml::from_str(&fs::read_to_string(temp.path().join(".codex/config.toml")).unwrap())
            .unwrap();
    let lockfile = Lockfile::read(&temp.path().join(LOCKFILE_NAME)).unwrap();
    let firebase_package = lockfile
        .packages
        .iter()
        .find(|package| package.alias == "firebase")
        .unwrap();

    assert_eq!(firebase_package.mcp_servers, vec!["figma", "firebase"]);
    assert!(
        lockfile
            .managed_files
            .contains(&String::from(".codex/config.toml"))
    );
    assert_eq!(
        config["mcp_servers"]["firebase__firebase"]["command"].as_str(),
        Some("npx")
    );
    assert_eq!(
        config["mcp_servers"]["firebase__firebase"]["args"].as_array(),
        Some(&vec![
            toml::Value::String("-y".into()),
            toml::Value::String("firebase-tools".into()),
            toml::Value::String("mcp".into()),
            toml::Value::String("--dir".into()),
            toml::Value::String(".".into()),
        ])
    );
    assert_eq!(
        config["mcp_servers"]["firebase__firebase"]["cwd"].as_str(),
        Some(".")
    );
    assert_eq!(
        config["mcp_servers"]["firebase__firebase"]["env"]["IS_FIREBASE_MCP"].as_str(),
        Some("true")
    );
    assert_eq!(
        config["mcp_servers"]["firebase__figma"]["url"].as_str(),
        Some("https://mcp.figma.com/mcp")
    );
    assert_eq!(
        config["mcp_servers"]["firebase__figma"]["bearer_token_env_var"].as_str(),
        Some("FIGMA_TOKEN")
    );
    assert_eq!(
        config["mcp_servers"]["firebase__figma"]["http_headers"]["X-Figma-Region"].as_str(),
        Some("us-east-1")
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
    assert!(
        lockfile
            .managed_files
            .contains(&String::from("opencode.json"))
    );
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
    assert!(lockfile.managed_files.contains(&String::from(".mcp.json")));
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
    assert!(
        json["mcpServers"].get("nodus").is_some(),
        "nodus server should be auto-registered"
    );
    assert!(lockfile.managed_files.contains(&String::from(".mcp.json")));
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

    fs::rename(
        temp.path()
            .join(format!(".claude/agents/{managed_agent_file}")),
        temp.path()
            .join(format!(".claude/agents/{legacy_agent_file}")),
    )
    .unwrap();
    fs::rename(
        temp.path()
            .join(format!(".claude/commands/{managed_command_file}")),
        temp.path()
            .join(format!(".claude/commands/{legacy_command_file}")),
    )
    .unwrap();
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
    Lockfile {
        version: 8,
        packages: current_lockfile.packages,
        managed_files: current_lockfile.managed_files,
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
    assert!(
        temp.path()
            .join(format!(".claude/agents/{managed_agent_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/commands/{managed_command_file}"))
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
            .join(format!(".opencode/skills/{managed_skill_id}"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(".claude/agents/{legacy_agent_file}"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(".claude/commands/{legacy_command_file}"))
            .exists()
    );
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

    assert!(claude_settings.contains("\"SessionStart\""));
    assert!(claude_settings.contains("\"startup|resume\""));
    assert_eq!(
        codex_config["features"]["codex_hooks"].as_bool(),
        Some(true)
    );
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
            .contains("/.codex/hooks/nodus-hook-")
    );
    assert!(opencode_plugin.contains(".opencode/scripts/nodus-hook-"));
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
                            && hook["command"].as_str().is_some_and(|command| {
                                command.contains("/.codex/hooks/nodus-hook-")
                            })
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
            .contains("/.codex/hooks/nodus-hook-")
    );
    assert!(opencode_plugin.contains("\"tool.execute.before\""));
    assert!(opencode_plugin.contains(".opencode/scripts/nodus-hook-"));
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
    assert!(!temp.path().join(".codex/skills").exists());
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
    let initial_skill_path = temp
        .path()
        .join(format!(".claude/skills/{initial_skill_id}/SKILL.md"));
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
    let updated_skill_path = temp
        .path()
        .join(format!(".claude/skills/{updated_skill_id}/SKILL.md"));
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
    assert!(temp.path().join(".claude/skills").exists());
    assert!(temp.path().join(".opencode/skills").exists());

    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Claude]).unwrap();

    let manifest = load_root_from_dir(temp.path()).unwrap();
    assert_eq!(
        manifest.manifest.enabled_adapters().unwrap(),
        [Adapter::Claude].as_slice()
    );
    assert!(temp.path().join(".claude/skills").exists());
    assert!(temp.path().join(".claude/.gitignore").exists());
    assert!(!temp.path().join(".codex/skills").exists());
    assert!(!temp.path().join(".codex/.gitignore").exists());
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
    assert!(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/agents/{managed_agent_file}"))
            .exists()
    );

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

    assert!(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(".claude/agents/{managed_agent_file}"))
            .exists()
    );
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

    assert!(
        lockfile
            .managed_files
            .contains(&".claude/skills/iframe-ad".into())
    );
    assert!(
        lockfile
            .managed_files
            .contains(&".github/skills/iframe-ad".into())
    );
    assert!(
        lockfile
            .managed_files
            .contains(&".opencode/skills/iframe-ad".into())
    );
    assert!(
        !lockfile
            .managed_files
            .iter()
            .any(|path| path.contains("iframe-ad_"))
    );
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
    // nodus auto-registers itself as an MCP server, generating .mcp.json,
    // .codex/config.toml, and .codex/.gitignore
    assert_eq!(summary.managed_file_count, 3);

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
    assert_eq!(lockfile.managed_files.len(), 3);
    assert!(lockfile.managed_files.contains(&String::from(".mcp.json")));
    assert!(!temp.path().join(".codex/agents").exists());
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
    assert!(
        lockfile
            .managed_files
            .contains(&".github/prompts/review.md".into())
    );
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
    assert!(
        lockfile
            .managed_files
            .contains(&".nodus/packages/shared/learnings".into())
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
    assert!(
        lockfile
            .managed_files
            .contains(&".github/prompts/review.md".into())
    );
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
    assert!(
        lockfile
            .managed_files
            .contains(&".github/prompts/review.md".into())
    );
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
    assert!(
        !lockfile
            .managed_files
            .contains(&".github/prompts/review.md".into())
    );
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
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    let managed_skill_path = temp
        .path()
        .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"));
    assert!(managed_skill_path.exists());

    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared", enabled = false }
"#,
    );

    sync_all(temp.path(), cache.path());

    assert!(!managed_skill_path.exists());
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
    assert!(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/agents/{managed_agent_file}"))
            .exists()
    );

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
    assert!(
        !temp
            .path()
            .join(format!(".claude/agents/{managed_wrapper_agent_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/skills/{managed_leaf_skill_id}/SKILL.md"))
            .exists()
    );
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
    assert!(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
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
    assert!(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"))
            .exists()
    );
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
    assert!(
        temp.path()
            .join(format!(".claude/agents/{managed_agent_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/commands/{managed_command_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/rules/{managed_rule_file}"))
            .exists()
    );
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

    assert!(
        !temp
            .path()
            .join(format!(".claude/agents/{managed_agent_file}"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(".claude/commands/{managed_command_file}"))
            .exists()
    );
    assert!(
        !temp
            .path()
            .join(format!(".claude/rules/{managed_rule_file}"))
            .exists()
    );
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
    assert!(
        temp.path()
            .join(format!(".claude/rules/{managed_rule_file}"))
            .exists()
    );
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
    let shared_claude_rule_file =
        resolution_file_name(&resolution, shared, ArtifactKind::Rule, "default", "md");
    let other_claude_rule_file =
        resolution_file_name(&resolution, other, ArtifactKind::Rule, "default", "md");

    assert_ne!(shared_agent_file, other_agent_file);
    assert_ne!(shared_copilot_agent_file, other_copilot_agent_file);
    assert_ne!(shared_command_file, other_command_file);
    assert_ne!(shared_claude_rule_file, other_claude_rule_file);

    assert!(
        temp.path()
            .join(format!(".claude/agents/{shared_agent_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/agents/{other_agent_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/commands/{shared_command_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/commands/{other_command_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/rules/{shared_claude_rule_file}"))
            .exists()
    );
    assert!(
        temp.path()
            .join(format!(".claude/rules/{other_claude_rule_file}"))
            .exists()
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
    let first_skill_dir = temp.path().join(format!(".claude/skills/{first_skill_id}"));
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
    let second_skill_dir = temp
        .path()
        .join(format!(".claude/skills/{second_skill_id}"));

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
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    fs::remove_file(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md")),
    )
    .unwrap();

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
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    let managed_skill_path = temp
        .path()
        .join(format!(".claude/skills/{managed_skill_id}/SKILL.md"));
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
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    fs::remove_file(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md")),
    )
    .unwrap();

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Fixed);
    assert!(temp.path().join(".claude/skills/review/SKILL.md").exists());
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
        "[dependencies.firebase]\npath = \"vendor/firebase\"\n",
    );
    write_file(
        &temp.path().join("vendor/firebase/nodus.toml"),
        "[mcp_servers.firebase]\ncommand = \"npx\"\n",
    );
    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

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
fn doctor_missing_lockfile_with_workspace_marketplace_collision_blocks_repair() {
    let repo = create_workspace_dependency();
    let cache = cache_dir();

    sync_in_dir_with_adapters(repo.path(), cache.path(), false, false, &[Adapter::Claude]).unwrap();
    fs::remove_file(repo.path().join(LOCKFILE_NAME)).unwrap();
    write_file(
        &repo.path().join(".claude-plugin/marketplace.json"),
        "user-authored marketplace\n",
    );

    let summary = doctor_in_dir_with_mode(
        repo.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Blocked);
    assert!(summary.findings.iter().any(|finding| {
        finding.kind == DoctorFindingKind::RiskyFix
            && finding.message.contains(".claude-plugin/marketplace.json")
    }));
    assert!(!repo.path().join(LOCKFILE_NAME).exists());
    assert_eq!(
        fs::read_to_string(repo.path().join(".claude-plugin/marketplace.json")).unwrap(),
        "user-authored marketplace\n"
    );
}

#[test]
fn doctor_recovers_exact_match_workspace_marketplace_after_lockfile_loss() {
    let repo = create_workspace_dependency();
    let cache = cache_dir();

    sync_in_dir_with_adapters(repo.path(), cache.path(), false, false, &[Adapter::Claude]).unwrap();
    let expected_marketplace =
        fs::read_to_string(repo.path().join(".claude-plugin/marketplace.json")).unwrap();
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
        fs::read_to_string(repo.path().join(".claude-plugin/marketplace.json")).unwrap(),
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
    fs::remove_dir_all(temp.path().join(".codex")).unwrap();
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
    fs::remove_dir_all(temp.path().join(".codex")).unwrap();

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
    fs::remove_dir_all(temp.path().join(".codex")).unwrap();

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
    fs::remove_dir_all(temp.path().join(".codex")).unwrap();

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
    fs::remove_dir_all(temp.path().join(".codex")).unwrap();

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
    let temp = create_workspace_dependency();
    let cache = cache_dir();
    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();
    fs::remove_file(temp.path().join(LOCKFILE_NAME)).unwrap();
    fs::remove_dir_all(temp.path().join(".agents")).unwrap();
    write_file(&temp.path().join(".agents"), "user-owned file\n");

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
    assert!(temp.path().join(".agents").is_file());
}

#[test]
fn doctor_force_mode_applies_risky_cleanup_without_prompt() {
    let temp = create_workspace_dependency();
    let cache = cache_dir();
    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();
    fs::remove_file(temp.path().join(LOCKFILE_NAME)).unwrap();
    fs::remove_dir_all(temp.path().join(".agents")).unwrap();
    write_file(&temp.path().join(".agents"), "user-owned file\n");

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
    assert!(!temp.path().join(".agents").is_file());
    assert!(
        temp.path()
            .join(".agents/plugins/marketplace.json")
            .exists()
    );
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
