use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::{Arc, Mutex};

use clap::Parser;
use clap_complete::Shell;
use serde_json::Value;
use tempfile::TempDir;
use walkdir::WalkDir;

use super::args::{Cli, Command};
use super::output::should_auto_check_for_updates;
use super::router::run_command_in_dir;
use crate::adapters::Adapter;
use crate::report::{ColorMode, Reporter};
use crate::resolver;

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

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn write_skill(path: &Path, name: &str) {
    write_file(
        &path.join("SKILL.md"),
        &format!("---\nname: {name}\ndescription: Example skill.\n---\n# {name}\n"),
    );
}

fn init_git_repo(path: &Path) {
    let run = |args: &[&str]| {
        let output = ProcessCommand::new("git")
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
    run(&["config", "core.autocrlf", "false"]);
    write_file(&path.join(".gitattributes"), "* text eol=lf\n");
    run(&["add", "."]);
    run(&["commit", "-m", "initial"]);
}

fn create_git_dependency() -> (TempDir, String) {
    let repo = TempDir::new().unwrap();
    write_skill(&repo.path().join("skills/review"), "Review");
    init_git_repo(repo.path());

    let output = ProcessCommand::new("git")
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

fn create_workspace_dependency() -> (TempDir, String) {
    let repo = TempDir::new().unwrap();
    write_file(
        &repo.path().join("nodus.toml"),
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
    write_skill(&repo.path().join("plugins/axiom/skills/review"), "Review");
    write_skill(
        &repo.path().join("plugins/firebase/skills/checks"),
        "Checks",
    );
    init_git_repo(repo.path());

    let output = ProcessCommand::new("git")
        .args(["tag", "v0.2.0"])
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

fn run_command_output(command: Command, cwd: &Path, cache_root: &Path) -> String {
    let buffer = SharedBuffer::default();
    let reporter = Reporter::sink(ColorMode::Never, buffer.clone());

    run_command_in_dir(command, cwd, cache_root, &reporter).unwrap();

    buffer.contents()
}

fn run_command_streams(command: Command, cwd: &Path, cache_root: &Path) -> (String, String) {
    let stdout = SharedBuffer::default();
    let stderr = SharedBuffer::default();
    let reporter = Reporter::sink_split(ColorMode::Never, stdout.clone(), stderr.clone());

    run_command_in_dir(command, cwd, cache_root, &reporter).unwrap();

    (stdout.contents(), stderr.contents())
}

fn read_optional(path: &Path) -> Option<Vec<u8>> {
    fs::read(path).ok()
}

fn first_file_under(root: &Path, file_name: &str) -> PathBuf {
    WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .find(|entry| entry.file_type().is_file() && entry.file_name() == file_name)
        .unwrap()
        .path()
        .to_path_buf()
}

#[test]
fn parses_remove_subcommand() {
    let cli = Cli::try_parse_from(["nodus", "remove", "playbook_ios"]).unwrap();

    match cli.command {
        Command::Remove { package, .. } => assert_eq!(package, "playbook_ios"),
        other => panic!("expected remove command, got {other:?}"),
    }
}

#[test]
fn parses_global_add_and_remove_flags() {
    let add = Cli::try_parse_from(["nodus", "add", "example/repo", "--global"]).unwrap();
    let remove = Cli::try_parse_from(["nodus", "remove", "example/repo", "--global"]).unwrap();

    assert!(matches!(add.command, Command::Add { global: true, .. }));
    assert!(matches!(
        remove.command,
        Command::Remove { global: true, .. }
    ));
}

#[test]
fn parses_list_subcommand() {
    let cli = Cli::try_parse_from(["nodus", "list"]).unwrap();

    match cli.command {
        Command::List { json } => assert!(!json),
        other => panic!("expected list command, got {other:?}"),
    }
}

#[test]
fn rejects_uninstall_subcommand() {
    let error = Cli::try_parse_from(["nodus", "uninstall", "playbook_ios"]).unwrap_err();

    assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
}

#[test]
fn parses_info_subcommand() {
    let cli =
        Cli::try_parse_from(["nodus", "info", "obra/superpowers", "--branch", "main"]).unwrap();

    match cli.command {
        Command::Info {
            package,
            tag,
            branch,
            json,
        } => {
            assert_eq!(package, "obra/superpowers");
            assert_eq!(tag, None);
            assert_eq!(branch.as_deref(), Some("main"));
            assert!(!json);
        }
        other => panic!("expected info command, got {other:?}"),
    }
}

#[test]
fn parses_add_version_selector() {
    let cli =
        Cli::try_parse_from(["nodus", "add", "obra/superpowers", "--version", "^1.2.0"]).unwrap();

    match cli.command {
        Command::Add { url, version, .. } => {
            assert_eq!(url, "obra/superpowers");
            assert_eq!(version.as_deref(), Some("^1.2.0"));
        }
        other => panic!("expected add command, got {other:?}"),
    }
}

#[test]
fn parses_json_flags_for_read_only_commands() {
    let info = Cli::try_parse_from(["nodus", "info", ".", "--json"]).unwrap();
    let outdated = Cli::try_parse_from(["nodus", "outdated", "--json"]).unwrap();
    let doctor = Cli::try_parse_from(["nodus", "doctor", "--json"]).unwrap();

    assert!(matches!(info.command, Command::Info { json: true, .. }));
    assert!(matches!(outdated.command, Command::Outdated { json: true }));
    assert!(matches!(doctor.command, Command::Doctor { json: true }));
}

#[test]
fn parses_review_subcommand() {
    let cli = Cli::try_parse_from([
        "nodus",
        "review",
        "obra/superpowers",
        "--provider",
        "anthropic",
        "--model",
        "claude-sonnet",
    ])
    .unwrap();

    match cli.command {
        Command::Review {
            package,
            tag,
            branch,
            provider,
            model,
        } => {
            assert_eq!(package, "obra/superpowers");
            assert_eq!(tag, None);
            assert_eq!(branch, None);
            assert_eq!(provider, crate::review::ReviewProvider::Anthropic);
            assert_eq!(model.as_deref(), Some("claude-sonnet"));
        }
        other => panic!("expected review command, got {other:?}"),
    }
}

#[test]
fn parses_outdated_subcommand() {
    let cli = Cli::try_parse_from(["nodus", "outdated"]).unwrap();

    match cli.command {
        Command::Outdated { json } => assert!(!json),
        other => panic!("expected outdated command, got {other:?}"),
    }
}

#[test]
fn auto_update_checks_only_run_for_interactive_human_output_commands() {
    assert!(should_auto_check_for_updates(
        &Command::List { json: false },
        true,
        false
    ));
    assert!(!should_auto_check_for_updates(
        &Command::List { json: true },
        true,
        false
    ));
    assert!(!should_auto_check_for_updates(
        &Command::Completion { shell: Shell::Bash },
        true,
        false
    ));
    assert!(!should_auto_check_for_updates(
        &Command::Upgrade { check: false },
        true,
        false
    ));
    assert!(!should_auto_check_for_updates(
        &Command::List { json: false },
        false,
        false
    ));
    assert!(!should_auto_check_for_updates(
        &Command::List { json: false },
        true,
        true
    ));
}

#[test]
fn parses_relay_subcommand() {
    let cli = Cli::try_parse_from([
        "nodus",
        "relay",
        "wenext-limited/playbook-ios",
        "--repo-path",
        "/tmp/playbook-ios",
        "--watch",
    ])
    .unwrap();

    match cli.command {
        Command::Relay {
            packages,
            repo_path,
            via,
            watch,
            ..
        } => {
            assert_eq!(packages, ["wenext-limited/playbook-ios"]);
            assert_eq!(repo_path.as_deref(), Some(Path::new("/tmp/playbook-ios")));
            assert_eq!(via, None);
            assert!(watch);
        }
        other => panic!("expected relay command, got {other:?}"),
    }
}

#[test]
fn parses_multiple_relay_targets() {
    let cli = Cli::try_parse_from(["nodus", "relay", "example/one", "example/two"]).unwrap();

    match cli.command {
        Command::Relay { packages, .. } => {
            assert_eq!(packages, ["example/one", "example/two"]);
        }
        other => panic!("expected relay command, got {other:?}"),
    }
}

#[test]
fn parses_relay_via_aliases() {
    let via = Cli::try_parse_from(["nodus", "relay", "example/repo", "--via", "claude"]).unwrap();
    let relay_via =
        Cli::try_parse_from(["nodus", "relay", "example/repo", "--relay-via", "codex"]).unwrap();
    let prefer =
        Cli::try_parse_from(["nodus", "relay", "example/repo", "--prefer", "opencode"]).unwrap();

    assert!(matches!(
        via.command,
        Command::Relay {
            via: Some(Adapter::Claude),
            ..
        }
    ));
    assert!(matches!(
        relay_via.command,
        Command::Relay {
            via: Some(Adapter::Codex),
            ..
        }
    ));
    assert!(matches!(
        prefer.command,
        Command::Relay {
            via: Some(Adapter::OpenCode),
            ..
        }
    ));
}

#[test]
fn parses_relay_create_missing_flag() {
    let cli = Cli::try_parse_from(["nodus", "relay", "example/repo", "--create-missing"]).unwrap();

    assert!(matches!(
        cli.command,
        Command::Relay {
            create_missing: true,
            ..
        }
    ));
}

#[test]
fn parses_update_subcommand() {
    let cli = Cli::try_parse_from(["nodus", "update", "--allow-high-sensitivity"]).unwrap();

    match cli.command {
        Command::Update {
            allow_high_sensitivity,
            ..
        } => assert!(allow_high_sensitivity),
        other => panic!("expected update command, got {other:?}"),
    }
}

#[test]
fn parses_upgrade_subcommand() {
    let cli = Cli::try_parse_from(["nodus", "upgrade"]).unwrap();

    assert!(matches!(cli.command, Command::Upgrade { check: false }));
}

#[test]
fn parses_upgrade_check_flag_and_self_update_alias() {
    let check = Cli::try_parse_from(["nodus", "upgrade", "--check"]).unwrap();
    let alias = Cli::try_parse_from(["nodus", "self-update"]).unwrap();

    assert!(matches!(check.command, Command::Upgrade { check: true }));
    assert!(matches!(alias.command, Command::Upgrade { check: false }));
}

#[test]
fn parses_dry_run_flags_for_mutating_commands() {
    let add = Cli::try_parse_from(["nodus", "add", "example/repo", "--dry-run"]).unwrap();
    let remove = Cli::try_parse_from(["nodus", "remove", "example/repo", "--dry-run"]).unwrap();
    let update = Cli::try_parse_from(["nodus", "update", "--dry-run"]).unwrap();
    let relay = Cli::try_parse_from(["nodus", "relay", "example/repo", "--dry-run"]).unwrap();
    let init = Cli::try_parse_from(["nodus", "init", "--dry-run"]).unwrap();
    let sync = Cli::try_parse_from(["nodus", "sync", "--dry-run"]).unwrap();

    assert!(matches!(add.command, Command::Add { dry_run: true, .. }));
    assert!(matches!(
        remove.command,
        Command::Remove { dry_run: true, .. }
    ));
    assert!(matches!(
        update.command,
        Command::Update { dry_run: true, .. }
    ));
    assert!(matches!(
        relay.command,
        Command::Relay { dry_run: true, .. }
    ));
    assert!(matches!(init.command, Command::Init { dry_run: true }));
    assert!(matches!(sync.command, Command::Sync { dry_run: true, .. }));
}

#[test]
fn parses_sync_force_flag() {
    let cli = Cli::try_parse_from(["nodus", "sync", "--force"]).unwrap();

    match cli.command {
        Command::Sync { force, .. } => assert!(force),
        other => panic!("expected sync command, got {other:?}"),
    }
}

#[test]
fn rejects_relay_watch_with_dry_run() {
    let error = Cli::try_parse_from(["nodus", "relay", "example/repo", "--watch", "--dry-run"])
        .unwrap_err();

    assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
}

#[test]
fn root_help_describes_commands() {
    let help = <Cli as clap::CommandFactory>::command()
        .render_long_help()
        .to_string();

    assert!(help.contains("Nodus installs agent packages from GitHub, Git URLs, or local paths"));
    assert!(help.contains("For most repos, the normal flow is:"));
    assert!(help.contains("nodus add <package> --adapter <adapter>"));
    assert!(help.contains("nodus doctor"));
    assert!(help.contains("Add a dependency and run sync"));
    assert!(help.contains("List configured dependencies and any locked metadata"));
    assert!(help.contains("Display resolved package metadata"));
    assert!(help.contains("Check configured dependencies for newer tags or branch head changes"));
    assert!(help.contains("Update configured dependencies and resync managed outputs"));
    assert!(
        help.contains(
            "Check for or install a newer nodus CLI when the install method is supported"
        )
    );
    assert!(help.contains("Generate shell completion scripts"));
    assert!(
        help.contains("Use an AI review agent to assess whether a package graph looks safe to use")
    );
    assert!(help.contains("Validate lockfile, shared store, and managed output consistency"));
    assert!(help.contains("Project-scoped installs are the default"));
    assert!(help.contains("Use `nodus <command> --help` for examples and flag details"));
}

#[test]
fn add_help_describes_arguments() {
    let mut root = <Cli as clap::CommandFactory>::command();
    let help = root
        .find_subcommand_mut("add")
        .unwrap()
        .render_long_help()
        .to_string();

    assert!(help.contains("Add a package to the current repo and immediately sync"));
    assert!(help.contains("<PACKAGE>"));
    assert!(help.contains("Git URL, local path, or GitHub shortcut like owner/repo"));
    assert!(help.contains("Record the dependency under `[dev-dependencies]`"));
    assert!(help.contains("Pin a specific Git tag instead of resolving the latest tag"));
    assert!(help.contains("Track a specific Git branch instead of resolving the latest tag"));
    assert!(help.contains("Pin a specific Git commit revision"));
    assert!(help.contains("Install into user-level global state"));
    assert!(help.contains("Select one or more adapters to persist for this install target"));
    assert!(help.contains("Select which dependency components to install from the package"));
    assert!(help.contains("Persist project startup hooks"));
    assert!(help.contains("By default Nodus installs the whole package"));
    assert!(help.contains("nodus add nodus-rs/nodus --adapter codex"));
    assert!(help.contains("After a project-scoped install, run `nodus doctor`"));
}

#[test]
fn mutating_subcommand_help_mentions_dry_run() {
    let mut root = <Cli as clap::CommandFactory>::command();
    for name in ["add", "remove", "update", "relay", "init", "sync"] {
        let help = root
            .find_subcommand_mut(name)
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(help.contains("--dry-run"), "{name} help missing dry-run");
        assert!(
            help.contains("may still populate the shared store"),
            "{name} help missing shared-store explanation"
        );
    }
}

#[test]
fn sync_help_describes_force() {
    let mut root = <Cli as clap::CommandFactory>::command();
    let help = root
        .find_subcommand_mut("sync")
        .unwrap()
        .render_long_help()
        .to_string();

    assert!(help.contains("Resolve the dependencies already declared in `nodus.toml`"));
    assert!(help.contains("--force"));
    assert!(help.contains("Overwrite unmanaged files"));
    assert!(help.contains("nodus sync --locked"));
    assert!(help.contains("Use `--locked` when the lockfile must stay unchanged"));
}

#[test]
fn review_help_describes_arguments() {
    let mut root = <Cli as clap::CommandFactory>::command();
    let help = root
        .find_subcommand_mut("review")
        .unwrap()
        .render_long_help()
        .to_string();

    assert!(help.contains(
        "Dependency alias, local package path, Git URL, or GitHub shortcut like owner/repo"
    ));
    assert!(help.contains("LLM provider to use for the safety review"));
    assert!(help.contains("Specific model id to use"));
}

#[test]
fn read_only_help_mentions_json() {
    let mut root = <Cli as clap::CommandFactory>::command();
    for name in ["list", "info", "outdated", "doctor"] {
        let help = root
            .find_subcommand_mut(name)
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(help.contains("--json"), "{name} help missing --json");
        assert!(
            help.contains("Emit machine-readable JSON"),
            "{name} help missing JSON description"
        );
    }
}

#[test]
fn doctor_help_describes_when_to_run_it() {
    let mut root = <Cli as clap::CommandFactory>::command();
    let help = root
        .find_subcommand_mut("doctor")
        .unwrap()
        .render_long_help()
        .to_string();

    assert!(help.contains("Validate that `nodus.toml`, `nodus.lock`, the shared store"));
    assert!(
        help.contains(
            "Run this after `nodus add`, `nodus sync`, `nodus update`, or `nodus remove`"
        )
    );
    assert!(help.contains("nodus doctor --json"));
}

#[test]
fn info_help_describes_read_only_inspection_flow() {
    let mut root = <Cli as clap::CommandFactory>::command();
    let help = root
        .find_subcommand_mut("info")
        .unwrap()
        .render_long_help()
        .to_string();

    assert!(help.contains("Inspect a package without changing the current repo"));
    assert!(help.contains("Use this when you want to see discovered skills"));
    assert!(help.contains("nodus info ./vendor/playbook"));
}

#[test]
fn update_help_distinguishes_itself_from_sync() {
    let mut root = <Cli as clap::CommandFactory>::command();
    let help = root
        .find_subcommand_mut("update")
        .unwrap()
        .render_long_help()
        .to_string();

    assert!(help.contains("Resolve newer allowed versions for configured dependencies"));
    assert!(help.contains("Use `nodus update` when you want newer package revisions"));
    assert!(help.contains("Use `nodus sync` when you only want to rebuild"));
}

#[test]
fn list_command_emits_human_readable_dependencies() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write_file(
        &temp.path().join("nodus.toml"),
        r#"
[dependencies]
local_playbook = { path = "vendor/playbook", components = ["skills"] }
"#,
    );
    write_skill(&temp.path().join("vendor/playbook/skills/review"), "Review");

    let output = run_command_output(Command::List { json: false }, temp.path(), cache.path());

    assert!(output.contains("local_playbook"));
    assert!(output.contains("path vendor/playbook"));
    assert!(output.contains("components skills"));
    assert!(output.contains("unlocked"));
    assert!(!output.contains("Finished"));
}

#[test]
fn list_command_writes_results_to_stdout() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write_file(
        &temp.path().join("nodus.toml"),
        r#"
[dependencies]
local_playbook = { path = "vendor/playbook", components = ["skills"] }
"#,
    );
    write_skill(&temp.path().join("vendor/playbook/skills/review"), "Review");

    let (stdout, stderr) =
        run_command_streams(Command::List { json: false }, temp.path(), cache.path());

    assert!(stdout.contains("local_playbook"));
    assert!(stdout.contains("path vendor/playbook"));
    assert!(stderr.is_empty());
}

#[test]
fn list_command_writes_empty_state_note_to_stderr() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();

    let (stdout, stderr) =
        run_command_streams(Command::List { json: false }, temp.path(), cache.path());

    assert!(stdout.is_empty());
    assert_eq!(stderr, "note: no dependencies configured\n");
}

#[test]
fn list_command_emits_json_with_locked_metadata() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let (_repo, url) = create_git_dependency();

    run_command_in_dir(
        Command::Add {
            url,
            global: false,
            dev: false,
            tag: Some("v0.1.0".into()),
            branch: None,
            version: None,
            revision: None,
            adapter: vec![Adapter::Codex],
            component: vec![],
            sync_on_launch: false,
            dry_run: false,
        },
        temp.path(),
        cache.path(),
        &Reporter::silent(),
    )
    .unwrap();
    let alias = crate::manifest::load_root_from_dir(temp.path())
        .unwrap()
        .manifest
        .dependencies
        .keys()
        .next()
        .unwrap()
        .clone();

    let output = run_command_output(Command::List { json: true }, temp.path(), cache.path());

    let json: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(json["dependencies"][0]["alias"], alias);
    assert_eq!(json["dependencies"][0]["source"]["kind"], "git");
    assert_eq!(json["dependencies"][0]["requested_ref"]["kind"], "tag");
    assert_eq!(json["dependencies"][0]["requested_ref"]["value"], "v0.1.0");
    assert_eq!(json["dependencies"][0]["locked"]["version_tag"], "v0.1.0");
    assert!(json["dependencies"][0]["locked"]["rev"].as_str().is_some());
    assert!(!output.contains("Finished"));
    assert!(!output.contains("Checking"));
}

#[test]
fn list_json_command_writes_only_json_to_stdout() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write_file(
        &temp.path().join("nodus.toml"),
        r#"
[dependencies]
local_playbook = { path = "vendor/playbook", components = ["skills"] }
"#,
    );
    write_skill(&temp.path().join("vendor/playbook/skills/review"), "Review");

    let (stdout, stderr) =
        run_command_streams(Command::List { json: true }, temp.path(), cache.path());

    let json: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["dependencies"][0]["alias"], "local_playbook");
    assert!(stderr.is_empty());
}

#[test]
fn list_command_labels_dev_dependencies() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write_file(
        &temp.path().join("nodus.toml"),
        r#"
[dev-dependencies]
tooling = { path = "vendor/tooling" }
"#,
    );
    write_skill(
        &temp.path().join("vendor/tooling/skills/tooling"),
        "Tooling",
    );

    let output = run_command_output(Command::List { json: false }, temp.path(), cache.path());
    assert!(output.contains("tooling [dev]"));

    let output = run_command_output(Command::List { json: true }, temp.path(), cache.path());
    let json: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(json["dependencies"][0]["alias"], "tooling");
    assert_eq!(json["dependencies"][0]["kind"], "dev_dependency");
}

#[test]
fn list_command_marks_disabled_dependencies() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write_file(
        &temp.path().join("nodus.toml"),
        r#"
[dependencies]
tooling = { path = "vendor/tooling", enabled = false }
"#,
    );
    write_skill(
        &temp.path().join("vendor/tooling/skills/tooling"),
        "Tooling",
    );

    let output = run_command_output(Command::List { json: false }, temp.path(), cache.path());
    assert!(output.contains("disabled"));

    let output = run_command_output(Command::List { json: true }, temp.path(), cache.path());
    let json: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(json["dependencies"][0]["alias"], "tooling");
    assert_eq!(json["dependencies"][0]["enabled"], false);
}

#[test]
fn list_command_emits_version_requested_ref_for_semver_dependencies() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    write_skill(&repo.path().join("skills/review"), "Review");
    init_git_repo(repo.path());
    let output = ProcessCommand::new("git")
        .args(["tag", "v1.0.0"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    run_command_in_dir(
        Command::Add {
            url: repo.path().to_string_lossy().to_string(),
            global: false,
            dev: false,
            tag: None,
            branch: None,
            version: Some("^1.0.0".into()),
            revision: None,
            adapter: vec![Adapter::Codex],
            component: vec![],
            sync_on_launch: false,
            dry_run: false,
        },
        temp.path(),
        cache.path(),
        &Reporter::silent(),
    )
    .unwrap();

    let output = run_command_output(Command::List { json: true }, temp.path(), cache.path());
    let json: Value = serde_json::from_str(&output).unwrap();

    assert_eq!(json["dependencies"][0]["requested_ref"]["kind"], "version");
    assert_eq!(json["dependencies"][0]["requested_ref"]["value"], "^1.0.0");
}

#[test]
fn completion_help_describes_shell_argument() {
    let mut root = <Cli as clap::CommandFactory>::command();
    let help = root
        .find_subcommand_mut("completion")
        .unwrap()
        .render_long_help()
        .to_string();

    assert!(help.contains("Generate shell completion scripts"));
    assert!(help.contains("Shell to generate completions for"));
    assert!(help.contains("[possible values: bash, elvish, fish, powershell, zsh]"));
}

#[test]
fn parses_completion_shell_argument() {
    let cli = Cli::try_parse_from(["nodus", "completion", "zsh"]).unwrap();

    match cli.command {
        Command::Completion { shell } => assert_eq!(shell, Shell::Zsh),
        other => panic!("expected completion command, got {other:?}"),
    }
}

#[test]
fn parses_repeatable_add_adapter_flags() {
    let cli = Cli::try_parse_from([
        "nodus",
        "add",
        "example/repo",
        "--adapter",
        "codex",
        "--adapter",
        "opencode",
    ])
    .unwrap();

    match cli.command {
        Command::Add { adapter, .. } => {
            assert_eq!(adapter, vec![Adapter::Codex, Adapter::OpenCode]);
        }
        other => panic!("expected add command, got {other:?}"),
    }
}

#[test]
fn parses_add_dev_flag() {
    let cli = Cli::try_parse_from(["nodus", "add", "example/repo", "--dev"]).unwrap();

    match cli.command {
        Command::Add { dev, .. } => assert!(dev),
        other => panic!("expected add command, got {other:?}"),
    }
}

#[test]
fn parses_add_branch_and_revision_flags() {
    let branch = Cli::try_parse_from(["nodus", "add", "example/repo", "--branch", "main"]).unwrap();
    let revision =
        Cli::try_parse_from(["nodus", "add", "example/repo", "--revision", "abc1234"]).unwrap();

    match branch.command {
        Command::Add {
            tag,
            branch,
            revision,
            ..
        } => {
            assert_eq!(tag, None);
            assert_eq!(branch.as_deref(), Some("main"));
            assert_eq!(revision, None);
        }
        other => panic!("expected add command, got {other:?}"),
    }

    match revision.command {
        Command::Add {
            tag,
            branch,
            revision,
            ..
        } => {
            assert_eq!(tag, None);
            assert_eq!(branch, None);
            assert_eq!(revision.as_deref(), Some("abc1234"));
        }
        other => panic!("expected add command, got {other:?}"),
    }
}

#[test]
fn parses_sync_on_launch_flags() {
    let add = Cli::try_parse_from(["nodus", "add", "example/repo", "--sync-on-launch"]).unwrap();
    let sync = Cli::try_parse_from(["nodus", "sync", "--sync-on-launch"]).unwrap();

    match add.command {
        Command::Add { sync_on_launch, .. } => assert!(sync_on_launch),
        other => panic!("expected add command, got {other:?}"),
    }

    match sync.command {
        Command::Sync { sync_on_launch, .. } => assert!(sync_on_launch),
        other => panic!("expected sync command, got {other:?}"),
    }
}

#[test]
fn parses_sync_frozen_flag() {
    let cli = Cli::try_parse_from(["nodus", "sync", "--frozen"]).unwrap();

    match cli.command {
        Command::Sync { frozen, locked, .. } => {
            assert!(frozen);
            assert!(!locked);
        }
        other => panic!("expected sync command, got {other:?}"),
    }
}

#[test]
fn rejects_sync_locked_with_frozen() {
    let error = Cli::try_parse_from(["nodus", "sync", "--locked", "--frozen"]).unwrap_err();

    assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
}

#[test]
fn parses_repeatable_add_component_flags() {
    let cli = Cli::try_parse_from([
        "nodus",
        "add",
        "example/repo",
        "--component",
        "skills",
        "--component",
        "agents",
    ])
    .unwrap();

    match cli.command {
        Command::Add { component, .. } => {
            assert_eq!(
                component,
                vec![
                    crate::manifest::DependencyComponent::Skills,
                    crate::manifest::DependencyComponent::Agents
                ]
            );
        }
        other => panic!("expected add command, got {other:?}"),
    }
}

#[test]
fn init_command_emits_creating_and_finished_lines() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();

    let output = run_command_output(Command::Init { dry_run: false }, temp.path(), cache.path());

    assert!(output.contains("Creating"));
    assert!(output.contains("nodus.toml"));
    assert!(output.contains("skills/example/SKILL.md"));
    assert!(output.contains("Finished"));
}

#[test]
fn init_command_writes_progress_to_stderr_and_finish_to_stdout() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();

    let (stdout, stderr) =
        run_command_streams(Command::Init { dry_run: false }, temp.path(), cache.path());

    assert!(stdout.contains("Finished created"));
    assert!(stderr.contains("Creating"));
}

#[test]
fn init_dry_run_previews_without_writing() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();

    let output = run_command_output(Command::Init { dry_run: true }, temp.path(), cache.path());

    assert!(output.contains("would create"));
    assert!(output.contains("dry run: would create"));
    assert!(!temp.path().join("nodus.toml").exists());
    assert!(!temp.path().join("skills/example/SKILL.md").exists());
}

#[test]
fn info_command_emits_package_metadata_lines() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write_file(
        &temp.path().join("nodus.toml"),
        r#"
name = "playbook-ios"
version = "0.1.0"
"#,
    );
    write_skill(&temp.path().join("skills/review"), "Review");

    let output = run_command_output(
        Command::Info {
            package: ".".into(),
            tag: None,
            branch: None,
            json: false,
        },
        temp.path(),
        cache.path(),
    );

    assert!(output.contains("playbook-ios"));
    assert!(output.contains("version: 0.1.0"));
    assert!(output.contains("alias: playbook_ios"));
    assert!(output.contains("artifacts:"));
    assert!(output.contains("skills = [review]"));
    assert!(!output.contains("Finished"));
}

#[test]
fn info_command_writes_metadata_to_stdout() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write_file(
        &temp.path().join("nodus.toml"),
        r#"
name = "playbook-ios"
version = "0.1.0"
"#,
    );
    write_skill(&temp.path().join("skills/review"), "Review");

    let (stdout, stderr) = run_command_streams(
        Command::Info {
            package: ".".into(),
            tag: None,
            branch: None,
            json: false,
        },
        temp.path(),
        cache.path(),
    );

    assert!(stdout.contains("playbook-ios"));
    assert!(stdout.contains("version: 0.1.0"));
    assert!(stderr.is_empty());
}

#[test]
fn info_command_emits_json_without_status_lines() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write_file(
        &temp.path().join("nodus.toml"),
        r#"
name = "playbook-ios"
version = "0.1.0"
"#,
    );
    write_skill(&temp.path().join("skills/review"), "Review");

    let output = run_command_output(
        Command::Info {
            package: ".".into(),
            tag: None,
            branch: None,
            json: true,
        },
        temp.path(),
        cache.path(),
    );

    let json: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(json["name"], "playbook-ios");
    assert_eq!(json["alias"], "playbook_ios");
    assert_eq!(json["skills"], serde_json::json!(["review"]));
    assert!(!output.contains("Finished"));
    assert!(!output.contains("Checking"));
}

#[test]
fn add_command_emits_resolving_and_adding_lines() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let (_repo, url) = create_git_dependency();

    let output = run_command_output(
        Command::Add {
            url,
            global: false,
            dev: false,
            tag: None,
            branch: None,
            version: None,
            revision: None,
            adapter: vec![Adapter::Claude],
            component: vec![],
            sync_on_launch: false,
            dry_run: false,
        },
        temp.path(),
        cache.path(),
    );

    assert!(output.contains("Resolving"));
    assert!(output.contains("latest tag"));
    assert!(output.contains("Adding"));
    assert!(output.contains("Finished"));
}

#[test]
fn info_command_renders_version_requirement_for_semver_dependencies() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    write_skill(&repo.path().join("skills/review"), "Review");
    init_git_repo(repo.path());
    let output = ProcessCommand::new("git")
        .args(["tag", "v1.0.0"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    run_command_in_dir(
        Command::Add {
            url: repo.path().to_string_lossy().to_string(),
            global: false,
            dev: false,
            tag: None,
            branch: None,
            version: Some("^1.0.0".into()),
            revision: None,
            adapter: vec![Adapter::Claude],
            component: vec![],
            sync_on_launch: false,
            dry_run: false,
        },
        temp.path(),
        cache.path(),
        &Reporter::silent(),
    )
    .unwrap();

    let alias = crate::manifest::load_root_from_dir(temp.path())
        .unwrap()
        .manifest
        .dependencies
        .keys()
        .next()
        .unwrap()
        .clone();
    let output = run_command_output(
        Command::Info {
            package: alias,
            tag: None,
            branch: None,
            json: false,
        },
        temp.path(),
        cache.path(),
    );

    assert!(output.contains("version-requirement: ^1.0.0"));
    assert!(output.contains("source:"));
}

#[test]
fn add_dry_run_previews_without_writing_project_files() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let (_repo, url) = create_git_dependency();

    let output = run_command_output(
        Command::Add {
            url,
            global: false,
            dev: false,
            tag: None,
            branch: None,
            version: None,
            revision: None,
            adapter: vec![Adapter::Claude],
            component: vec![],
            sync_on_launch: false,
            dry_run: true,
        },
        temp.path(),
        cache.path(),
    );

    assert!(output.contains("dry run: would added") || output.contains("dry run: would add"));
    assert!(output.contains("would create"));
    assert!(!temp.path().join("nodus.toml").exists());
    assert!(!temp.path().join("nodus.lock").exists());
    assert!(!temp.path().join(".codex").exists());
}

#[test]
fn add_dry_run_previews_workspace_members_and_config() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let (_repo, url) = create_workspace_dependency();

    let output = run_command_output(
        Command::Add {
            url,
            global: false,
            dev: false,
            tag: None,
            branch: None,
            version: None,
            revision: None,
            adapter: vec![Adapter::Claude],
            component: vec![],
            sync_on_launch: false,
            dry_run: true,
        },
        temp.path(),
        cache.path(),
    );

    assert!(output.contains("workspace dependency preview:"));
    assert!(output.contains("config:"));
    assert!(output.contains("members = [\"axiom\", \"firebase\"]"));
    assert!(output.contains("axiom (enabled)"));
    assert!(output.contains("firebase (enabled)"));
}

#[test]
fn sync_command_emits_statuses_and_notes() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join(".codex")).unwrap();
    write_file(
        &temp.path().join("nodus.toml"),
        r#"
[[capabilities]]
id = "shell.exec"
sensitivity = "high"
justification = "Run checks."
"#,
    );

    let output = run_command_output(
        Command::Sync {
            locked: false,
            frozen: false,
            allow_high_sensitivity: true,
            force: false,
            adapter: vec![],
            sync_on_launch: false,
            dry_run: false,
        },
        temp.path(),
        cache.path(),
    );

    assert!(output.contains("Resolving"));
    assert!(output.contains("Checking"));
    assert!(output.contains("Snapshotting"));
    assert!(output.contains("note: capability root shell.exec (high)"));
    assert!(output.contains("Finished"));
}

#[test]
fn sync_dry_run_previews_without_writing_project_files() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();

    let output = run_command_output(
        Command::Sync {
            locked: false,
            frozen: false,
            allow_high_sensitivity: false,
            force: false,
            adapter: vec![Adapter::Codex],
            sync_on_launch: true,
            dry_run: true,
        },
        temp.path(),
        cache.path(),
    );

    assert!(output.contains("would create"));
    assert!(output.contains("dry run: would resolve"));
    assert!(!temp.path().join("nodus.toml").exists());
    assert!(!temp.path().join("nodus.lock").exists());
    assert!(!temp.path().join(".codex").exists());
}

#[test]
fn doctor_command_emits_checking_and_finished_lines() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join(".codex")).unwrap();

    let reporter = Reporter::silent();
    resolver::sync_in_dir_with_adapters(
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

    let output = run_command_output(Command::Doctor { json: false }, temp.path(), cache.path());

    assert!(output.contains("Checking"));
    assert!(output.contains("Finished"));
    assert!(output.contains("project state is consistent"));
}

#[test]
fn doctor_command_emits_json_without_status_lines() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join(".codex")).unwrap();

    let reporter = Reporter::silent();
    resolver::sync_in_dir_with_adapters(
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

    let output = run_command_output(Command::Doctor { json: true }, temp.path(), cache.path());

    let json: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(json["package_count"], 1);
    assert_eq!(json["warnings"], serde_json::json!([]));
    assert!(!output.contains("Checking"));
    assert!(!output.contains("Finished"));
}

#[test]
fn outdated_command_emits_json_without_status_lines() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let (_repo, url) = create_git_dependency();

    run_command_in_dir(
        Command::Add {
            url,
            global: false,
            dev: false,
            tag: Some("v0.1.0".into()),
            branch: None,
            version: None,
            revision: None,
            adapter: vec![Adapter::Codex],
            component: vec![],
            sync_on_launch: false,
            dry_run: false,
        },
        temp.path(),
        cache.path(),
        &Reporter::silent(),
    )
    .unwrap();
    let alias = crate::manifest::load_root_from_dir(temp.path())
        .unwrap()
        .manifest
        .dependencies
        .keys()
        .next()
        .unwrap()
        .clone();

    let output = run_command_output(Command::Outdated { json: true }, temp.path(), cache.path());

    let json: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(json["dependency_count"], 1);
    assert_eq!(json["outdated_count"], 0);
    assert_eq!(json["dependencies"][0]["alias"], alias);
    assert_eq!(json["dependencies"][0]["status"], "git_tag_current");
    assert!(!output.contains("Checking"));
    assert!(!output.contains("Finished"));
}

#[test]
fn update_command_emits_updating_and_finished_lines() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let (_repo, url) = create_git_dependency();

    run_command_in_dir(
        Command::Add {
            url,
            global: false,
            dev: false,
            tag: Some("v0.1.0".into()),
            branch: None,
            version: None,
            revision: None,
            adapter: vec![Adapter::Codex],
            component: vec![],
            sync_on_launch: false,
            dry_run: false,
        },
        temp.path(),
        cache.path(),
        &Reporter::silent(),
    )
    .unwrap();

    let output = run_command_output(
        Command::Update {
            allow_high_sensitivity: false,
            dry_run: false,
        },
        temp.path(),
        cache.path(),
    );

    assert!(output.contains("Checking"));
    assert!(output.contains("Resolving"));
    assert!(output.contains("Finished"));
}

#[test]
fn remove_dry_run_keeps_manifest_and_lockfile_unchanged() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let (_repo, url) = create_git_dependency();

    run_command_in_dir(
        Command::Add {
            url,
            global: false,
            dev: false,
            tag: None,
            branch: None,
            version: None,
            revision: None,
            adapter: vec![Adapter::Claude],
            component: vec![],
            sync_on_launch: false,
            dry_run: false,
        },
        temp.path(),
        cache.path(),
        &Reporter::silent(),
    )
    .unwrap();

    let alias = crate::manifest::load_root_from_dir(temp.path())
        .unwrap()
        .manifest
        .dependencies
        .keys()
        .next()
        .unwrap()
        .clone();
    let manifest_before = read_optional(&temp.path().join("nodus.toml")).unwrap();
    let lockfile_before = read_optional(&temp.path().join("nodus.lock")).unwrap();

    let output = run_command_output(
        Command::Remove {
            package: alias,
            global: false,
            dry_run: true,
        },
        temp.path(),
        cache.path(),
    );

    assert!(output.contains("dry run: would remove"));
    assert_eq!(
        read_optional(&temp.path().join("nodus.toml")).unwrap(),
        manifest_before
    );
    assert_eq!(
        read_optional(&temp.path().join("nodus.lock")).unwrap(),
        lockfile_before
    );
}

#[test]
fn update_dry_run_keeps_manifest_and_lockfile_unchanged() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let (repo, url) = create_git_dependency();

    run_command_in_dir(
        Command::Add {
            url,
            global: false,
            dev: false,
            tag: Some("v0.1.0".into()),
            branch: None,
            version: None,
            revision: None,
            adapter: vec![Adapter::Codex],
            component: vec![],
            sync_on_launch: false,
            dry_run: false,
        },
        temp.path(),
        cache.path(),
        &Reporter::silent(),
    )
    .unwrap();

    let output = ProcessCommand::new("git")
        .args(["tag", "v0.2.0"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let manifest_before = read_optional(&temp.path().join("nodus.toml")).unwrap();
    let lockfile_before = read_optional(&temp.path().join("nodus.lock")).unwrap();

    let output = run_command_output(
        Command::Update {
            allow_high_sensitivity: false,
            dry_run: true,
        },
        temp.path(),
        cache.path(),
    );

    assert!(output.contains("dry run: would update"));
    assert_eq!(
        read_optional(&temp.path().join("nodus.toml")).unwrap(),
        manifest_before
    );
    assert_eq!(
        read_optional(&temp.path().join("nodus.lock")).unwrap(),
        lockfile_before
    );
}

#[test]
fn sync_dry_run_locked_and_frozen_modes_leave_state_unchanged() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let (_repo, url) = create_git_dependency();

    run_command_in_dir(
        Command::Add {
            url,
            global: false,
            dev: false,
            tag: None,
            branch: None,
            version: None,
            revision: None,
            adapter: vec![Adapter::Claude],
            component: vec![],
            sync_on_launch: false,
            dry_run: false,
        },
        temp.path(),
        cache.path(),
        &Reporter::silent(),
    )
    .unwrap();

    let manifest_before = read_optional(&temp.path().join("nodus.toml")).unwrap();
    let lockfile_before = read_optional(&temp.path().join("nodus.lock")).unwrap();

    let locked_output = run_command_output(
        Command::Sync {
            locked: true,
            frozen: false,
            allow_high_sensitivity: false,
            force: false,
            adapter: vec![],
            sync_on_launch: false,
            dry_run: true,
        },
        temp.path(),
        cache.path(),
    );
    let frozen_output = run_command_output(
        Command::Sync {
            locked: false,
            frozen: true,
            allow_high_sensitivity: false,
            force: false,
            adapter: vec![],
            sync_on_launch: false,
            dry_run: true,
        },
        temp.path(),
        cache.path(),
    );

    assert!(locked_output.contains("dry run: would resolve"));
    assert!(frozen_output.contains("dry run: would resolve"));
    assert_eq!(
        read_optional(&temp.path().join("nodus.toml")).unwrap(),
        manifest_before
    );
    assert_eq!(
        read_optional(&temp.path().join("nodus.lock")).unwrap(),
        lockfile_before
    );
}

#[test]
fn relay_dry_run_does_not_persist_local_config_or_repo_edits() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let (repo, url) = create_git_dependency();

    let output = ProcessCommand::new("git")
        .args(["remote", "add", "origin", &repo.path().to_string_lossy()])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    run_command_in_dir(
        Command::Add {
            url,
            global: false,
            dev: false,
            tag: None,
            branch: None,
            version: None,
            revision: None,
            adapter: vec![Adapter::Claude],
            component: vec![],
            sync_on_launch: false,
            dry_run: false,
        },
        temp.path(),
        cache.path(),
        &Reporter::silent(),
    )
    .unwrap();

    let managed_skill = first_file_under(&temp.path().join(".claude"), "SKILL.md");
    write_file(
        &managed_skill,
        "---\nname: Review\ndescription: Example skill.\n---\n# Edited\n",
    );
    let repo_skill = repo.path().join("skills/review/SKILL.md");
    let repo_before = read_optional(&repo_skill).unwrap();

    let output = run_command_output(
        Command::Relay {
            packages: vec![
                crate::manifest::load_root_from_dir(temp.path())
                    .unwrap()
                    .manifest
                    .dependencies
                    .keys()
                    .next()
                    .unwrap()
                    .clone(),
            ],
            repo_path: Some(repo.path().to_path_buf()),
            via: Some(Adapter::Claude),
            watch: false,
            dry_run: true,
            create_missing: false,
        },
        temp.path(),
        cache.path(),
    );

    assert!(output.contains("would persist local config"));
    assert!(output.contains("would relay"));
    assert_eq!(read_optional(&repo_skill).unwrap(), repo_before);
    assert!(!temp.path().join(".nodus/local.toml").exists());
    assert!(!temp.path().join(".nodus/.gitignore").exists());
}

#[test]
fn relay_dry_run_previews_state_only_local_config_changes() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let (repo, url) = create_git_dependency();

    let output = ProcessCommand::new("git")
        .args(["remote", "add", "origin", &repo.path().to_string_lossy()])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    run_command_in_dir(
        Command::Add {
            url,
            global: false,
            dev: false,
            tag: None,
            branch: None,
            version: None,
            revision: None,
            adapter: vec![Adapter::Claude],
            component: vec![],
            sync_on_launch: false,
            dry_run: false,
        },
        temp.path(),
        cache.path(),
        &Reporter::silent(),
    )
    .unwrap();

    let alias = crate::manifest::load_root_from_dir(temp.path())
        .unwrap()
        .manifest
        .dependencies
        .keys()
        .next()
        .unwrap()
        .clone();
    run_command_in_dir(
        Command::Relay {
            packages: vec![alias.clone()],
            repo_path: Some(repo.path().to_path_buf()),
            via: None,
            watch: false,
            dry_run: false,
            create_missing: false,
        },
        temp.path(),
        cache.path(),
        &Reporter::silent(),
    )
    .unwrap();

    let managed_skill = first_file_under(&temp.path().join(".claude"), "SKILL.md");
    write_file(
        &managed_skill,
        "---\nname: Review\ndescription: Example skill.\n---\n# Edited\n",
    );
    let repo_skill = repo.path().join("skills/review/SKILL.md");
    let repo_before = read_optional(&repo_skill).unwrap();
    let local_config_path = temp.path().join(".nodus/local.toml");
    let local_config_before = fs::read_to_string(&local_config_path).unwrap();

    let output = run_command_output(
        Command::Relay {
            packages: vec![alias],
            repo_path: None,
            via: None,
            watch: false,
            dry_run: true,
            create_missing: false,
        },
        temp.path(),
        cache.path(),
    );

    assert!(output.contains("would persist local config"));
    assert!(output.contains("would relay"));
    assert_eq!(read_optional(&repo_skill).unwrap(), repo_before);
    assert_eq!(
        fs::read_to_string(&local_config_path).unwrap(),
        local_config_before
    );
}

#[test]
fn relay_rejects_repo_path_for_multiple_dependencies() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();

    let error = run_command_in_dir(
        Command::Relay {
            packages: vec!["example/one".into(), "example/two".into()],
            repo_path: Some(PathBuf::from("/tmp/example")),
            via: None,
            watch: false,
            dry_run: false,
            create_missing: false,
        },
        temp.path(),
        cache.path(),
        &Reporter::silent(),
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("`nodus relay --repo-path` requires exactly one dependency")
    );
}

fn clone_linked_repo(remote: &Path) -> TempDir {
    let linked = TempDir::new().unwrap();
    let target = linked.path().join("linked");
    let output = ProcessCommand::new("git")
        .args([
            "clone",
            remote.to_string_lossy().as_ref(),
            target.to_string_lossy().as_ref(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    linked
}

#[test]
fn relay_supports_multiple_dependencies_in_one_command() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let (repo_one, url_one) = create_git_dependency();
    let (repo_two, url_two) = create_git_dependency();
    write_file(&repo_two.path().join("README.md"), "# Second dependency\n");
    for args in [
        vec!["add", "."],
        vec!["commit", "-m", "second"],
        vec!["tag", "v0.2.0"],
    ] {
        let output = ProcessCommand::new("git")
            .args(args)
            .current_dir(repo_two.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let linked_one = clone_linked_repo(repo_one.path());
    let linked_two = clone_linked_repo(repo_two.path());

    run_command_in_dir(
        Command::Add {
            url: url_one,
            global: false,
            dev: false,
            tag: None,
            branch: None,
            version: None,
            revision: None,
            adapter: vec![Adapter::Claude],
            component: vec![],
            sync_on_launch: false,
            dry_run: false,
        },
        temp.path(),
        cache.path(),
        &Reporter::silent(),
    )
    .unwrap();
    let alias_one = crate::manifest::load_root_from_dir(temp.path())
        .unwrap()
        .manifest
        .dependencies
        .keys()
        .next()
        .unwrap()
        .clone();
    run_command_in_dir(
        Command::Add {
            url: url_two,
            global: false,
            dev: false,
            tag: Some("v0.2.0".into()),
            branch: None,
            version: None,
            revision: None,
            adapter: vec![Adapter::Claude],
            component: vec![],
            sync_on_launch: false,
            dry_run: false,
        },
        temp.path(),
        cache.path(),
        &Reporter::silent(),
    )
    .unwrap();

    let aliases = crate::manifest::load_root_from_dir(temp.path())
        .unwrap()
        .manifest
        .dependencies
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(aliases.len(), 2);
    let alias_two = aliases
        .iter()
        .find(|alias| **alias != alias_one)
        .unwrap()
        .clone();

    run_command_in_dir(
        Command::Relay {
            packages: vec![alias_one.clone()],
            repo_path: Some(linked_one.path().join("linked")),
            via: Some(Adapter::Claude),
            watch: false,
            dry_run: false,
            create_missing: false,
        },
        temp.path(),
        cache.path(),
        &Reporter::silent(),
    )
    .unwrap();
    run_command_in_dir(
        Command::Relay {
            packages: vec![alias_two.clone()],
            repo_path: Some(linked_two.path().join("linked")),
            via: Some(Adapter::Claude),
            watch: false,
            dry_run: false,
            create_missing: false,
        },
        temp.path(),
        cache.path(),
        &Reporter::silent(),
    )
    .unwrap();

    let managed_skills = WalkDir::new(temp.path().join(".claude"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == "SKILL.md")
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    assert_eq!(managed_skills.len(), 2);
    for path in &managed_skills {
        let mut contents = fs::read_to_string(path).unwrap();
        contents.push_str("\nBatch relay update.\n");
        fs::write(path, contents).unwrap();
    }

    let output = run_command_output(
        Command::Relay {
            packages: aliases.clone(),
            repo_path: None,
            via: Some(Adapter::Claude),
            watch: false,
            dry_run: false,
            create_missing: false,
        },
        temp.path(),
        cache.path(),
    );

    assert!(output.contains("relayed 2 dependencies; created 0 and updated 2 source files"));
    assert!(
        fs::read_to_string(linked_one.path().join("linked/skills/review/SKILL.md"))
            .unwrap()
            .ends_with("\nBatch relay update.\n")
    );
    assert!(
        fs::read_to_string(linked_two.path().join("linked/skills/review/SKILL.md"))
            .unwrap()
            .ends_with("\nBatch relay update.\n")
    );
}
