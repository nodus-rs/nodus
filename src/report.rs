use std::cell::RefCell;
use std::io::{self, Write};

use anstream::{AutoStream, ColorChoice};
use anstyle::{AnsiColor, Style};
use anyhow::Error;

const LABEL_WIDTH: usize = 12;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

impl ColorMode {
    fn choice(self) -> ColorChoice {
        match self {
            Self::Auto => ColorChoice::Auto,
            Self::Always => ColorChoice::Always,
            Self::Never => ColorChoice::Never,
        }
    }
}

pub struct Reporter {
    writer: RefCell<Box<dyn Write>>,
    color_enabled: bool,
}

impl Reporter {
    pub fn stderr() -> Self {
        let stream = AutoStream::new(io::stderr().lock(), ColorMode::Auto.choice());
        let color_enabled = !matches!(stream.current_choice(), ColorChoice::Never);
        Self {
            writer: RefCell::new(Box::new(stream)),
            color_enabled,
        }
    }

    pub fn sink(mode: ColorMode, writer: impl Write + 'static) -> Self {
        Self {
            writer: RefCell::new(Box::new(writer)),
            color_enabled: matches!(mode, ColorMode::Always),
        }
    }

    #[allow(dead_code)]
    pub fn silent() -> Self {
        Self::sink(ColorMode::Never, io::sink())
    }

    pub fn status(&self, label: &str, message: impl std::fmt::Display) -> anyhow::Result<()> {
        let padded = format!("{label:>LABEL_WIDTH$}");
        self.write_line(&format!(
            "{} {message}",
            self.styled(&padded, Self::status_style()),
        ))
    }

    pub fn finish(&self, message: impl std::fmt::Display) -> anyhow::Result<()> {
        let padded = format!("{:>LABEL_WIDTH$}", "Finished");
        self.write_line(&format!(
            "{} {message}",
            self.styled(&padded, Self::finish_style()),
        ))
    }

    pub fn warning(&self, message: impl std::fmt::Display) -> anyhow::Result<()> {
        self.write_line(&format!(
            "{} {message}",
            self.styled("warning:", Self::warning_style()),
        ))
    }

    pub fn note(&self, message: impl std::fmt::Display) -> anyhow::Result<()> {
        self.write_line(&format!(
            "{} {message}",
            self.styled("note:", Self::note_style()),
        ))
    }

    pub fn line(&self, message: impl std::fmt::Display) -> anyhow::Result<()> {
        self.write_line(&message.to_string())
    }

    pub fn color_enabled(&self) -> bool {
        self.color_enabled
    }

    pub fn paint(&self, value: &str, style: Style) -> String {
        self.styled(value, style)
    }

    pub fn error(&self, error: &Error) -> anyhow::Result<()> {
        let mut chain = error.chain();
        if let Some(head) = chain.next() {
            self.write_line(&format!(
                "{} {head}",
                self.styled("error:", Self::error_style()),
            ))?;
        }

        let causes = chain.map(|cause| cause.to_string()).collect::<Vec<_>>();
        if !causes.is_empty() {
            self.write_line("Caused by:")?;
            for (index, cause) in causes.iter().enumerate() {
                self.write_line(&format!("  {index}: {cause}"))?;
            }
        }

        Ok(())
    }

    fn write_line(&self, line: &str) -> anyhow::Result<()> {
        let mut writer = self.writer.borrow_mut();
        writeln!(writer, "{line}").map_err(Into::into)
    }

    fn styled(&self, value: &str, style: Style) -> String {
        if self.color_enabled {
            format!("{style}{value}{style:#}")
        } else {
            value.to_string()
        }
    }

    fn status_style() -> Style {
        Style::new().bold().fg_color(Some(AnsiColor::Green.into()))
    }

    fn finish_style() -> Style {
        Self::status_style()
    }

    fn warning_style() -> Style {
        Style::new().bold().fg_color(Some(AnsiColor::Yellow.into()))
    }

    fn note_style() -> Style {
        Style::new().bold().fg_color(Some(AnsiColor::Cyan.into()))
    }

    fn error_style() -> Style {
        Style::new().bold().fg_color(Some(AnsiColor::Red.into()))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn renders_plain_status_output_when_color_is_disabled() {
        let buffer = SharedBuffer::default();
        let reporter = Reporter::sink(ColorMode::Never, buffer.clone());

        reporter.status("Checking", "project graph").unwrap();

        assert_eq!(buffer.contents(), "    Checking project graph\n");
    }

    #[test]
    fn renders_colored_output_when_color_is_forced() {
        let buffer = SharedBuffer::default();
        let reporter = Reporter::sink(ColorMode::Always, buffer.clone());

        reporter.warning("be careful").unwrap();

        let output = buffer.contents();
        assert!(output.contains("\u{1b}["));
        assert!(output.contains("warning:"));
        assert!(output.contains("be careful"));
    }

    #[test]
    fn renders_finish_and_note_output() {
        let buffer = SharedBuffer::default();
        let reporter = Reporter::sink(ColorMode::Never, buffer.clone());

        reporter.note("using shared checkout").unwrap();
        reporter.finish("1 package in 0.01s").unwrap();

        assert_eq!(
            buffer.contents(),
            "note: using shared checkout\n    Finished 1 package in 0.01s\n"
        );
    }

    #[test]
    fn renders_plain_lines_without_prefixes() {
        let buffer = SharedBuffer::default();
        let reporter = Reporter::sink(ColorMode::Never, buffer.clone());

        reporter.line("hello world").unwrap();

        assert_eq!(buffer.contents(), "hello world\n");
    }

    #[test]
    fn renders_error_chains() {
        let buffer = SharedBuffer::default();
        let reporter = Reporter::sink(ColorMode::Never, buffer.clone());
        let error = anyhow::anyhow!("outer").context("middle").context("inner");

        reporter.error(&error).unwrap();

        assert_eq!(
            buffer.contents(),
            "error: inner\nCaused by:\n  0: middle\n  1: outer\n"
        );
    }
}
