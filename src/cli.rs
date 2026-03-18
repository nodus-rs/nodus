mod args;
mod dispatch;
#[cfg(test)]
mod tests;

pub fn run() -> std::process::ExitCode {
    dispatch::run()
}
