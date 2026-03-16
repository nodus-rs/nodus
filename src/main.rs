mod adapters;
mod cache;
mod cli;
mod git;
mod lockfile;
mod manifest;
mod resolver;
mod selection;
mod store;

fn main() -> anyhow::Result<()> {
    cli::run()
}
