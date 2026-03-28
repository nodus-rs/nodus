use std::path::PathBuf;

use clap::{Parser, Subcommand};
use clap_complete::Shell;

use crate::adapters::Adapter;
use crate::manifest::DependencyComponent;
use crate::review::ReviewProvider;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Manage project-scoped agent packages",
    long_about = "Nodus resolves agent packages from local paths and Git tags, locks exact revisions, and writes managed runtime outputs for supported adapters."
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
    #[command(about = "Add a dependency and run sync")]
    Add {
        #[arg(help = "Git URL, local path, or GitHub shortcut like owner/repo")]
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
            long = "dry-run",
            help = "Preview project changes without writing to the project or linked repo; may still populate the shared store to compute the result"
        )]
        dry_run: bool,
    },
    #[command(about = "Remove a dependency and prune its managed outputs")]
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
    #[command(about = "List configured dependencies and any locked metadata")]
    List {
        #[arg(
            long,
            help = "Emit machine-readable JSON instead of human-readable text"
        )]
        json: bool,
    },
    #[command(about = "Display resolved package metadata")]
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
    #[command(about = "Use an AI review agent to assess whether a package graph looks safe to use")]
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
    #[command(about = "Check configured dependencies for newer tags or branch head changes")]
    Outdated {
        #[arg(
            long,
            help = "Emit machine-readable JSON instead of human-readable text"
        )]
        json: bool,
    },
    #[command(about = "Update configured dependencies and resync managed outputs")]
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
        about = "Check for or install a newer nodus CLI when the install method is supported"
    )]
    Upgrade {
        #[arg(
            long,
            help = "Check whether a newer nodus CLI release is available without installing it"
        )]
        check: bool,
    },
    #[command(about = "Relay linked managed edits back into a maintainer checkout")]
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
    #[command(about = "Create a minimal nodus.toml and example skill")]
    Init {
        #[arg(
            long = "dry-run",
            help = "Preview project changes without writing to the project or linked repo; may still populate the shared store to compute the result"
        )]
        dry_run: bool,
    },
    #[command(about = "Resolve dependencies and write managed runtime outputs")]
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
    #[command(about = "Generate shell completion scripts")]
    Completion {
        #[arg(value_enum, help = "Shell to generate completions for")]
        shell: Shell,
    },
    #[command(about = "Validate lockfile, shared store, and managed output consistency")]
    Doctor {
        #[arg(
            long,
            help = "Emit machine-readable JSON instead of human-readable text"
        )]
        json: bool,
    },
}
