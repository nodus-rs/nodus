use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use semver::Version;
use tempfile::TempDir;

use super::*;
use crate::adapters::Adapter;
use crate::paths::canonicalize_path;
use crate::report::Reporter;

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut file = fs::File::create(path).unwrap();
    file.write_all(contents.as_bytes()).unwrap();
}

#[cfg(unix)]
fn create_directory_symlink_impl(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_directory_symlink_impl(target: &Path, link: &Path) -> io::Result<()> {
    let normalized_target = target
        .components()
        .fold(PathBuf::new(), |mut path, component| {
            path.push(component.as_os_str());
            path
        });
    std::os::windows::fs::symlink_dir(&normalized_target, link)
}

fn create_directory_symlink(target: &Path, link: &Path) -> bool {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    match create_directory_symlink_impl(target, link) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => false,
        Err(error) => panic!(
            "failed to create directory symlink {} -> {}: {error}",
            link.display(),
            target.display()
        ),
    }
}

fn write_valid_skill(root: &Path) {
    write_file(
        &root.join("skills/review/SKILL.md"),
        "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
    );
}

fn write_skill(root: &Path, name: &str) {
    write_file(
        &root.join("SKILL.md"),
        &format!("---\nname: {name}\ndescription: Example skill.\n---\n# {name}\n"),
    );
}

fn write_directory_placeholder(path: &Path, target: &str) {
    write_file(path, target);
}

fn write_workspace_member(root: &Path, skill_name: &str) {
    write_file(
        &root.join("skills/review/SKILL.md"),
        &format!("---\nname: {skill_name}\ndescription: Example skill.\n---\n# {skill_name}\n"),
    );
}

fn write_workspace_root_with_codex_policy(root: &Path, installation: &str, authentication: &str) {
    write_workspace_member(&root.join("plugins/axiom"), "Axiom");
    write_file(
        &root.join(MANIFEST_FILE),
        &format!(
            r#"
[workspace]
members = ["plugins/axiom"]

[workspace.package.axiom]
path = "plugins/axiom"
name = "Axiom"

[workspace.package.axiom.codex]
category = "Productivity"
installation = "{installation}"
authentication = "{authentication}"
"#
        ),
    );
}

fn write_marketplace(root: &Path, contents: &str) {
    write_file(&root.join(".claude-plugin/marketplace.json"), contents);
}

fn write_claude_plugin_json(root: &Path, version: &str) {
    write_file(
        &root.join("claude-code.json"),
        &format!("{{\n  \"name\": \"plugin\",\n  \"version\": \"{version}\"\n}}\n"),
    );
}

fn write_modern_claude_plugin_json(root: &Path, version: Option<&str>) {
    let mut fields = vec![String::from(r#"  "name": "plugin""#)];
    if let Some(version) = version {
        fields.push(format!(r#"  "version": "{version}""#));
    }
    write_file(
        &root.join(".claude-plugin/plugin.json"),
        &format!("{{\n{}\n}}\n", fields.join(",\n")),
    );
}

fn write_modern_claude_plugin_json_with_fields(root: &Path, fields: &[&str]) {
    let mut all_fields = vec![String::from(r#"  "name": "plugin""#)];
    all_fields.extend(fields.iter().map(|field| field.to_string()));
    write_file(
        &root.join(".claude-plugin/plugin.json"),
        &format!("{{\n{}\n}}\n", all_fields.join(",\n")),
    );
}

fn write_codex_marketplace(root: &Path, contents: &str) {
    write_file(&root.join(".agents/plugins/marketplace.json"), contents);
}

fn write_codex_plugin_json(root: &Path, version: &str, mcp_servers_path: Option<&str>) {
    let mut fields = vec![
        String::from(r#"  "name": "plugin""#),
        format!(r#"  "version": "{version}""#),
    ];
    if let Some(mcp_servers_path) = mcp_servers_path {
        fields.push(format!(r#"  "mcpServers": "{mcp_servers_path}""#));
    }
    write_file(
        &root.join(".codex-plugin/plugin.json"),
        &format!("{{\n{}\n}}\n", fields.join(",\n")),
    );
}

fn write_codex_mcp_config(root: &Path) {
    write_file(
        &root.join(".mcp.json"),
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

#[test]
fn loads_root_manifest_without_required_metadata() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies]
playbook_ios = { url = "https://github.com/wenext-limited/playbook-ios", tag = "v0.1.0" }
"#,
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();

    assert!(loaded.manifest.api_version.is_none());
    assert!(loaded.manifest.name.is_none());
    assert!(loaded.manifest.version.is_none());
    assert_eq!(loaded.discovered.skills[0].id, "review");
}

#[test]
fn accepts_root_project_with_only_dependencies() {
    let temp = TempDir::new().unwrap();
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", tag = "v0.1.0" }
"#,
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();
    assert!(loaded.discovered.is_empty());
    assert_eq!(loaded.manifest.dependencies.len(), 1);
    assert_eq!(
        loaded
            .manifest
            .dependencies
            .get("playbook_ios")
            .unwrap()
            .resolved_git_url()
            .unwrap(),
        "https://github.com/wenext-limited/playbook-ios"
    );
}

#[test]
fn accepts_root_project_with_only_dev_dependencies() {
    let temp = TempDir::new().unwrap();
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dev-dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", tag = "v0.1.0" }
"#,
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();
    assert!(loaded.discovered.is_empty());
    assert!(loaded.manifest.dependencies.is_empty());
    assert_eq!(loaded.manifest.dev_dependencies.len(), 1);
}

#[test]
fn accepts_workspace_root_without_discovered_root_assets() {
    let temp = TempDir::new().unwrap();
    write_workspace_member(&temp.path().join("plugins/axiom"), "Axiom");
    write_workspace_member(&temp.path().join("plugins/firebase"), "Firebase");
    write_file(
        &temp.path().join(MANIFEST_FILE),
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

    let loaded = load_root_from_dir(temp.path()).unwrap();

    assert!(loaded.discovered.is_empty());
    let members = loaded.resolved_workspace_members().unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[0].id, "axiom");
    assert_eq!(members[1].id, "firebase");
    assert_eq!(members[0].name.as_deref(), Some("Axiom"));
    assert_eq!(members[0].codex.as_ref().unwrap().category, "Productivity");
}

#[test]
fn accepts_workspace_dependency_wrapper() {
    let temp = TempDir::new().unwrap();
    write_workspace_member(&temp.path().join("plugins/axiom"), "Axiom");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[workspace]
members = ["plugins/axiom"]

[workspace.package.axiom]
path = "plugins/axiom"
name = "Axiom"
"#,
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert!(loaded.discovered.is_empty());
    assert!(loaded.manifest.workspace.is_some());
}

#[test]
fn accepts_all_supported_codex_workspace_policy_literals() {
    for installation in ["NOT_AVAILABLE", "AVAILABLE", "INSTALLED_BY_DEFAULT"] {
        let temp = TempDir::new().unwrap();
        write_workspace_root_with_codex_policy(temp.path(), installation, "ON_INSTALL");

        let loaded = load_root_from_dir(temp.path()).unwrap();
        let members = loaded.resolved_workspace_members().unwrap();
        let codex = members[0].codex.as_ref().unwrap();

        assert_eq!(codex.installation, installation);
        assert_eq!(codex.authentication, "ON_INSTALL");
    }

    for authentication in ["ON_INSTALL", "ON_USE"] {
        let temp = TempDir::new().unwrap();
        write_workspace_root_with_codex_policy(temp.path(), "AVAILABLE", authentication);

        let loaded = load_root_from_dir(temp.path()).unwrap();
        let members = loaded.resolved_workspace_members().unwrap();
        let codex = members[0].codex.as_ref().unwrap();

        assert_eq!(codex.installation, "AVAILABLE");
        assert_eq!(codex.authentication, authentication);
    }
}

#[test]
fn rejects_workspace_member_with_invalid_codex_installation_policy() {
    let temp = TempDir::new().unwrap();
    write_workspace_root_with_codex_policy(temp.path(), "ALWAYS", "ON_INSTALL");

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();

    assert!(error.contains("workspace.package.axiom.codex.installation"));
    assert!(error.contains("NOT_AVAILABLE, AVAILABLE, INSTALLED_BY_DEFAULT"));
}

#[test]
fn rejects_workspace_member_with_invalid_codex_authentication_policy() {
    let temp = TempDir::new().unwrap();
    write_workspace_root_with_codex_policy(temp.path(), "AVAILABLE", "ALWAYS");

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();

    assert!(error.contains("workspace.package.axiom.codex.authentication"));
    assert!(error.contains("ON_INSTALL, ON_USE"));
}

#[test]
fn workspace_root_ignores_invalid_member_and_warns() {
    let temp = TempDir::new().unwrap();
    write_workspace_member(&temp.path().join("plugins/axiom"), "Axiom");
    write_file(
        &temp.path().join("plugins/firebase/README.md"),
        "# Not a package\n",
    );
    write_file(
        &temp.path().join(MANIFEST_FILE),
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

    let loaded = load_root_from_dir(temp.path()).unwrap();

    assert_eq!(
        loaded
            .resolved_workspace_members()
            .unwrap()
            .into_iter()
            .map(|member| member.id)
            .collect::<Vec<_>>(),
        vec![String::from("axiom")]
    );
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("ignoring workspace member `firebase`"))
    );
}

#[test]
fn does_not_warn_for_supported_launch_hook_config() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[launch_hooks]
sync_on_startup = true
"#,
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();

    assert!(loaded.warnings.is_empty());
    assert!(loaded.manifest.sync_on_launch_enabled());
}

#[test]
fn loads_root_manifest_hooks() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[[hooks]]
id = "bash-preflight"
event = "pre_tool_use"
adapters = ["claude", "codex"]
blocking = true
timeout_sec = 15

[hooks.matcher]
tool_names = ["bash"]

[hooks.handler]
type = "command"
command = "./scripts/preflight.sh"
cwd = "session"
"#,
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();
    let hook = &loaded.manifest.hooks[0];

    assert_eq!(loaded.manifest.hooks.len(), 1);
    assert_eq!(hook.id, "bash-preflight");
    assert_eq!(hook.event, HookEvent::PreToolUse);
    assert_eq!(hook.adapters, vec![Adapter::Claude, Adapter::Codex]);
    assert_eq!(hook.timeout_sec, Some(15));
    assert!(hook.blocking);
    assert_eq!(
        hook.matcher.as_ref().unwrap().tool_names,
        vec![HookTool::Bash]
    );
    assert_eq!(hook.handler.command, "./scripts/preflight.sh");
    assert_eq!(hook.handler.cwd, HookWorkingDirectory::Session);
}

#[test]
fn lowers_legacy_launch_hook_into_effective_hooks() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[launch_hooks]
sync_on_startup = true
"#,
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();
    let hooks = loaded.manifest.effective_hooks();

    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].id, "nodus.sync_on_startup");
    assert_eq!(hooks[0].event, HookEvent::SessionStart);
    assert_eq!(
        hooks[0].matcher.as_ref().unwrap().sources,
        vec![HookSessionSource::Startup, HookSessionSource::Resume]
    );
    assert_eq!(hooks[0].handler.command, "nodus sync");
}

#[test]
fn does_not_warn_for_supported_content_root_config() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    fs::create_dir_all(temp.path().join("nodus-development")).unwrap();
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
content_roots = ["nodus-development"]
publish_root = true
"#,
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();

    assert!(loaded.warnings.is_empty());
    assert_eq!(
        loaded.manifest.content_roots,
        vec![PathBuf::from("nodus-development")]
    );
    assert!(loaded.manifest.publish_root);
}

#[test]
fn rejects_workspace_root_with_discovered_assets() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_workspace_member(&temp.path().join("plugins/axiom"), "Axiom");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[workspace]
members = ["plugins/axiom"]

[workspace.package.axiom]
path = "plugins/axiom"
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("workspace roots must not declare root-level"));
}

#[test]
fn rejects_workspace_root_with_unmatched_member_path() {
    let temp = TempDir::new().unwrap();
    write_workspace_member(&temp.path().join("plugins/axiom"), "Axiom");
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[workspace]
members = ["plugins/axiom"]

[workspace.package.firebase]
path = "plugins/firebase"
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("workspace.package.firebase.path"));
}

#[test]
fn rejects_dependency_repo_without_supported_directories() {
    let temp = TempDir::new().unwrap();
    let error = load_dependency_from_dir(temp.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("must contain at least one of"));
}

#[test]
fn accepts_dependency_repo_with_only_nested_dependencies() {
    let temp = TempDir::new().unwrap();
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", tag = "v0.1.0" }
"#,
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert!(loaded.discovered.is_empty());
    assert_eq!(loaded.manifest.dependencies.len(), 1);
}

#[test]
fn accepts_dependency_repo_with_only_mcp_servers() {
    let temp = TempDir::new().unwrap();
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[mcp_servers.firebase]
command = "npx"
args = ["-y", "firebase-tools", "mcp", "--dir", "."]
"#,
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert!(loaded.discovered.is_empty());
    assert!(loaded.manifest.dependencies.is_empty());
    assert!(loaded.manifest.mcp_servers.contains_key("firebase"));
}

#[test]
fn accepts_dependency_repo_with_only_managed_exports() {
    let temp = TempDir::new().unwrap();
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[[managed_exports]]
source = "hooks"
target = "hooks"
"#,
    );
    write_file(
        &temp.path().join("hooks/hook-stop.sh"),
        "#!/usr/bin/env bash\n",
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert!(loaded.discovered.is_empty());
    assert!(loaded.manifest.dependencies.is_empty());
    assert!(loaded.manifest.mcp_servers.is_empty());
    assert_eq!(loaded.manifest.managed_exports.len(), 1);
}

#[test]
fn accepts_dependency_repo_with_claude_marketplace_wrapper() {
    let temp = TempDir::new().unwrap();
    write_marketplace(
        temp.path(),
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
    write_file(
        &temp
            .path()
            .join(".claude-plugin/plugins/axiom/agents/reviewer.md"),
        "# Reviewer\n",
    );
    write_file(
        &temp
            .path()
            .join(".claude-plugin/plugins/axiom/commands/build.md"),
        "# Build\n",
    );
    write_file(
        &temp
            .path()
            .join(".claude-plugin/plugins/axiom/skills/review/SKILL.md"),
        "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
    );
    write_claude_plugin_json(&temp.path().join(".claude-plugin/plugins/axiom"), "2.34.0");

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert!(loaded.discovered.is_empty());
    let dependency = loaded.manifest.dependencies.get("axiom").unwrap();
    assert_eq!(
        dependency.path.as_deref(),
        Some(Path::new("./.claude-plugin/plugins/axiom"))
    );
    assert_eq!(dependency.tag, None);
    assert!(dependency.version.is_none());
    assert_eq!(
        loaded.manifest.version,
        Some(Version::parse("2.34.0").unwrap())
    );

    let package_files = loaded.package_files().unwrap();
    assert!(package_files.contains(
        &canonicalize_path(&temp.path().join(".claude-plugin/marketplace.json")).unwrap()
    ));
}

#[test]
fn discovers_symlinked_skill_directories_inside_package_root() {
    let temp = TempDir::new().unwrap();
    write_file(
        &temp.path().join("vendor/shared/skills/review/SKILL.md"),
        "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
    );
    if !create_directory_symlink(
        Path::new("../vendor/shared/skills/review"),
        &temp.path().join("skills/review"),
    ) {
        return;
    }

    let loaded = load_dependency_from_dir(temp.path()).unwrap();
    let package_files = loaded.package_files().unwrap();

    assert_eq!(
        loaded
            .discovered
            .skills
            .iter()
            .map(|skill| skill.id.as_str())
            .collect::<Vec<_>>(),
        vec!["review"]
    );
    assert!(
        package_files
            .iter()
            .any(|path| path.ends_with(Path::new("skills/review/SKILL.md"))),
        "{package_files:?}"
    );
    let skill_file = package_files
        .iter()
        .find(|path| path.ends_with(Path::new("skills/review/SKILL.md")))
        .unwrap();
    let contents = String::from_utf8(loaded.read_package_file(skill_file).unwrap()).unwrap();
    assert!(contents.contains("name: Review"));
}

#[test]
fn discovers_placeholder_skill_directories_inside_package_root() {
    let temp = TempDir::new().unwrap();
    write_file(
        &temp.path().join("vendor/shared/skills/review/SKILL.md"),
        "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
    );
    write_directory_placeholder(
        &temp.path().join("skills/review"),
        "../vendor/shared/skills/review",
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();
    let package_files = loaded.package_files().unwrap();

    assert_eq!(
        loaded
            .discovered
            .skills
            .iter()
            .map(|skill| skill.id.as_str())
            .collect::<Vec<_>>(),
        vec!["review"]
    );
    assert!(
        package_files
            .iter()
            .any(|path| path.ends_with(Path::new("skills/review/SKILL.md"))),
        "{package_files:?}"
    );
}

#[test]
fn accepts_dependency_repo_with_structured_claude_marketplace_sources() {
    let temp = TempDir::new().unwrap();
    write_marketplace(
        temp.path(),
        r#"{
  "plugins": [
    {
      "name": "External",
      "source": {
        "source": "url",
        "url": "https://github.com/acme/external.git",
        "sha": "aa70dbdbbbb843e94a794c10c2b13f5dd66b5e40"
      }
    },
    {
      "name": "Subdir",
      "source": {
        "source": "git-subdir",
        "url": "owner/repo",
        "path": "plugins/subdir",
        "ref": "main"
      }
    },
    {
      "name": "Stagehand",
      "source": {
        "source": "github",
        "repo": "browserbase/agent-browse"
      }
    }
  ]
}"#,
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    let external = loaded.manifest.dependencies.get("external").unwrap();
    assert_eq!(external.github.as_deref(), Some("acme/external"));
    assert_eq!(
        external.revision.as_deref(),
        Some("aa70dbdbbbb843e94a794c10c2b13f5dd66b5e40")
    );
    assert!(external.subpath.is_none());

    let subdir = loaded.manifest.dependencies.get("subdir").unwrap();
    assert_eq!(subdir.github.as_deref(), Some("owner/repo"));
    assert_eq!(subdir.branch.as_deref(), Some("main"));
    assert_eq!(subdir.subpath.as_deref(), Some(Path::new("plugins/subdir")));

    let stagehand = loaded.manifest.dependencies.get("stagehand").unwrap();
    assert_eq!(
        stagehand.github.as_deref(),
        Some("browserbase/agent-browse")
    );
    assert!(stagehand.revision.is_none());
    assert!(stagehand.branch.is_none());
    assert!(stagehand.subpath.is_none());
}

#[test]
fn imports_firebase_style_marketplace_mcp_servers() {
    let temp = TempDir::new().unwrap();
    write_marketplace(
        temp.path(),
        r#"{
  "plugins": [
    {
      "name": "firebase",
      "version": "1.0.0",
      "source": "./",
      "mcpServers": {
        "firebase": {
          "description": "Firebase MCP server",
          "command": "npx",
          "args": ["-y", "firebase-tools", "mcp", "--dir", "."],
          "env": {
            "IS_FIREBASE_MCP": "true"
          }
        }
      }
    }
  ]
}"#,
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert!(loaded.manifest.dependencies.is_empty());
    assert_eq!(
        loaded.manifest.version,
        Some(Version::parse("1.0.0").unwrap())
    );
    let server = loaded.manifest.mcp_servers.get("firebase").unwrap();
    assert_eq!(server.command.as_deref(), Some("npx"));
    assert!(server.url.is_none());
    assert_eq!(
        server.args,
        vec!["-y", "firebase-tools", "mcp", "--dir", "."]
    );
    assert_eq!(
        server.env,
        BTreeMap::from([(String::from("IS_FIREBASE_MCP"), String::from("true"))])
    );
}

#[test]
fn imports_firebase_style_marketplace_url_mcp_servers() {
    let temp = TempDir::new().unwrap();
    write_marketplace(
        temp.path(),
        r#"{
  "plugins": [
    {
      "name": "figma",
      "version": "1.0.0",
      "source": "./",
      "mcpServers": {
        "figma": {
          "url": "http://127.0.0.1:3845/mcp",
          "enabled": false
        }
      }
    }
  ]
}"#,
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert!(loaded.manifest.dependencies.is_empty());
    assert_eq!(
        loaded.manifest.version,
        Some(Version::parse("1.0.0").unwrap())
    );
    let server = loaded.manifest.mcp_servers.get("figma").unwrap();
    assert!(server.command.is_none());
    assert_eq!(server.url.as_deref(), Some("http://127.0.0.1:3845/mcp"));
    assert!(!server.enabled);
}

#[test]
fn accepts_marketplace_plugin_that_points_at_root_claude_plugin_metadata() {
    let temp = TempDir::new().unwrap();
    write_marketplace(
        temp.path(),
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
    write_modern_claude_plugin_json(temp.path(), Some("1.0.0"));
    write_file(
        &temp.path().join(".mcp.json"),
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
    write_file(&temp.path().join("tools.yaml"), "version: v1\n");

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert!(loaded.discovered.is_empty());
    assert!(loaded.manifest.dependencies.is_empty());
    assert_eq!(
        loaded.manifest.version,
        Some(Version::parse("1.0.0").unwrap())
    );
    let server = loaded.manifest.mcp_servers.get("atlan").unwrap();
    assert_eq!(server.transport_type.as_deref(), Some("http"));
    assert_eq!(server.url.as_deref(), Some("https://mcp.atlan.com/mcp"));
}

#[test]
fn imports_all_marketplace_plugins_in_sorted_alias_order() {
    let temp = TempDir::new().unwrap();
    write_marketplace(
        temp.path(),
        r#"{
  "plugins": [
    {
      "name": "Zeta Plugin",
      "source": "./plugins/zeta"
    },
    {
      "name": "Alpha Plugin",
      "source": "./plugins/alpha"
    }
  ]
}"#,
    );
    write_file(
        &temp.path().join("plugins/zeta/skills/zeta/SKILL.md"),
        "---\nname: Zeta\ndescription: Zeta skill.\n---\n# Zeta\n",
    );
    write_file(
        &temp.path().join("plugins/alpha/skills/alpha/SKILL.md"),
        "---\nname: Alpha\ndescription: Alpha skill.\n---\n# Alpha\n",
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert_eq!(
        loaded
            .manifest
            .dependencies
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["alpha_plugin", "zeta_plugin"]
    );
}

#[test]
fn marketplace_sources_are_resolved_from_repo_root() {
    let temp = TempDir::new().unwrap();
    write_marketplace(
        temp.path(),
        r#"{
  "plugins": [
    {
      "name": "Axiom",
      "source": "./plugins/axiom"
    }
  ]
}"#,
    );
    write_file(
        &temp.path().join("plugins/axiom/skills/review/SKILL.md"),
        "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert_eq!(
        loaded
            .manifest
            .dependencies
            .get("axiom")
            .and_then(|dependency| dependency.path.as_deref()),
        Some(Path::new("./plugins/axiom"))
    );
}

#[test]
fn skips_missing_claude_marketplace_local_plugin_sources_with_warning() {
    let temp = TempDir::new().unwrap();
    write_marketplace(
        temp.path(),
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
    write_file(
        &temp.path().join("plugins/axiom/skills/review/SKILL.md"),
        "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert_eq!(
        loaded
            .manifest
            .dependencies
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["axiom"]
    );
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("skipping marketplace plugin `Missing`"));
    assert!(loaded.warnings[0].contains("./plugins/missing"));
}

#[test]
fn reads_claude_plugin_version_from_json() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_claude_plugin_json(temp.path(), "2.34.0");

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert_eq!(
        loaded.manifest.version,
        Some(Version::parse("2.34.0").unwrap())
    );
}

#[test]
fn ignores_non_semver_claude_plugin_version_from_json() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_claude_plugin_json(temp.path(), "latest");

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert!(loaded.manifest.version.is_none());
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("ignoring non-SemVer Claude plugin version `latest`"));
    assert!(loaded.warnings[0].contains("claude-code.json"));
}

#[test]
fn accepts_dependency_repo_with_only_modern_claude_plugin_metadata_and_flat_mcp_servers() {
    let temp = TempDir::new().unwrap();
    write_modern_claude_plugin_json(temp.path(), Some("2.34.0"));
    write_file(
        &temp.path().join(".mcp.json"),
        r#"{
  "asana": {
    "type": "sse",
    "url": "https://mcp.asana.com/sse"
  }
}
"#,
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert!(loaded.discovered.is_empty());
    assert_eq!(
        loaded.manifest.version,
        Some(Version::parse("2.34.0").unwrap())
    );
    let server = loaded.manifest.mcp_servers.get("asana").unwrap();
    assert_eq!(server.transport_type.as_deref(), Some("sse"));
    assert_eq!(server.url.as_deref(), Some("https://mcp.asana.com/sse"));
    let package_files = loaded.package_files().unwrap();
    assert!(
        package_files
            .contains(&canonicalize_path(&temp.path().join(".claude-plugin/plugin.json")).unwrap())
    );
    assert!(package_files.contains(&canonicalize_path(&temp.path().join(".mcp.json")).unwrap()));
}

#[test]
fn discovers_modern_claude_plugin_extra_component_paths_outside_standard_roots() {
    let temp = TempDir::new().unwrap();
    write_modern_claude_plugin_json_with_fields(
        temp.path(),
        &[
            r#"  "skills": ["./plugin-skills"]"#,
            r#"  "agents": "./security.md""#,
            r#"  "commands": ["./build.md"]"#,
        ],
    );
    write_skill(&temp.path().join("plugin-skills/review"), "Review");
    write_file(&temp.path().join("security.md"), "# Security\n");
    write_file(&temp.path().join("build.md"), "# Build\n");

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert_eq!(
        loaded
            .discovered
            .skills
            .iter()
            .map(|entry| (entry.id.as_str(), entry.path.as_path()))
            .collect::<Vec<_>>(),
        vec![("review", Path::new("plugin-skills/review"))]
    );
    assert_eq!(
        loaded
            .discovered
            .agents
            .iter()
            .map(|entry| (entry.id.as_str(), entry.path.as_path()))
            .collect::<Vec<_>>(),
        vec![("security", Path::new("security.md"))]
    );
    assert_eq!(
        loaded
            .discovered
            .commands
            .iter()
            .map(|entry| (entry.id.as_str(), entry.path.as_path()))
            .collect::<Vec<_>>(),
        vec![("build", Path::new("build.md"))]
    );
}

#[test]
fn discovers_object_mapped_modern_claude_plugin_commands_from_source_paths() {
    let temp = TempDir::new().unwrap();
    write_modern_claude_plugin_json_with_fields(
        temp.path(),
        &[r#"  "commands": { "about": { "source": "./README.md" } }"#],
    );
    write_file(&temp.path().join("README.md"), "# About\n");

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert_eq!(loaded.discovered.commands.len(), 1);
    assert_eq!(loaded.discovered.commands[0].id, "about");
    assert_eq!(
        loaded.discovered.commands[0].path,
        PathBuf::from("README.md")
    );
}

#[test]
fn warns_when_modern_claude_plugin_command_uses_inline_content() {
    let temp = TempDir::new().unwrap();
    write_modern_claude_plugin_json_with_fields(
        temp.path(),
        &[r#"  "commands": { "about": { "content": "Inline only" } }"#],
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert!(loaded.discovered.commands.is_empty());
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("unsupported inline Claude plugin command `about`"))
    );
}

#[test]
fn warns_when_modern_claude_plugin_command_points_to_directory() {
    let temp = TempDir::new().unwrap();
    write_modern_claude_plugin_json_with_fields(temp.path(), &[r#"  "commands": "./command-dir""#]);
    write_file(&temp.path().join("command-dir/build.md"), "# Build\n");

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert!(loaded.discovered.commands.is_empty());
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("directory-backed commands are not supported"))
    );
}

#[test]
fn imports_modern_claude_plugin_manifest_inline_mcp_servers() {
    let temp = TempDir::new().unwrap();
    write_modern_claude_plugin_json_with_fields(
        temp.path(),
        &[r#"  "mcpServers": { "github": { "type": "http", "url": "https://example.com/mcp" } }"#],
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    let github = loaded.manifest.mcp_servers.get("github").unwrap();
    assert_eq!(github.transport_type.as_deref(), Some("http"));
    assert_eq!(github.url.as_deref(), Some("https://example.com/mcp"));
}

#[test]
fn imports_modern_claude_plugin_manifest_mcp_servers_from_relative_json_path() {
    let temp = TempDir::new().unwrap();
    write_modern_claude_plugin_json_with_fields(
        temp.path(),
        &[r#"  "mcpServers": "./config/mcp.json""#],
    );
    write_file(
        &temp.path().join("config/mcp.json"),
        r#"{
  "mcpServers": {
    "figma": {
      "url": "http://127.0.0.1:3845/mcp"
    }
  }
}
"#,
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    let figma = loaded.manifest.mcp_servers.get("figma").unwrap();
    assert_eq!(figma.url.as_deref(), Some("http://127.0.0.1:3845/mcp"));
    let package_files = loaded.package_files().unwrap();
    assert!(
        package_files.contains(&canonicalize_path(&temp.path().join("config/mcp.json")).unwrap())
    );
}

#[test]
fn overlays_modern_claude_plugin_manifest_mcp_servers_in_declaration_order() {
    let temp = TempDir::new().unwrap();
    write_modern_claude_plugin_json_with_fields(
        temp.path(),
        &[
            r#"  "mcpServers": ["./config/path.json", { "shared": { "command": "inline" }, "inlineOnly": { "command": "bun", "args": ["run", "--cwd", "${CLAUDE_PLUGIN_ROOT}", "start"] } }, "./config/final.json"]"#,
        ],
    );
    write_file(
        &temp.path().join(".mcp.json"),
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
        &temp.path().join("config/path.json"),
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
    write_file(&temp.path().join("tools.yaml"), "version: v1\n");
    write_file(
        &temp.path().join("config/final.json"),
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

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert_eq!(
        loaded
            .manifest
            .mcp_servers
            .get("shared")
            .unwrap()
            .command
            .as_deref(),
        Some("final")
    );
    assert_eq!(
        loaded
            .manifest
            .mcp_servers
            .get("baseOnly")
            .unwrap()
            .command
            .as_deref(),
        Some("base-only")
    );
    assert_eq!(
        loaded
            .manifest
            .mcp_servers
            .get("pathOnly")
            .unwrap()
            .command
            .as_deref(),
        Some("path-only")
    );
    let inline_only = loaded.manifest.mcp_servers.get("inlineOnly").unwrap();
    assert_eq!(inline_only.command.as_deref(), Some("bun"));
    assert_eq!(
        inline_only.args,
        vec![String::from("run"), String::from("start")]
    );
    assert_eq!(
        inline_only
            .cwd
            .as_ref()
            .and_then(|cwd| canonicalize_path(cwd).ok()),
        Some(canonicalize_path(temp.path()).unwrap())
    );
    let rooted = loaded.manifest.mcp_servers.get("rooted").unwrap();
    assert_eq!(rooted.command.as_deref(), Some("toolbox"));
    assert_eq!(rooted.args[0], "--tools-file");
    assert_eq!(rooted.args[2], "--stdio");
    assert!(!rooted.args[1].contains("${CLAUDE_PLUGIN_ROOT}"));
    assert_eq!(
        canonicalize_path(Path::new(&rooted.args[1])).unwrap(),
        canonicalize_path(&temp.path().join("tools.yaml")).unwrap()
    );
}

#[test]
fn warns_when_modern_claude_plugin_manifest_mcp_servers_use_unsupported_paths() {
    let temp = TempDir::new().unwrap();
    write_modern_claude_plugin_json_with_fields(
        temp.path(),
        &[r#"  "mcpServers": "./plugin.mcpb""#],
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert!(loaded.manifest.mcp_servers.is_empty());
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("unsupported Claude plugin field `mcpServers` path"))
    );
}

#[test]
fn ignores_non_semver_modern_claude_plugin_version_in_metadata() {
    let temp = TempDir::new().unwrap();
    write_modern_claude_plugin_json(temp.path(), Some("latest"));
    write_file(
        &temp.path().join(".mcp.json"),
        r#"{
  "asana": {
    "type": "sse",
    "url": "https://mcp.asana.com/sse"
  }
}
"#,
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert!(loaded.manifest.version.is_none());
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("ignoring non-SemVer Claude plugin version `latest`"));
    assert!(loaded.warnings[0].contains(".claude-plugin/plugin.json"));
    assert!(loaded.manifest.mcp_servers.contains_key("asana"));
}

#[test]
fn imports_modern_claude_plugin_wrapped_mcp_servers_and_normalizes_plugin_root_cwd() {
    let temp = TempDir::new().unwrap();
    write_modern_claude_plugin_json(temp.path(), Some("2.34.0"));
    write_file(
        &temp.path().join(".mcp.json"),
        r#"{
  "mcpServers": {
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
}
"#,
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    let github = loaded.manifest.mcp_servers.get("github").unwrap();
    assert_eq!(github.transport_type.as_deref(), Some("http"));
    assert_eq!(
        github.headers,
        BTreeMap::from([(
            String::from("Authorization"),
            String::from("Bearer ${GITHUB_PERSONAL_ACCESS_TOKEN}")
        )])
    );

    let discord = loaded.manifest.mcp_servers.get("discord").unwrap();
    assert_eq!(discord.command.as_deref(), Some("bun"));
    assert_eq!(
        discord.args,
        vec![
            String::from("run"),
            String::from("--shell=bun"),
            String::from("--silent"),
            String::from("start"),
        ]
    );
    assert_eq!(
        discord
            .cwd
            .as_ref()
            .and_then(|cwd| canonicalize_path(cwd).ok()),
        Some(canonicalize_path(&loaded.root).unwrap())
    );
}

#[test]
fn normalizes_claude_plugin_root_arg_paths_in_mcp_config() {
    let temp = TempDir::new().unwrap();
    write_modern_claude_plugin_json(temp.path(), Some("2.34.0"));
    write_file(
        &temp.path().join(".mcp.json"),
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

    let loaded = load_dependency_from_dir(temp.path()).unwrap();
    let server = loaded
        .manifest
        .mcp_servers
        .get("cockroachdb-toolbox")
        .unwrap();
    assert_eq!(server.command.as_deref(), Some("toolbox"));
    assert_eq!(
        server.args[..2].to_vec(),
        vec![String::from("--tools-file"), server.args[1].clone()]
    );
    assert_eq!(server.args[2], String::from("--stdio"));
    assert!(!server.args[1].contains("${CLAUDE_PLUGIN_ROOT}"));
    assert!(Path::new(&server.args[1]).is_absolute());
    assert!(server.args[1].ends_with("/tools.yaml"));
}

#[test]
fn reads_codex_plugin_version_and_mcp_servers_from_json() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_codex_mcp_config(temp.path());
    write_codex_plugin_json(temp.path(), "2.34.0", Some("./.mcp.json"));

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert_eq!(
        loaded.manifest.version,
        Some(Version::parse("2.34.0").unwrap())
    );
    let server = loaded.manifest.mcp_servers.get("figma").unwrap();
    assert!(server.command.is_none());
    assert_eq!(server.url.as_deref(), Some("http://127.0.0.1:3845/mcp"));
    let package_files = loaded.package_files().unwrap();
    assert!(
        package_files
            .contains(&canonicalize_path(&temp.path().join(".codex-plugin/plugin.json")).unwrap())
    );
    assert!(package_files.contains(&canonicalize_path(&temp.path().join(".mcp.json")).unwrap()));
}

#[test]
fn allows_identical_mcp_servers_from_claude_and_codex_plugin_metadata() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_modern_claude_plugin_json(temp.path(), Some("2.34.0"));
    write_codex_mcp_config(temp.path());
    write_codex_plugin_json(temp.path(), "2.34.0", Some("./.mcp.json"));

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert_eq!(loaded.manifest.mcp_servers.len(), 1);
    let server = loaded.manifest.mcp_servers.get("figma").unwrap();
    assert_eq!(server.url.as_deref(), Some("http://127.0.0.1:3845/mcp"));
}

#[test]
fn deduplicates_standard_roots_referenced_by_claude_plugin_metadata() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(&temp.path().join("agents/reviewer.md"), "# Reviewer\n");
    write_file(
        &temp.path().join("commands/review.md"),
        "Review this code.\n",
    );
    write_modern_claude_plugin_json_with_fields(
        temp.path(),
        &[
            r#"  "skills": ["./skills"]"#,
            r#"  "agents": ["./agents/reviewer.md"]"#,
            r#"  "commands": ["./commands/review.md"]"#,
        ],
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert_eq!(loaded.discovered.skills.len(), 1);
    assert_eq!(loaded.discovered.skills[0].id, "review");
    assert_eq!(loaded.discovered.agents.len(), 1);
    assert_eq!(loaded.discovered.agents[0].id, "reviewer");
    assert_eq!(loaded.discovered.commands.len(), 1);
    assert_eq!(loaded.discovered.commands[0].id, "review");
}

#[test]
fn rejects_conflicting_mcp_servers_from_claude_and_codex_plugin_metadata() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_modern_claude_plugin_json(temp.path(), Some("2.34.0"));
    write_file(
        &temp.path().join(".mcp.json"),
        r#"{
  "mcpServers": {
    "figma": {
      "url": "http://127.0.0.1:3845/mcp"
    }
  }
}
"#,
    );
    write_file(
        &temp.path().join("config/codex-mcp.json"),
        r#"{
  "mcpServers": {
    "figma": {
      "url": "http://127.0.0.1:9999/mcp"
    }
  }
}
"#,
    );
    write_codex_plugin_json(temp.path(), "2.34.0", Some("./config/codex-mcp.json"));

    let error = load_dependency_from_dir(temp.path())
        .unwrap_err()
        .to_string();
    let normalized_error = error.replace('\\', "/");

    assert!(error.contains("declares conflicting MCP server `figma`"));
    assert!(normalized_error.contains("config/codex-mcp.json"));
}

#[test]
fn ignores_non_semver_codex_plugin_version_in_metadata() {
    let temp = TempDir::new().unwrap();
    write_codex_mcp_config(temp.path());
    write_codex_plugin_json(temp.path(), "latest", Some("./.mcp.json"));

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert!(loaded.manifest.version.is_none());
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("ignoring non-SemVer Codex plugin version `latest`"));
    assert!(loaded.warnings[0].contains(".codex-plugin/plugin.json"));
    assert!(loaded.manifest.mcp_servers.contains_key("figma"));
}

#[test]
fn rejects_marketplace_with_invalid_json() {
    let temp = TempDir::new().unwrap();
    write_marketplace(temp.path(), "{");

    let error = load_dependency_from_dir(temp.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("failed to parse JSON"));
}

#[test]
fn rejects_marketplace_without_plugins() {
    let temp = TempDir::new().unwrap();
    write_marketplace(temp.path(), r#"{ "plugins": [] }"#);

    let error = load_dependency_from_dir(temp.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("must declare at least one plugin"));
}

#[test]
fn ignores_marketplace_plugins_with_duplicate_aliases() {
    let temp = TempDir::new().unwrap();
    write_marketplace(
        temp.path(),
        r#"{
  "plugins": [
    {
      "name": "Axiom",
      "source": "./plugins/one"
    },
    {
      "name": "axiom",
      "source": "./plugins/two"
    }
  ]
}"#,
    );
    write_file(
        &temp.path().join("plugins/one/skills/one/SKILL.md"),
        "---\nname: One\ndescription: One skill.\n---\n# One\n",
    );
    write_file(
        &temp.path().join("plugins/two/skills/two/SKILL.md"),
        "---\nname: Two\ndescription: Two skill.\n---\n# Two\n",
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();
    assert_eq!(loaded.manifest.dependencies.len(), 1);
    assert!(loaded.manifest.dependencies.contains_key("axiom"));
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("duplicate plugin alias `axiom`"))
    );
}

#[test]
fn ignores_marketplace_with_escaping_source_path() {
    let temp = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    write_file(
        &outside.path().join("skills/review/SKILL.md"),
        "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
    );
    let escaping_source = format!(
        "../{}",
        outside.path().file_name().unwrap().to_string_lossy()
    );
    write_marketplace(
        temp.path(),
        &format!(
            r#"{{
  "plugins": [
    {{
      "name": "Axiom",
      "source": "{escaping_source}"
    }}
  ]
}}"#
        ),
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();
    assert!(loaded.manifest.dependencies.is_empty());
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("plugin `Axiom` has invalid source"))
    );
}

#[test]
fn ignores_marketplace_with_missing_source_directory() {
    let temp = TempDir::new().unwrap();
    write_marketplace(
        temp.path(),
        r#"{
  "plugins": [
    {
      "name": "Axiom",
      "source": "./plugins/missing"
    }
  ]
}"#,
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();
    assert!(loaded.manifest.dependencies.is_empty());
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("local source `./plugins/missing` is missing"))
    );
}

#[test]
fn ignores_marketplace_plugin_source_that_is_not_a_directory() {
    let temp = TempDir::new().unwrap();
    write_marketplace(
        temp.path(),
        r#"{
  "plugins": [
    {
      "name": "Axiom",
      "source": "./plugins/axiom"
    }
  ]
}"#,
    );
    write_file(&temp.path().join("plugins/axiom"), "not a directory\n");

    let loaded = load_dependency_from_dir(temp.path()).unwrap();
    assert!(loaded.manifest.dependencies.is_empty());
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("must point to a directory"))
    );
}

#[test]
fn ignores_marketplace_plugin_source_that_is_not_a_nodus_package() {
    let temp = TempDir::new().unwrap();
    write_marketplace(
        temp.path(),
        r#"{
  "plugins": [
    {
      "name": "Axiom",
      "source": "./plugins/axiom"
    }
  ]
}"#,
    );
    write_file(
        &temp.path().join("plugins/axiom/README.md"),
        "# Not a package\n",
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();
    assert!(loaded.manifest.dependencies.is_empty());
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("does not expose Nodus-manageable package content"))
    );
}

#[test]
fn skips_marketplace_with_docs_only_local_plugin_source() {
    let temp = TempDir::new().unwrap();
    write_marketplace(
        temp.path(),
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
        &temp.path().join("plugins/docs/README.md"),
        "# Informational plugin\n",
    );
    write_file(
        &temp.path().join("plugins/axiom/skills/review/SKILL.md"),
        "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert_eq!(
        loaded
            .manifest
            .dependencies
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["axiom"]
    );
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("skipping marketplace plugin `Docs Only`"));
    assert!(loaded.warnings[0].contains("./plugins/docs"));
}

#[test]
fn skips_marketplace_with_hook_only_claude_plugin_source() {
    let temp = TempDir::new().unwrap();
    write_marketplace(
        temp.path(),
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
    write_modern_claude_plugin_json(&temp.path().join("plugins/hook-only"), Some("1.0.0"));
    write_file(
        &temp.path().join("plugins/hook-only/hooks/hooks.json"),
        "{\n  \"hooks\": []\n}\n",
    );
    write_file(
        &temp.path().join("plugins/axiom/skills/review/SKILL.md"),
        "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert_eq!(
        loaded
            .manifest
            .dependencies
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["axiom"]
    );
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("skipping marketplace plugin `Hook Only`"));
    assert!(loaded.warnings[0].contains("./plugins/hook-only"));
}

#[test]
fn ignores_marketplace_with_mcp_server_path_indirection() {
    let temp = TempDir::new().unwrap();
    write_marketplace(
        temp.path(),
        r#"{
  "plugins": [
    {
      "name": "firebase",
      "source": "./",
      "mcpServers": "./mcp.json"
    }
  ]
}"#,
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();
    assert!(loaded.manifest.dependencies.is_empty());
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("unsupported `mcpServers` path"))
    );
}

#[test]
fn rejects_marketplace_with_plugin_root_interpolation_in_mcp_server() {
    let temp = TempDir::new().unwrap();
    write_marketplace(
        temp.path(),
        r#"{
  "plugins": [
    {
      "name": "firebase",
      "source": "./",
      "mcpServers": {
        "firebase": {
          "command": "${CLAUDE_PLUGIN_ROOT}/server"
        }
      }
    }
  ]
}"#,
    );

    let error = load_dependency_from_dir(temp.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("${CLAUDE_PLUGIN_ROOT}"));
}

#[test]
fn accepts_dependency_repo_with_codex_marketplace_wrapper() {
    let temp = TempDir::new().unwrap();
    write_codex_marketplace(
        temp.path(),
        r#"{
  "name": "workspace-plugins",
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
    write_file(
        &temp.path().join("plugins/axiom/skills/review/SKILL.md"),
        "---\nname: Review\ndescription: Review code safely.\n---\n# Review\n",
    );
    write_codex_plugin_json(&temp.path().join("plugins/axiom"), "2.34.0", None);

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert!(loaded.discovered.is_empty());
    let dependency = loaded.manifest.dependencies.get("axiom").unwrap();
    assert_eq!(
        dependency.path.as_deref(),
        Some(Path::new("./plugins/axiom"))
    );
    assert_eq!(
        loaded.manifest.version,
        Some(Version::parse("2.34.0").unwrap())
    );

    let package_files = loaded.package_files().unwrap();
    assert!(package_files.contains(
        &canonicalize_path(&temp.path().join(".agents/plugins/marketplace.json")).unwrap()
    ));
}

#[test]
fn ignores_codex_marketplace_plugin_source_that_points_at_package_root() {
    let temp = TempDir::new().unwrap();
    write_codex_marketplace(
        temp.path(),
        r#"{
  "plugins": [
    {
      "name": "Axiom",
      "source": {
        "source": "local",
        "path": "./"
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
    write_codex_plugin_json(temp.path(), "2.34.0", None);

    let loaded = load_dependency_from_dir(temp.path()).unwrap();
    assert!(loaded.manifest.dependencies.is_empty());
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("must not point at the package root"))
    );
}

#[test]
fn prefers_standard_layout_over_marketplace_fallback() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_marketplace(
        temp.path(),
        r#"{
  "plugins": [
    {
      "name": "Axiom",
      "source": "./plugins/axiom"
    }
  ]
}"#,
    );
    write_file(
        &temp.path().join("plugins/axiom/skills/axiom/SKILL.md"),
        "---\nname: Axiom\ndescription: Axiom skill.\n---\n# Axiom\n",
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert_eq!(
        loaded
            .discovered
            .skills
            .iter()
            .map(|skill| skill.id.as_str())
            .collect::<Vec<_>>(),
        vec!["review"]
    );
    assert!(loaded.manifest.dependencies.is_empty());
}

#[test]
fn marketplace_fallback_still_runs_with_only_dev_dependencies() {
    let temp = TempDir::new().unwrap();
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dev-dependencies]
tooling = { github = "example/tooling", tag = "v0.1.0" }
"#,
    );
    write_marketplace(
        temp.path(),
        r#"{
  "plugins": [
    {
      "name": "Axiom",
      "source": "./plugins/axiom"
    }
  ]
}"#,
    );
    write_file(
        &temp.path().join("plugins/axiom/skills/axiom/SKILL.md"),
        "---\nname: Axiom\ndescription: Axiom skill.\n---\n# Axiom\n",
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();

    assert!(loaded.manifest.dev_dependencies.contains_key("tooling"));
    assert!(loaded.manifest.dependencies.contains_key("axiom"));
}

#[test]
fn rejects_invalid_git_dependency_without_tag() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies]
playbook_ios = { url = "https://github.com/wenext-limited/playbook-ios" }
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("must declare `tag`"));
}

#[test]
fn rejects_invalid_github_dependency_reference() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies]
playbook_ios = { github = "wenext-limited", tag = "v0.1.0" }
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("must use the format `owner/repo`"));
}

#[test]
fn rejects_invalid_skill_frontmatter() {
    let temp = TempDir::new().unwrap();
    write_file(
        &temp.path().join("skills/review/SKILL.md"),
        "---\nname: Review\n---\n# Review\n",
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("skill `review` is invalid"));
}

#[test]
fn accepts_unquoted_description_with_colon() {
    let temp = TempDir::new().unwrap();
    write_file(
        &temp.path().join("skills/ios-websocket/SKILL.md"),
        "---\nname: ios-websocket\ndescription: Use when a task involves WebSocket push-notification subscriptions. Trigger this skill for any of: subscribing to a new server push URI.\n---\n# iOS WebSocket\n",
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();

    assert_eq!(loaded.discovered.skills[0].id, "ios-websocket");
}

#[test]
fn accepts_skill_frontmatter_without_name_by_falling_back_to_folder_name() {
    let temp = TempDir::new().unwrap();
    write_file(
        &temp.path().join("skills/atomic-agents/SKILL.md"),
        "---\ndescription: Use when a task involves atomic agents.\n---\n# Atomic Agents\n",
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();

    assert_eq!(loaded.discovered.skills[0].id, "atomic-agents");
}

#[test]
fn discovers_nested_skills_under_category_directories() {
    let temp = TempDir::new().unwrap();
    write_file(&temp.path().join("skills/operations/.gitkeep"), "");
    write_file(
        &temp.path().join("skills/onboarding/molt-fetch/SKILL.md"),
        "---\nname: Molt Fetch\ndescription: Run molt fetch.\n---\n# Molt Fetch\n",
    );
    write_file(
        &temp
            .path()
            .join("skills/security/configuring-audit-logging/SKILL.md"),
        "---\nname: Audit Logging\ndescription: Configure audit logging.\n---\n# Audit Logging\n",
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();

    assert_eq!(
        loaded
            .discovered
            .skills
            .iter()
            .map(|skill| (skill.id.as_str(), skill.path.as_path()))
            .collect::<Vec<_>>(),
        vec![
            (
                "onboarding__molt-fetch",
                Path::new("skills/onboarding/molt-fetch"),
            ),
            (
                "security__configuring-audit-logging",
                Path::new("skills/security/configuring-audit-logging"),
            ),
        ]
    );
}

#[test]
fn discovers_agents_rules_and_commands() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(&temp.path().join("agents/security.md"), "# Security\n");
    write_file(&temp.path().join("rules/default.rules"), "allow = []\n");
    write_file(&temp.path().join("commands/build.txt"), "cargo test\n");

    let loaded = load_root_from_dir(temp.path()).unwrap();

    assert_eq!(loaded.discovered.skills[0].id, "review");
    assert_eq!(loaded.discovered.agents[0].id, "security");
    assert_eq!(loaded.discovered.rules[0].id, "default");
    assert_eq!(loaded.discovered.commands[0].id, "build");
}

#[test]
fn discovers_artifacts_from_configured_content_root() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
content_roots = ["nodus-development"]
"#,
    );
    write_skill(
        &temp.path().join("nodus-development/skills/checks"),
        "Checks",
    );
    write_file(
        &temp.path().join("nodus-development/agents/reviewer.md"),
        "# Reviewer\n",
    );
    write_file(
        &temp.path().join("nodus-development/rules/policy.md"),
        "# Policy\n",
    );
    write_file(
        &temp.path().join("nodus-development/commands/build.txt"),
        "cargo test\n",
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();

    assert_eq!(
        loaded
            .discovered
            .skills
            .iter()
            .map(|entry| (entry.id.as_str(), entry.path.as_path()))
            .collect::<Vec<_>>(),
        vec![
            ("checks", Path::new("nodus-development/skills/checks")),
            ("review", Path::new("skills/review")),
        ]
    );
    assert_eq!(
        loaded
            .discovered
            .agents
            .iter()
            .map(|entry| (entry.id.as_str(), entry.path.as_path()))
            .collect::<Vec<_>>(),
        vec![(
            "reviewer",
            Path::new("nodus-development/agents/reviewer.md")
        )]
    );
    assert_eq!(
        loaded
            .discovered
            .rules
            .iter()
            .map(|entry| (entry.id.as_str(), entry.path.as_path()))
            .collect::<Vec<_>>(),
        vec![("policy", Path::new("nodus-development/rules/policy.md"))]
    );
    assert_eq!(
        loaded
            .discovered
            .commands
            .iter()
            .map(|entry| (entry.id.as_str(), entry.path.as_path()))
            .collect::<Vec<_>>(),
        vec![("build", Path::new("nodus-development/commands/build.txt"))]
    );
}

#[test]
fn discovers_nested_rules_with_stable_ids() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join("rules/common/coding-style.md"),
        "# Common\n",
    );
    write_file(&temp.path().join("rules/swift/patterns.md"), "# Swift\n");

    let loaded = load_root_from_dir(temp.path()).unwrap();

    let ids = loaded
        .discovered
        .rules
        .iter()
        .map(|entry| entry.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["common__coding-style", "swift__patterns"]);
}

#[test]
fn ignores_readme_and_dotfiles_in_discovery_directories() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(&temp.path().join("skills/README.md"), "# Skills\n");
    write_file(&temp.path().join("skills/.DS_Store"), "binary\n");
    write_file(&temp.path().join("agents/.DS_Store"), "binary\n");
    write_file(&temp.path().join("agents/README.md"), "# Agents\n");
    write_file(&temp.path().join("agents/security.md"), "# Security\n");

    let loaded = load_root_from_dir(temp.path()).unwrap();

    assert_eq!(loaded.discovered.skills.len(), 1);
    assert_eq!(loaded.discovered.skills[0].id, "review");
    assert_eq!(loaded.discovered.agents.len(), 1);
    assert_eq!(loaded.discovered.agents[0].id, "security");
}

#[test]
fn rejects_duplicate_artifact_ids_across_content_roots() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
content_roots = ["nodus-development"]
"#,
    );
    write_skill(
        &temp.path().join("nodus-development/skills/review"),
        "Review Again",
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("duplicate skill id `review`"));
}

#[test]
fn init_scaffolds_a_minimal_manifest_and_example_skill() {
    let temp = TempDir::new().unwrap();
    let reporter = Reporter::silent();

    scaffold_init_in_dir(temp.path(), &reporter).unwrap();

    assert!(temp.path().join(MANIFEST_FILE).exists());
    assert!(temp.path().join("skills/example/SKILL.md").exists());
    let loaded = load_root_from_dir(temp.path()).unwrap();
    assert_eq!(loaded.discovered.skills[0].id, "example");
}

#[test]
fn serializes_dependencies_as_inline_tables() {
    let mut manifest = Manifest::default();
    manifest.dependencies.insert(
        "playbook_ios".into(),
        DependencySpec {
            github: Some("wenext-limited/playbook-ios".into()),
            url: None,
            path: None,
            subpath: None,
            tag: Some("v0.1.0".into()),
            branch: None,
            revision: None,
            version: Some(semver::VersionReq::parse("^0.1.0").unwrap()),
            components: Some(vec![
                DependencyComponent::Rules,
                DependencyComponent::Skills,
            ]),
            members: None,
            managed: None,
            enabled: true,
        },
    );

    let encoded = serialize_manifest(&manifest).unwrap();

    assert!(encoded.contains("[dependencies]"));
    assert!(encoded.contains("playbook_ios = {"));
    assert!(encoded.contains("github = \"wenext-limited/playbook-ios\""));
    assert!(encoded.contains("version = \"^0.1.0\""));
    assert!(encoded.contains("components = [\"skills\", \"rules\"]"));
    assert!(!encoded.contains("url = "));
}

#[test]
fn serializes_disabled_dependencies() {
    let mut manifest = Manifest::default();
    manifest.dependencies.insert(
        "playbook_ios".into(),
        DependencySpec {
            github: Some("wenext-limited/playbook-ios".into()),
            url: None,
            path: None,
            subpath: None,
            tag: Some("v0.1.0".into()),
            branch: None,
            revision: None,
            version: None,
            components: None,
            members: None,
            managed: None,
            enabled: false,
        },
    );

    let encoded = serialize_manifest(&manifest).unwrap();

    assert!(encoded.contains("enabled = false"));
}

#[test]
fn serializes_content_roots_and_publish_root() {
    let manifest = Manifest {
        content_roots: vec![
            PathBuf::from("nodus-development"),
            PathBuf::from("vendor/skills"),
        ],
        publish_root: true,
        ..Manifest::default()
    };

    let encoded = serialize_manifest(&manifest).unwrap();

    assert!(encoded.contains("content_roots = [\"nodus-development\", \"vendor/skills\"]"));
    assert!(encoded.contains("publish_root = true"));
}

#[test]
fn serializes_workspace_and_dependency_members() {
    let mut manifest = Manifest {
        workspace: Some(WorkspaceConfig {
            members: vec![
                PathBuf::from("plugins/axiom"),
                PathBuf::from("plugins/firebase"),
            ],
            package: BTreeMap::from([
                (
                    "axiom".into(),
                    WorkspaceMemberSpec {
                        path: PathBuf::from("plugins/axiom"),
                        name: Some("Axiom".into()),
                        codex: Some(WorkspaceMemberCodexSpec {
                            category: "Productivity".into(),
                            installation: "AVAILABLE".into(),
                            authentication: "ON_INSTALL".into(),
                        }),
                    },
                ),
                (
                    "firebase".into(),
                    WorkspaceMemberSpec {
                        path: PathBuf::from("plugins/firebase"),
                        name: Some("Firebase".into()),
                        codex: None,
                    },
                ),
            ]),
        }),
        ..Manifest::default()
    };
    manifest.dependencies.insert(
        "bundle".into(),
        DependencySpec {
            github: Some("acme/bundle".into()),
            url: None,
            path: None,
            subpath: None,
            tag: Some("v1.0.0".into()),
            branch: None,
            revision: None,
            version: None,
            components: None,
            members: Some(vec!["firebase".into(), "axiom".into()]),
            managed: None,
            enabled: true,
        },
    );

    let encoded = serialize_manifest(&manifest).unwrap();

    assert!(encoded.contains("[workspace]"));
    assert!(encoded.contains("members = [\"plugins/axiom\", \"plugins/firebase\"]"));
    assert!(encoded.contains("[workspace.package.axiom]"));
    assert!(encoded.contains("[workspace.package.axiom.codex]"));
    assert!(encoded.contains("bundle = { github = \"acme/bundle\", tag = \"v1.0.0\", members = [\"axiom\", \"firebase\"] }"));
}

#[test]
fn serializes_mcp_servers() {
    let manifest = Manifest {
        mcp_servers: BTreeMap::from([(
            "firebase".into(),
            McpServerConfig {
                transport_type: None,
                command: Some("npx".into()),
                url: None,
                args: vec!["-y".into(), "firebase-tools".into()],
                env: BTreeMap::from([(String::from("IS_FIREBASE_MCP"), String::from("true"))]),
                headers: BTreeMap::new(),
                cwd: Some(PathBuf::from(".")),
                enabled: true,
            },
        )]),
        ..Manifest::default()
    };

    let encoded = serialize_manifest(&manifest).unwrap();

    assert!(encoded.contains("[mcp_servers.firebase]"));
    assert!(encoded.contains("command = \"npx\""));
    assert!(encoded.contains("args = [\"-y\", \"firebase-tools\"]"));
    assert!(encoded.contains("cwd = \".\""));
    assert!(encoded.contains("[mcp_servers.firebase.env]"));
    assert!(encoded.contains("IS_FIREBASE_MCP = \"true\""));
}

#[test]
fn serializes_url_backed_disabled_mcp_servers() {
    let manifest = Manifest {
        mcp_servers: BTreeMap::from([(
            "figma".into(),
            McpServerConfig {
                transport_type: Some("http".into()),
                command: None,
                url: Some("http://127.0.0.1:3845/mcp".into()),
                args: Vec::new(),
                env: BTreeMap::new(),
                headers: BTreeMap::from([(
                    String::from("Authorization"),
                    String::from("Bearer token"),
                )]),
                cwd: None,
                enabled: false,
            },
        )]),
        ..Manifest::default()
    };

    let encoded = serialize_manifest(&manifest).unwrap();

    assert!(encoded.contains("[mcp_servers.figma]"));
    assert!(encoded.contains("type = \"http\""));
    assert!(encoded.contains("url = \"http://127.0.0.1:3845/mcp\""));
    assert!(encoded.contains("[mcp_servers.figma.headers]"));
    assert!(encoded.contains("Authorization = \"Bearer token\""));
    assert!(encoded.contains("enabled = false"));
    assert!(!encoded.contains("command = "));
}

#[test]
fn serializes_managed_dependencies_as_expanded_tables() {
    let mut manifest = Manifest::default();
    manifest.dependencies.insert(
        "superpowers".into(),
        DependencySpec {
            github: Some("org/superpowers".into()),
            url: None,
            path: None,
            subpath: None,
            tag: Some("v1.2.3".into()),
            branch: None,
            revision: None,
            version: None,
            components: None,
            members: None,
            managed: Some(vec![
                ManagedPathSpec {
                    source: PathBuf::from("prompts/review.md"),
                    target: PathBuf::from(".github/prompts/review.md"),
                },
                ManagedPathSpec {
                    source: PathBuf::from("templates"),
                    target: PathBuf::from("docs/templates"),
                },
            ]),
            enabled: true,
        },
    );

    let encoded = serialize_manifest(&manifest).unwrap();

    assert!(encoded.contains("[dependencies]"));
    assert!(encoded.contains("[dependencies.superpowers]"));
    assert!(encoded.contains("github = \"org/superpowers\""));
    assert!(encoded.contains("[[dependencies.superpowers.managed]]"));
    assert!(encoded.contains("source = \"prompts/review.md\""));
    assert!(encoded.contains("target = \".github/prompts/review.md\""));
    assert!(!encoded.contains("superpowers = {"));
}

#[test]
fn serializes_managed_exports_as_expanded_tables() {
    let manifest = Manifest {
        managed_exports: vec![
            ManagedExportSpec {
                source: PathBuf::from("learnings"),
                target: PathBuf::from("learnings"),
                placement: ManagedPlacement::Package,
            },
            ManagedExportSpec {
                source: PathBuf::from("prompts/review.md"),
                target: PathBuf::from("docs/review.md"),
                placement: ManagedPlacement::Project,
            },
        ],
        ..Manifest::default()
    };

    let encoded = serialize_manifest(&manifest).unwrap();

    assert!(encoded.contains("[[managed_exports]]"));
    assert!(encoded.contains("source = \"learnings\""));
    assert!(encoded.contains("target = \"learnings\""));
    assert!(encoded.contains("placement = \"project\""));
}

#[test]
fn serializes_dev_dependencies() {
    let mut manifest = Manifest::default();
    manifest.dev_dependencies.insert(
        "tooling".into(),
        DependencySpec {
            github: Some("org/tooling".into()),
            url: None,
            path: None,
            subpath: None,
            tag: Some("v1.2.3".into()),
            branch: None,
            revision: None,
            version: None,
            components: Some(vec![DependencyComponent::Skills]),
            members: None,
            managed: None,
            enabled: true,
        },
    );

    let encoded = serialize_manifest(&manifest).unwrap();

    assert!(encoded.contains("[dev-dependencies]"));
    assert!(encoded.contains("tooling = {"));
    assert!(encoded.contains("components = [\"skills\"]"));
}

#[test]
fn serializes_adapters_in_stable_sorted_order() {
    let manifest = Manifest {
        adapters: Some(AdapterConfig {
            enabled: vec![Adapter::OpenCode, Adapter::Claude, Adapter::Codex],
        }),
        ..Manifest::default()
    };

    let encoded = serialize_manifest(&manifest).unwrap();

    assert!(encoded.contains("[adapters]"));
    assert!(encoded.contains("enabled = [\"claude\", \"codex\", \"opencode\"]"));
}

#[test]
fn serializes_launch_hooks() {
    let manifest = Manifest {
        launch_hooks: Some(LaunchHookConfig {
            sync_on_startup: true,
        }),
        ..Manifest::default()
    };

    let encoded = serialize_manifest(&manifest).unwrap();

    assert!(encoded.contains("[launch_hooks]"));
    assert!(encoded.contains("sync_on_startup = true"));
}

#[test]
fn serializes_hooks() {
    let manifest = Manifest {
        hooks: vec![HookSpec {
            id: "bash-preflight".into(),
            event: HookEvent::PreToolUse,
            adapters: vec![Adapter::Claude, Adapter::Codex],
            matcher: Some(HookMatcher {
                sources: Vec::new(),
                tool_names: vec![HookTool::Bash],
            }),
            handler: HookHandler {
                handler_type: HookHandlerType::Command,
                command: "./scripts/preflight.sh".into(),
                cwd: HookWorkingDirectory::Session,
            },
            timeout_sec: Some(15),
            blocking: true,
        }],
        ..Manifest::default()
    };

    let encoded = serialize_manifest(&manifest).unwrap();

    assert!(encoded.contains("[[hooks]]"));
    assert!(encoded.contains("id = \"bash-preflight\""));
    assert!(encoded.contains("event = \"pre_tool_use\""));
    assert!(encoded.contains("adapters = [\"claude\", \"codex\"]"));
    assert!(encoded.contains("[hooks.matcher]"));
    assert!(encoded.contains("tool_names = [\"bash\"]"));
    assert!(encoded.contains("[hooks.handler]"));
    assert!(encoded.contains("command = \"./scripts/preflight.sh\""));
    assert!(encoded.contains("cwd = \"session\""));
}

#[test]
fn rejects_empty_adapter_selection() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = []
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("adapters.enabled"));
}

#[test]
fn rejects_duplicate_adapter_selection() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["codex", "codex"]
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("must not contain duplicates"));
}

#[test]
fn rejects_unknown_adapter_selection() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[adapters]
enabled = ["unknown"]
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("unknown variant"));
}

#[test]
fn rejects_disabled_launch_hook_config() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[launch_hooks]
sync_on_startup = false
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("launch_hooks.sync_on_startup"));
}

#[test]
fn rejects_hooks_in_dependency_manifest() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[[hooks]]
id = "bash-preflight"
event = "pre_tool_use"

[hooks.handler]
type = "command"
command = "./scripts/preflight.sh"
"#,
    );

    let error = load_dependency_from_dir(temp.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("root project manifests"));
}

#[test]
fn rejects_content_roots_with_parent_segments() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
content_roots = ["../shared"]
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("manifest field `content_roots` entry"));
}

#[test]
fn rejects_duplicate_content_roots_after_normalization() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    fs::create_dir_all(temp.path().join("nodus-development")).unwrap();
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
content_roots = ["nodus-development", "./nodus-development"]
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("must not contain duplicate paths"));
}

#[test]
fn rejects_missing_content_root_directory() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
content_roots = ["nodus-development"]
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("content_roots"));
    assert!(error.contains("nodus-development"));
}

#[test]
fn rejects_dependencies_with_multiple_git_sources() {
    let dependency = DependencySpec {
        github: Some("wenext-limited/playbook-ios".into()),
        url: Some("https://github.com/wenext-limited/playbook-ios".into()),
        path: None,
        subpath: None,
        tag: Some("v0.1.0".into()),
        branch: None,
        revision: None,
        version: None,
        components: None,
        members: None,
        managed: None,
        enabled: true,
    };

    let error = dependency.source_kind().unwrap_err().to_string();
    assert!(error.contains("must not declare both `github` and `url`"));
}

#[test]
fn parses_dependency_components() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", tag = "v0.1.0", components = ["skills", "agents"] }
"#,
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();
    let dependency = loaded.manifest.dependencies.get("playbook_ios").unwrap();
    assert_eq!(
        dependency.explicit_components_sorted().unwrap(),
        vec![DependencyComponent::Skills, DependencyComponent::Agents]
    );
}

#[test]
fn active_dependency_entries_skip_disabled_dependencies() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies]
enabled_dep = { github = "wenext-limited/playbook-ios", tag = "v0.1.0" }
disabled_dep = { github = "wenext-limited/playbook-ios", tag = "v0.1.0", enabled = false }
"#,
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();
    let active = loaded
        .manifest
        .active_dependency_entries()
        .into_iter()
        .map(|entry| entry.alias.to_string())
        .collect::<Vec<_>>();

    assert_eq!(active, vec!["enabled_dep"]);
}

#[test]
fn parses_mcp_servers() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[mcp_servers.firebase]
command = "npx"
args = ["-y", "firebase-tools"]
cwd = "."

[mcp_servers.firebase.env]
IS_FIREBASE_MCP = "true"
"#,
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();
    let server = loaded.manifest.mcp_servers.get("firebase").unwrap();
    assert!(server.transport_type.is_none());
    assert_eq!(server.command.as_deref(), Some("npx"));
    assert!(server.url.is_none());
    assert_eq!(server.args, vec!["-y", "firebase-tools"]);
    assert_eq!(server.cwd.as_deref(), Some(Path::new(".")));
    assert_eq!(
        server.env,
        BTreeMap::from([(String::from("IS_FIREBASE_MCP"), String::from("true"))])
    );
    assert!(server.headers.is_empty());
}

#[test]
fn parses_url_backed_mcp_servers() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[mcp_servers.figma]
type = "http"
url = "http://127.0.0.1:3845/mcp"
enabled = false

[mcp_servers.figma.headers]
Authorization = "Bearer token"
"#,
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();
    let server = loaded.manifest.mcp_servers.get("figma").unwrap();
    assert!(server.command.is_none());
    assert_eq!(server.transport_type.as_deref(), Some("http"));
    assert_eq!(server.url.as_deref(), Some("http://127.0.0.1:3845/mcp"));
    assert_eq!(
        server.headers,
        BTreeMap::from([(String::from("Authorization"), String::from("Bearer token"))])
    );
    assert!(!server.enabled);
}

#[test]
fn rejects_git_dependency_version_with_tag() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", tag = "v0.1.0", version = "^1.0.0" }
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("must not declare both `version` and `tag`"));
}

#[test]
fn accepts_git_dependency_version_requirement_without_explicit_ref() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", version = "^1.0.0" }
"#,
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();
    let dependency = loaded.manifest.dependencies.get("playbook_ios").unwrap();
    assert_eq!(dependency.version.as_ref().unwrap().to_string(), "^1.0.0");
}

#[test]
fn parses_managed_dependency_tables() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies.superpowers]
github = "org/superpowers"
tag = "v1.2.3"

[[dependencies.superpowers.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"

[[dependencies.superpowers.managed]]
source = "templates"
target = "docs/templates"
"#,
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();
    let dependency = loaded.manifest.dependencies.get("superpowers").unwrap();
    assert_eq!(
        dependency.resolved_git_url().unwrap(),
        "https://github.com/org/superpowers"
    );
    assert_eq!(dependency.managed_mappings().len(), 2);
    assert_eq!(
        dependency.managed_mappings()[0],
        ManagedPathSpec {
            source: PathBuf::from("prompts/review.md"),
            target: PathBuf::from(".github/prompts/review.md"),
        }
    );
    assert_eq!(
        dependency.managed_mappings()[1],
        ManagedPathSpec {
            source: PathBuf::from("templates"),
            target: PathBuf::from("docs/templates"),
        }
    );
}

#[test]
fn parses_managed_export_tables() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[[managed_exports]]
source = "learnings"
target = "learnings"

[[managed_exports]]
source = "prompts/review.md"
target = "docs/review.md"
placement = "project"
"#,
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();
    assert_eq!(
        loaded.manifest.managed_exports,
        vec![
            ManagedExportSpec {
                source: PathBuf::from("learnings"),
                target: PathBuf::from("learnings"),
                placement: ManagedPlacement::Package,
            },
            ManagedExportSpec {
                source: PathBuf::from("prompts/review.md"),
                target: PathBuf::from("docs/review.md"),
                placement: ManagedPlacement::Project,
            }
        ]
    );
}

#[test]
fn managed_exports_do_not_emit_unsupported_field_warnings() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[[managed_exports]]
source = "learnings"
target = "learnings"
"#,
    );

    let loaded = load_dependency_from_dir(temp.path()).unwrap();
    assert!(loaded.warnings.is_empty());
}

#[test]
fn rejects_duplicate_aliases_across_dependency_sections() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", tag = "v0.1.0" }

[dev-dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", tag = "v0.1.0" }
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("more than one dependency section"));
}

#[test]
fn parses_dev_dependency_tables() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dev-dependencies.tooling]
github = "org/tooling"
tag = "v1.2.3"
"#,
    );

    let loaded = load_root_from_dir(temp.path()).unwrap();
    let dependency = loaded.manifest.dev_dependencies.get("tooling").unwrap();
    assert_eq!(
        dependency.resolved_git_url().unwrap(),
        "https://github.com/org/tooling"
    );
}

#[test]
fn rejects_empty_dependency_components() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", tag = "v0.1.0", components = [] }
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("field `components` must not be empty"));
}

#[test]
fn rejects_empty_mcp_server_command() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[mcp_servers.firebase]
command = ""
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("mcp_servers.firebase.command"));
}

#[test]
fn rejects_mcp_server_with_both_command_and_url() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[mcp_servers.firebase]
command = "npx"
url = "http://127.0.0.1:3845/mcp"
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("must not declare both `command` and `url`"));
}

#[test]
fn rejects_url_backed_mcp_server_with_stdio_fields() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[mcp_servers.firebase]
url = "http://127.0.0.1:3845/mcp"
args = ["--verbose"]
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("must not combine `url` with `args`, `env`, or `cwd`"));
}

#[test]
fn rejects_duplicate_dependency_components() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", tag = "v0.1.0", components = ["skills", "skills"] }
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("must not contain duplicates"));
}

#[test]
fn rejects_empty_dependency_managed_paths() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies.superpowers]
github = "org/superpowers"
tag = "v1.2.3"
managed = []
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("field `managed` must not be empty"));
}

#[test]
fn rejects_duplicate_dependency_managed_pairs() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies.superpowers]
github = "org/superpowers"
tag = "v1.2.3"

[[dependencies.superpowers.managed]]
source = "prompts/review.md"
target = "docs/review.md"

[[dependencies.superpowers.managed]]
source = "./prompts/review.md"
target = "./docs/review.md"
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("must not contain duplicate source/target pairs"));
}

#[test]
fn rejects_dependency_managed_paths_with_parent_segments() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies.superpowers]
github = "org/superpowers"
tag = "v1.2.3"

[[dependencies.superpowers.managed]]
source = "../prompts/review.md"
target = "docs/review.md"
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("managed.source"));
}

#[test]
fn rejects_duplicate_managed_exports() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[[managed_exports]]
source = "learnings"
target = "learnings"

[[managed_exports]]
source = "./learnings"
target = "./learnings"
"#,
    );

    let error = load_dependency_from_dir(temp.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("managed_exports"));
    assert!(error.contains("duplicate"));
}

#[test]
fn rejects_managed_exports_with_parent_segments() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[[managed_exports]]
source = "../learnings"
target = "learnings"
"#,
    );

    let error = load_dependency_from_dir(temp.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("managed_exports.source"));
}

#[test]
fn rejects_unknown_dependency_component() {
    let temp = TempDir::new().unwrap();
    write_valid_skill(temp.path());
    write_file(
        &temp.path().join(MANIFEST_FILE),
        r#"
[dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", tag = "v0.1.0", components = ["widgets"] }
"#,
    );

    let error = load_root_from_dir(temp.path()).unwrap_err().to_string();
    assert!(error.contains("unknown variant"));
}
