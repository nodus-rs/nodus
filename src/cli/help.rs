pub(super) const ROOT_ABOUT: &str = "Install and maintain repo-scoped agent packages";

pub(super) const ROOT_LONG_ABOUT: &str = r#"Nodus adds AI agent packages to this repo and keeps the generated tool files in sync.

Most common tasks:
  nodus add nodus-rs/nodus --adapter codex
  nodus doctor
  nodus sync
  nodus update

Typical workflows:
  first install: add -> doctor
  rebuild current setup: sync -> doctor
  upgrade packages: update -> doctor
  remove a package: remove -> doctor
"#;

pub(super) const ROOT_AFTER_LONG_HELP: &str = r#"Need details? Run `nodus <command> --help` for examples and flag details.

Project-scoped installs are the default. Use `--global` on `nodus add` or `nodus remove` when you want user-level state instead of repo state.
"#;

pub(super) const ADD_ABOUT: &str = "Add a dependency and run sync";

pub(super) const ADD_LONG_ABOUT: &str = r#"Install one package into this repo and immediately write the managed files the selected AI tool needs.

Most common use:
  nodus add nodus-rs/nodus --adapter codex

Project-scoped installs do not add the startup sync hook by default. Pass `--sync-on-launch` when you want supported tools to run `nodus sync` when they open this repository.

By default Nodus installs the whole package. Wrapper packages that expose multiple child packages are added with no child packages enabled until you select `members` manually or pass `--accept-all-dependencies`.

What this changes:
  - creates or updates `nodus.toml`
  - resolves and records exact package revisions in `nodus.lock`
  - writes managed files under tool folders such as `.codex/` or `.claude/`

Run `nodus doctor` next to verify the repo is healthy."#;

pub(super) const ADD_AFTER_LONG_HELP: &str = r#"Examples:
  nodus add nodus-rs/nodus --adapter codex
  nodus add nodus-rs/nodus --adapter codex --sync-on-launch
  nodus add owner/repo --adapter codex --exclude-component mcp
  nodus add ./vendor/playbook --adapter claude
  nodus add owner/repo --tag v1.2.3 --adapter codex
  nodus add owner/marketplace --accept-all-dependencies --adapter codex
  nodus add owner/repo --global --adapter codex

After a project-scoped install, run `nodus doctor` to confirm the repo is consistent."#;

pub(super) const REMOVE_ABOUT: &str = "Remove a dependency and prune its managed outputs";

pub(super) const REMOVE_LONG_ABOUT: &str = r#"Remove a configured dependency, update `nodus.toml`, and prune the runtime files that dependency no longer owns.

Use this when you want to delete a package from the repo and keep the remaining managed files aligned with the current manifest.

Run `nodus doctor` next to confirm the repo is still consistent."#;

pub(super) const REMOVE_AFTER_LONG_HELP: &str = r#"Common options:
  nodus remove <package>
  nodus remove <package> --global
  nodus remove <package> --dry-run

Examples:
  nodus remove nodus
  nodus remove nodus --global
  nodus remove nodus --dry-run"#;

pub(super) const MEMBERS_ABOUT: &str =
    "Manage selected child packages for wrapper and workspace dependencies";

pub(super) const MEMBERS_LONG_ABOUT: &str = r#"Manage selected child packages for wrapper and workspace dependencies.

Inspect or update the `members = [...]` selection for a direct dependency that exposes child packages.

Use this after installing a wrapper or workspace dependency when you want to enable, disable, or replace the selected child packages without editing `nodus.toml` by hand."#;

pub(super) const MEMBERS_LIST_ABOUT: &str = "Show enabled and disabled child packages";

pub(super) const MEMBERS_LIST_LONG_ABOUT: &str = r#"List the selectable child packages for one direct dependency, or for every direct dependency that exposes child packages."#;

pub(super) const MEMBERS_ENABLE_ABOUT: &str = "Enable one or more child packages and resync";

pub(super) const MEMBERS_ENABLE_LONG_ABOUT: &str = r#"Enable one or more child packages for a direct wrapper or workspace dependency, update `nodus.toml`, and sync managed outputs."#;

pub(super) const MEMBERS_DISABLE_ABOUT: &str = "Disable one or more child packages and resync";

pub(super) const MEMBERS_DISABLE_LONG_ABOUT: &str = r#"Disable one or more child packages for a direct wrapper or workspace dependency, update `nodus.toml`, and sync managed outputs."#;

pub(super) const MEMBERS_SET_ABOUT: &str = "Replace the selected child packages and resync";

pub(super) const MEMBERS_SET_LONG_ABOUT: &str = r#"Replace the selected child packages for a direct wrapper or workspace dependency, update `nodus.toml`, and sync managed outputs.

Pass no child packages to clear the current selection and leave the wrapper recorded without enabled children."#;

pub(super) const LIST_ABOUT: &str = "List configured dependencies and any locked metadata";

pub(super) const LIST_LONG_ABOUT: &str = "List the dependencies recorded in `nodus.toml` together with any resolved metadata from `nodus.lock`.";

pub(super) const LIST_AFTER_LONG_HELP: &str = r#"Examples:
  nodus list
  nodus list --json"#;

pub(super) const INFO_ABOUT: &str = "Display resolved package metadata";

pub(super) const INFO_LONG_ABOUT: &str = r#"Inspect a package without changing the current repo.

Use this when you want to see discovered skills, agents, rules, commands, managed exports, or the resolved ref before you install or update a package."#;

pub(super) const INFO_AFTER_LONG_HELP: &str = r#"Examples:
  nodus info nodus-rs/nodus
  nodus info ./vendor/playbook
  nodus info nodus --json"#;

pub(super) const REVIEW_ABOUT: &str =
    "Use an AI review agent to assess whether a package graph looks safe to use";

pub(super) const REVIEW_LONG_ABOUT: &str = r#"Ask an AI review agent to assess whether a package graph looks safe to use before you install or update it."#;

pub(super) const REVIEW_AFTER_LONG_HELP: &str = r#"Examples:
  nodus review
  nodus review owner/repo --tag v1.2.3
  nodus review owner/repo --provider anthropic"#;

pub(super) const OUTDATED_ABOUT: &str =
    "Check configured dependencies for newer tags or branch head changes";

pub(super) const OUTDATED_LONG_ABOUT: &str = r#"Check whether configured dependencies have newer tags available, or whether tracked branches moved forward, without changing the repo."#;

pub(super) const OUTDATED_AFTER_LONG_HELP: &str = r#"Examples:
  nodus outdated
  nodus outdated --json"#;

pub(super) const UPDATE_ABOUT: &str = "Update configured dependencies and resync managed outputs";

pub(super) const UPDATE_LONG_ABOUT: &str = r#"Resolve newer allowed versions for configured dependencies, rewrite `nodus.lock`, and sync managed outputs to match the new result.

Use this when you want to upgrade what this repo already declares.

Use `nodus update` when you want newer package revisions. Use `nodus sync` when you only want to rebuild from the versions you already have recorded.

Run `nodus doctor` next to verify the repo is consistent."#;

pub(super) const UPDATE_AFTER_LONG_HELP: &str = r#"Common options:
  nodus update
  nodus update --dry-run
  nodus update --allow-high-sensitivity

Examples:
  nodus update
  nodus update --dry-run
  nodus update --allow-high-sensitivity"#;

pub(super) const UPGRADE_ABOUT: &str =
    "Check for or install a newer nodus CLI when the install method is supported";

pub(super) const UPGRADE_LONG_ABOUT: &str = r#"Check whether the installed `nodus` CLI can be upgraded, or install the newer version when the current install method supports that workflow."#;

pub(super) const UPGRADE_AFTER_LONG_HELP: &str = r#"Examples:
  nodus upgrade --check
  nodus upgrade"#;

pub(super) const RELAY_ABOUT: &str = "Relay linked managed edits back into a maintainer checkout";

pub(super) const RELAY_LONG_ABOUT: &str = r#"Relay edits from managed runtime files in a consumer repo back into a maintainer checkout.

This is mainly for package maintainers. Most users do not need `relay` in normal package consumption workflows."#;

pub(super) const RELAY_AFTER_LONG_HELP: &str = r#"Examples:
  nodus relay nodus --repo-path ../nodus
  nodus relay nodus --watch
  nodus relay nodus --repo-path ../nodus --create-missing"#;

pub(super) const INIT_ABOUT: &str = "Create a minimal nodus.toml and example skill";

pub(super) const INIT_LONG_ABOUT: &str = "Create a minimal `nodus.toml` and example package content when you are starting a new Nodus package repo.";

pub(super) const INIT_AFTER_LONG_HELP: &str = r#"Examples:
  nodus init
  nodus init --dry-run"#;

pub(super) const SYNC_LONG_ABOUT: &str = r#"Resolve the dependencies already declared in `nodus.toml` and write the managed adapter outputs that should exist for the current repo.

Use this when you want to rebuild from what this repo already declares.

Plain `nodus sync` does not add the startup sync hook by default. Pass `--sync-on-launch` when you want Nodus to persist `nodus.sync_on_startup` into `nodus.toml`.

Plain `nodus sync` also reuses the last locked cached revision when a Git dependency cannot be refreshed. Pass `--strict` when that situation should fail the sync instead.

Use `nodus sync` after manifest changes, after editing package content locally, or when you want to rebuild outputs without upgrading dependencies.

Run `nodus doctor` next to verify the repo stays healthy."#;

pub(super) const SYNC_ABOUT: &str = "Resolve dependencies and write managed runtime outputs";

pub(super) const SYNC_AFTER_LONG_HELP: &str = r#"Common options:
  nodus sync
  nodus sync --sync-on-launch
  nodus sync --locked
  nodus sync --frozen
  nodus sync --strict
  nodus sync --force

Examples:
  nodus sync
  nodus sync --sync-on-launch
  nodus sync --locked
  nodus sync --frozen
  nodus sync --strict
  nodus sync --force

Use `--locked` when the lockfile must stay unchanged. Use `--frozen` when installs must come exactly from the existing `nodus.lock`. Use `--strict` when any Git refresh failure should stop the sync instead of falling back to cached locked data."#;

pub(super) const CLEAN_ABOUT: &str = "Clear shared repository, checkout, and snapshot cache data";

pub(super) const CLEAN_LONG_ABOUT: &str = r#"Clear shared package cache data without changing `nodus.toml`, `nodus.lock`, or generated runtime outputs.

By default `nodus clean` removes only the cache entries referenced by the current repo's `nodus.lock`. Use `--all` when you want to clear the shared cache directories for every project under the selected store root.

The cache is shared, so project-scoped cleanup can make another repo redownload the same package data on its next `nodus sync`."#;

pub(super) const CLEAN_AFTER_LONG_HELP: &str = r#"Examples:
  nodus clean
  nodus clean --dry-run
  nodus clean --all

After cleaning the cache, run `nodus sync` again when you want Nodus to recreate the missing mirrors, checkouts, and snapshots."#;

pub(super) const COMPLETION_ABOUT: &str = "Generate shell completion scripts";

pub(super) const COMPLETION_LONG_ABOUT: &str = "Generate shell completion scripts for `nodus` so the shell can suggest commands and flags interactively.";

pub(super) const DOCTOR_ABOUT: &str =
    "Validate lockfile, shared store, and managed output consistency";

pub(super) const DOCTOR_LONG_ABOUT: &str = r#"If Nodus feels broken, start here.

Default behavior:
  - runs a read-only preview
  - reports repo consistency problems without changing anything
  - use `--apply` to repair safe issues and confirm risky ones
  - use `--apply --yes` for non-interactive repairs

Validate that `nodus.toml`, `nodus.lock`, the shared store, and the managed adapter outputs are still in sync.

Run this after `nodus add`, `nodus sync`, `nodus update`, or `nodus remove` when you want a final health check."#;

pub(super) const DOCTOR_AFTER_LONG_HELP: &str = r#"Common commands:
  nodus doctor
  nodus doctor --apply
  nodus doctor --apply --yes
  nodus doctor --json

Examples:
  nodus doctor
  nodus doctor --apply
  nodus doctor --apply --yes
  nodus doctor --json"#;

pub(super) const MCP_ABOUT: &str = "MCP server for AI tool integration";

pub(super) const MCP_LONG_ABOUT: &str = r#"Model Context Protocol (MCP) integration.

Exposes nodus operations as MCP tools so AI agents can manage packages, relay edits, and inspect project state."#;

pub(super) const MCP_SERVE_ABOUT: &str = "Start the MCP server on stdio";

pub(super) const MCP_SERVE_LONG_ABOUT: &str = r#"Start a Model Context Protocol server that communicates via stdin/stdout.

AI tools like Claude, Cursor, and Codex connect to this server to access nodus operations as MCP tools. The server runs until the client disconnects or the process is terminated.

Example MCP config entry:
  {
    "nodus": {
      "command": "nodus",
      "args": ["mcp", "serve"]
    }
  }"#;

pub(super) const MCP_STATUS_ABOUT: &str = "Inspect managed MCP config wiring for this project";

pub(super) const MCP_STATUS_LONG_ABOUT: &str = r#"Inspect the current project's managed MCP config files and report whether the auto-registered `nodus` server entry is present and correctly wired.

Checks the project `.mcp.json`, `.codex/config.toml`, and `opencode.json` files, then compares any discovered `nodus` entry against the expected `nodus mcp serve` command."#;
