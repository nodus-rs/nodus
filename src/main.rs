mod adapters;
mod cache;
mod cli;
mod git;
mod lockfile;
mod manifest;
mod report;
mod resolver;
mod selection;
mod store;

fn main() -> std::process::ExitCode {
    cli::run()
}
