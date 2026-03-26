mod adapters;
mod cache;
mod cli;
mod execution;
mod git;
mod info;
mod list;
mod local_config;
mod lockfile;
mod manifest;
mod outdated;
mod paths;
mod relay;
mod report;
mod resolver;
mod review;
mod selection;
mod store;
mod update;
mod update_checker;

fn main() -> std::process::ExitCode {
    cli::run()
}
