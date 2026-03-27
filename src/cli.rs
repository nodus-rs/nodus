mod args;
mod entry;
mod handlers;
mod output;
mod router;
#[cfg(test)]
mod tests;

pub fn run() -> std::process::ExitCode {
    entry::run()
}
