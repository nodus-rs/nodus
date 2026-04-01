use std::path::PathBuf;

use clap::{Parser, Subcommand};
use clap_complete::Shell;

use crate::adapters::Adapter;
use crate::cli::help::{
    ADD_ABOUT, ADD_AFTER_LONG_HELP, ADD_LONG_ABOUT, CLEAN_ABOUT, CLEAN_AFTER_LONG_HELP,
    CLEAN_LONG_ABOUT, COMPLETION_ABOUT, COMPLETION_LONG_ABOUT, DOCTOR_ABOUT,
    DOCTOR_AFTER_LONG_HELP, DOCTOR_LONG_ABOUT, INFO_ABOUT, INFO_AFTER_LONG_HELP,
    INFO_LONG_ABOUT, INIT_ABOUT, INIT_AFTER_LONG_HELP, INIT_LONG_ABOUT, LIST_ABOUT,
    LIST_AFTER_LONG_HELP, LIST_LONG_ABOUT, OUTDATED_ABOUT, OUTDATED_AFTER_LONG_HELP,
    OUTDATED_LONG_ABOUT, RELAY_ABOUT, RELAY_AFTER_LONG_HELP, RELAY_LONG_ABOUT, REMOVE_ABOUT,
    REMOVE_AFTER_LONG_HELP, REMOVE_LONG_ABOUT, ROOT_ABOUT, ROOT_AFTER_LONG_HELP, ROOT_LONG_ABOUT,
    REVIEW_ABOUT, REVIEW_AFTER_LONG_HELP, REVIEW_LONG_ABOUT, SYNC_ABOUT, SYNC_AFTER_LONG_HELP,
    SYNC_LONG_ABOUT, UPDATE_ABOUT, UPDATE_AFTER_LONG_HELP, UPDATE_LONG_ABOUT, UPGRADE_ABOUT,
    UPGRADE_AFTER_LONG_HELP, UPGRADE_LONG_ABOUT,
};
use crate::manifest::DependencyComponent;
use crate::review::ReviewProvider;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = ROOT_ABOUT,
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
        about = ADD_ABOUT,
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
        about = REMOVE_ABOUT,
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
        about = LIST_ABOUT,
        long_about = LIST_LONG_ABOUT,
        after_long_help = LIST_AFTER_LONG_HELP
    )]
    List {
        #[arg(
            long,
            help = "Emit machine-readable JSON instead of human-readable text"
        )]
        json: bool,
    },
    #[command(
        about = INFO_ABOUT,
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
        about = REVIEW_ABOUT,
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
        about = OUTDATED_ABOUT,
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
        about = UPDATE_ABOUT,
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
        about = UPGRADE_ABOUT,
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
        about = RELAY_ABOUT,
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
        about = INIT_ABOUT,
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
        about = SYNC_ABOUT,
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
        about = CLEAN_ABOUT,
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
    #[command(about = COMPLETION_ABOUT, long_about = COMPLETION_LONG_ABOUT)]
    Completion {
        #[arg(value_enum, help = "Shell to generate completions for")]
        shell: Shell,
    },
    #[command(
        about = DOCTOR_ABOUT,
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
