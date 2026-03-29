use std::path::PathBuf;

use clap::{Parser, Subcommand};
use clap_complete::Shell;

use crate::adapters::Adapter;
use crate::manifest::DependencyComponent;
use crate::review::ReviewProvider;

const ROOT_LONG_ABOUT: &str = r#"Nodus installs agent packages from GitHub, Git URLs, or local paths, locks the exact revision you resolved, and writes only the adapter runtime files your repo actually uses.

For most repos, the normal flow is:
  1. `nodus add <package> --adapter <adapter>`
  2. `nodus doctor`
  3. Use `nodus sync` or `nodus update` as the package changes over time
"#;

const ROOT_AFTER_LONG_HELP: &str = r#"Examples:
  nodus add nodus-rs/nodus --adapter codex
  nodus info nodus-rs/nodus
  nodus sync --locked

Project-scoped installs are the default. Use `--global` on `nodus add` or `nodus remove` when you want user-level state instead of repo state.

Use `nodus <command> --help` for examples and flag details."#;

const ADD_LONG_ABOUT: &str = r#"Add a package to the current repo and immediately sync the managed outputs for the selected adapters.

`<PACKAGE>` can be:
  - a GitHub shortcut like `owner/repo`
  - a full Git URL
  - a local path

By default Nodus installs the whole package. Wrapper packages that expose multiple child packages are added with no child packages enabled until you select `members` manually or pass `--accept-all-dependencies`."#;

const ADD_AFTER_LONG_HELP: &str = r#"Examples:
  nodus add nodus-rs/nodus --adapter codex
  nodus add ./vendor/playbook --adapter claude
  nodus add owner/repo --tag v1.2.3 --adapter codex
  nodus add owner/marketplace --accept-all-dependencies --adapter codex
  nodus add owner/repo --global --adapter codex

After a project-scoped install, run `nodus doctor` to confirm the repo is consistent."#;

const REMOVE_LONG_ABOUT: &str = r#"Remove a configured dependency, update `nodus.toml`, and prune the runtime files that dependency no longer owns."#;

const REMOVE_AFTER_LONG_HELP: &str = r#"Examples:
  nodus remove nodus
  nodus remove nodus --global
  nodus remove nodus --dry-run"#;

const INFO_LONG_ABOUT: &str = r#"Inspect a package without changing the current repo.

Use this when you want to see discovered skills, agents, rules, commands, managed exports, or the resolved ref before you install or update a package."#;

const INFO_AFTER_LONG_HELP: &str = r#"Examples:
  nodus info nodus-rs/nodus
  nodus info ./vendor/playbook
  nodus info nodus --json"#;

const REVIEW_LONG_ABOUT: &str = r#"Ask an AI review agent to assess whether a package graph looks safe to use before you install or update it."#;

const REVIEW_AFTER_LONG_HELP: &str = r#"Examples:
  nodus review
  nodus review owner/repo --tag v1.2.3
  nodus review owner/repo --provider anthropic"#;

const OUTDATED_LONG_ABOUT: &str = r#"Check whether configured dependencies have newer tags available, or whether tracked branches moved forward, without changing the repo."#;

const OUTDATED_AFTER_LONG_HELP: &str = r#"Examples:
  nodus outdated
  nodus outdated --json"#;

const UPDATE_LONG_ABOUT: &str = r#"Resolve newer allowed versions for configured dependencies, rewrite `nodus.lock`, and sync managed outputs to match the new result.

Use `nodus update` when you want newer package revisions. Use `nodus sync` when you only want to rebuild from the versions you already have recorded."#;

const UPDATE_AFTER_LONG_HELP: &str = r#"Examples:
  nodus update
  nodus update --dry-run
  nodus update --allow-high-sensitivity"#;

const UPGRADE_LONG_ABOUT: &str = r#"Check whether the installed `nodus` CLI can be upgraded, or install the newer version when the current install method supports that workflow."#;

const UPGRADE_AFTER_LONG_HELP: &str = r#"Examples:
  nodus upgrade --check
  nodus upgrade"#;

const RELAY_LONG_ABOUT: &str = r#"Relay edits from managed runtime files in a consumer repo back into a maintainer checkout.

This is mainly for package maintainers. Most users do not need `relay` in normal package consumption workflows."#;

const RELAY_AFTER_LONG_HELP: &str = r#"Examples:
  nodus relay nodus --repo-path ../nodus
  nodus relay nodus --watch
  nodus relay nodus --repo-path ../nodus --create-missing"#;

const INIT_LONG_ABOUT: &str = r#"Create a minimal `nodus.toml` and example package content when you are starting a new Nodus package repo."#;

const INIT_AFTER_LONG_HELP: &str = r#"Examples:
  nodus init
  nodus init --dry-run"#;

const SYNC_LONG_ABOUT: &str = r#"Resolve the dependencies already declared in `nodus.toml` and write the managed adapter outputs that should exist for the current repo.

Use `nodus sync` after manifest changes, after editing package content locally, or when you want to rebuild outputs without upgrading dependencies."#;

const SYNC_AFTER_LONG_HELP: &str = r#"Examples:
  nodus sync
  nodus sync --locked
  nodus sync --frozen
  nodus sync --force

Use `--locked` when the lockfile must stay unchanged. Use `--frozen` when installs must come exactly from the existing `nodus.lock`."#;

const CLEAN_LONG_ABOUT: &str = r#"Clear shared package cache data without changing `nodus.toml`, `nodus.lock`, or generated runtime outputs.

By default `nodus clean` removes only the cache entries referenced by the current repo's `nodus.lock`. Use `--all` when you want to clear the shared cache directories for every project under the selected store root.

The cache is shared, so project-scoped cleanup can make another repo redownload the same package data on its next `nodus sync`."#;

const CLEAN_AFTER_LONG_HELP: &str = r#"Examples:
  nodus clean
  nodus clean --dry-run
  nodus clean --all

After cleaning the cache, run `nodus sync` again when you want Nodus to recreate the missing mirrors, checkouts, and snapshots."#;

const COMPLETION_LONG_ABOUT: &str = r#"Generate shell completion scripts for `nodus` so the shell can suggest commands and flags interactively."#;

const DOCTOR_LONG_ABOUT: &str = r#"Validate that `nodus.toml`, `nodus.lock`, the shared store, and the managed adapter outputs are still in sync.

Run this after `nodus add`, `nodus sync`, `nodus update`, or `nodus remove` when you want a final health check."#;

const DOCTOR_AFTER_LONG_HELP: &str = r#"Examples:
  nodus doctor
  nodus doctor --json"#;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Install and maintain repo-scoped agent packages",
    long_about = ROOT_LONG_ABOUT,
    after_long_help = ROOT_AFTER_LONG_HELP
)]
pub(super) struct Cli {
    #[arg(
        long = "store-path",
        alias = "cache-path",
        global = true,
        help = "Override the shared storage root for repository mirrors, checkouts, and snapshots"
    )]
    pub(super) store_path: Option<PathBuf>,

    #[command(subcommand)]
    pub(super) command: Command,
}

#[derive(Debug, Subcommand)]
pub(super) enum Command {
    #[command(
        about = "Add a dependency and run sync",
        long_about = ADD_LONG_ABOUT,
        after_long_help = ADD_AFTER_LONG_HELP
    )]
    Add {
        #[arg(
            value_name = "PACKAGE",
            help = "Git URL, local path, or GitHub shortcut like owner/repo"
        )]
        url: String,
        #[arg(
            long,
            help = "Install into user-level global state and home-scoped agent folders instead of the current repository"
        )]
        global: bool,
        #[arg(
            long,
            help = "Record the dependency under `[dev-dependencies]` instead of `[dependencies]`"
        )]
        dev: bool,
        #[arg(
            long,
            conflicts_with_all = ["branch", "revision"],
            help = "Pin a specific Git tag instead of resolving the latest tag"
        )]
        tag: Option<String>,
        #[arg(
            long,
            conflicts_with_all = ["tag", "revision"],
            help = "Track a specific Git branch instead of resolving the latest tag"
        )]
        branch: Option<String>,
        #[arg(
            long,
            conflicts_with_all = ["tag", "branch", "revision"],
            help = "Select the highest compatible semver tag, such as ^1.2.0"
        )]
        version: Option<String>,
        #[arg(
            long,
            conflicts_with_all = ["tag", "branch", "version"],
            help = "Pin a specific Git commit revision"
        )]
        revision: Option<String>,
        #[arg(
            long,
            value_enum,
            help = "Select one or more adapters to persist for this install target"
        )]
        adapter: Vec<Adapter>,
        #[arg(
            long,
            value_enum,
            help = "Select which dependency components to install from the package"
        )]
        component: Vec<DependencyComponent>,
        #[arg(
            long = "sync-on-launch",
            help = "Persist project startup hooks so supported tools run `nodus sync` when they open this repository"
        )]
        sync_on_launch: bool,
        #[arg(
            long = "accept-all-dependencies",
            help = "Enable every child package exposed by a workspace or marketplace wrapper instead of leaving multi-package wrappers disabled by default"
        )]
        accept_all_dependencies: bool,
        #[arg(
            long = "dry-run",
            help = "Preview project changes without writing to the project or linked repo; may still populate the shared store to compute the result"
        )]
        dry_run: bool,
    },
    #[command(
        about = "Remove a dependency and prune its managed outputs",
        long_about = REMOVE_LONG_ABOUT,
        after_long_help = REMOVE_AFTER_LONG_HELP
    )]
    Remove {
        #[arg(help = "Dependency alias or repository reference to remove")]
        package: String,
        #[arg(
            long,
            help = "Remove from user-level global state and home-scoped agent folders instead of the current repository"
        )]
        global: bool,
        #[arg(
            long = "dry-run",
            help = "Preview project changes without writing to the project or linked repo; may still populate the shared store to compute the result"
        )]
        dry_run: bool,
    },
    #[command(
        about = "List configured dependencies and any locked metadata",
        long_about = "List the dependencies recorded in `nodus.toml` together with any resolved metadata from `nodus.lock`.",
        after_long_help = "Examples:\n  nodus list\n  nodus list --json"
    )]
    List {
        #[arg(
            long,
            help = "Emit machine-readable JSON instead of human-readable text"
        )]
        json: bool,
    },
    #[command(
        about = "Display resolved package metadata",
        long_about = INFO_LONG_ABOUT,
        after_long_help = INFO_AFTER_LONG_HELP
    )]
    Info {
        #[arg(
            help = "Dependency alias, local package path, Git URL, or GitHub shortcut like owner/repo"
        )]
        package: String,
        #[arg(long, conflicts_with = "branch", help = "Inspect a specific Git tag")]
        tag: Option<String>,
        #[arg(long, conflicts_with = "tag", help = "Inspect a specific Git branch")]
        branch: Option<String>,
        #[arg(
            long,
            help = "Emit machine-readable JSON instead of human-readable text"
        )]
        json: bool,
    },
    #[command(
        about = "Use an AI review agent to assess whether a package graph looks safe to use",
        long_about = REVIEW_LONG_ABOUT,
        after_long_help = REVIEW_AFTER_LONG_HELP
    )]
    Review {
        #[arg(
            default_value = ".",
            help = "Dependency alias, local package path, Git URL, or GitHub shortcut like owner/repo"
        )]
        package: String,
        #[arg(long, conflicts_with = "branch", help = "Inspect a specific Git tag")]
        tag: Option<String>,
        #[arg(long, conflicts_with = "tag", help = "Inspect a specific Git branch")]
        branch: Option<String>,
        #[arg(
            long,
            value_enum,
            default_value_t = ReviewProvider::Openai,
            help = "LLM provider to use for the safety review"
        )]
        provider: ReviewProvider,
        #[arg(
            long,
            help = "Specific model id to use; defaults to $MENTRA_MODEL or the provider's newest available model"
        )]
        model: Option<String>,
    },
    #[command(
        about = "Check configured dependencies for newer tags or branch head changes",
        long_about = OUTDATED_LONG_ABOUT,
        after_long_help = OUTDATED_AFTER_LONG_HELP
    )]
    Outdated {
        #[arg(
            long,
            help = "Emit machine-readable JSON instead of human-readable text"
        )]
        json: bool,
    },
    #[command(
        about = "Update configured dependencies and resync managed outputs",
        long_about = UPDATE_LONG_ABOUT,
        after_long_help = UPDATE_AFTER_LONG_HELP
    )]
    Update {
        #[arg(
            long = "allow-high-sensitivity",
            help = "Allow packages that declare high-sensitivity capabilities"
        )]
        allow_high_sensitivity: bool,
        #[arg(
            long = "dry-run",
            help = "Preview project changes without writing to the project or linked repo; may still populate the shared store to compute the result"
        )]
        dry_run: bool,
    },
    #[command(
        alias = "self-update",
        about = "Check for or install a newer nodus CLI when the install method is supported",
        long_about = UPGRADE_LONG_ABOUT,
        after_long_help = UPGRADE_AFTER_LONG_HELP
    )]
    Upgrade {
        #[arg(
            long,
            help = "Check whether a newer nodus CLI release is available without installing it"
        )]
        check: bool,
    },
    #[command(
        about = "Relay linked managed edits back into a maintainer checkout",
        long_about = RELAY_LONG_ABOUT,
        after_long_help = RELAY_AFTER_LONG_HELP
    )]
    Relay {
        #[arg(
            required = true,
            num_args = 1..,
            help = "One or more dependency aliases or repository references to relay"
        )]
        packages: Vec<String>,
        #[arg(
            long,
            help = "Local checkout path to persist and relay into; requires exactly one dependency"
        )]
        repo_path: Option<PathBuf>,
        #[arg(
            long = "via",
            alias = "relay-via",
            alias = "prefer",
            value_enum,
            help = "Persist the preferred adapter for relay metadata when one adapter should be treated as canonical"
        )]
        via: Option<Adapter>,
        #[arg(
            long,
            help = "Keep watching managed outputs and relay new edits automatically"
        )]
        watch: bool,
        #[arg(
            long = "dry-run",
            conflicts_with = "watch",
            help = "Preview project changes without writing to the project or linked repo; may still populate the shared store to compute the result"
        )]
        dry_run: bool,
        #[arg(
            long = "create-missing",
            help = "Create missing source skills and agents in the linked maintainer checkout from managed runtime files"
        )]
        create_missing: bool,
    },
    #[command(
        about = "Create a minimal nodus.toml and example skill",
        long_about = INIT_LONG_ABOUT,
        after_long_help = INIT_AFTER_LONG_HELP
    )]
    Init {
        #[arg(
            long = "dry-run",
            help = "Preview project changes without writing to the project or linked repo; may still populate the shared store to compute the result"
        )]
        dry_run: bool,
    },
    #[command(
        about = "Resolve dependencies and write managed runtime outputs",
        long_about = SYNC_LONG_ABOUT,
        after_long_help = SYNC_AFTER_LONG_HELP
    )]
    Sync {
        #[arg(
            long,
            conflicts_with = "frozen",
            help = "Fail if nodus.lock would change"
        )]
        locked: bool,
        #[arg(
            long,
            conflicts_with = "locked",
            help = "Install exact Git revisions from nodus.lock and fail if the lockfile is missing or stale"
        )]
        frozen: bool,
        #[arg(
            long = "allow-high-sensitivity",
            help = "Allow packages that declare high-sensitivity capabilities"
        )]
        allow_high_sensitivity: bool,
        #[arg(
            long,
            help = "Overwrite unmanaged files when this sync is about to manage those paths"
        )]
        force: bool,
        #[arg(
            long,
            value_enum,
            help = "Override and persist the adapter selection for this repository"
        )]
        adapter: Vec<Adapter>,
        #[arg(
            long = "sync-on-launch",
            help = "Persist project startup hooks so supported tools run `nodus sync` when they open this repository"
        )]
        sync_on_launch: bool,
        #[arg(
            long = "dry-run",
            help = "Preview project changes without writing to the project or linked repo; may still populate the shared store to compute the result"
        )]
        dry_run: bool,
    },
    #[command(
        about = "Clear shared repository, checkout, and snapshot cache data",
        long_about = CLEAN_LONG_ABOUT,
        after_long_help = CLEAN_AFTER_LONG_HELP
    )]
    Clean {
        #[arg(
            long,
            help = "Clear the shared cache directories for every project under the selected store root"
        )]
        all: bool,
        #[arg(
            long = "dry-run",
            help = "Preview cache removals without deleting anything"
        )]
        dry_run: bool,
    },
    #[command(about = "Generate shell completion scripts", long_about = COMPLETION_LONG_ABOUT)]
    Completion {
        #[arg(value_enum, help = "Shell to generate completions for")]
        shell: Shell,
    },
    #[command(
        about = "Validate lockfile, shared store, and managed output consistency",
        long_about = DOCTOR_LONG_ABOUT,
        after_long_help = DOCTOR_AFTER_LONG_HELP
    )]
    Doctor {
        #[arg(
            long,
            help = "Emit machine-readable JSON instead of human-readable text"
        )]
        json: bool,
    },
}
