use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(author, version, about = "Agen manages project-scoped agent packages", long_about = None)]
struct Cli {
    #[arg(long, global = true)]
    cache_path: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Add {
        url: String,
        #[arg(long)]
        tag: Option<String>,
    },
    Uninstall {
        package: String,
    },
    Init,
    Sync {
        #[arg(long)]
        locked: bool,
        #[arg(long = "allow-high-sensitivity")]
        allow_high_sensitivity: bool,
    },
    Doctor,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let cache_root = crate::cache::resolve_cache_root(cli.cache_path.as_deref())?;

    match cli.command {
        Command::Add { url, tag } => crate::git::add_dependency(&cache_root, &url, tag.as_deref()),
        Command::Uninstall { package } => crate::git::remove_dependency(&cache_root, &package),
        Command::Init => crate::manifest::scaffold_init(),
        Command::Sync {
            locked,
            allow_high_sensitivity,
        } => crate::resolver::sync(&cache_root, locked, allow_high_sensitivity),
        Command::Doctor => crate::resolver::doctor(&cache_root),
    }
}
