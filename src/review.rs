#[cfg(any(target_os = "macos", target_os = "linux"))]
mod runtime;

#[allow(unused_imports)]
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use runtime::{ReviewProvider, ReviewRequest, ReviewSummary, review_package_in_dir};

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
use std::path::Path;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
use anyhow::{Result, bail};

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
use clap::ValueEnum;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
use crate::report::Reporter;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReviewProvider {
    #[value(name = "openai")]
    Openai,
    #[value(name = "anthropic")]
    Anthropic,
    #[value(name = "gemini")]
    Gemini,
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl std::fmt::Display for ReviewProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Openai => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::Gemini => "Gemini",
        };
        formatter.write_str(name)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[derive(Debug, Clone)]
pub struct ReviewSummary {
    pub package_count: usize,
    pub provider: ReviewProvider,
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub struct ReviewRequest<'a> {
    pub package: &'a str,
    pub tag: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub provider: ReviewProvider,
    pub model: Option<&'a str>,
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn review_package_in_dir(
    _cwd: &Path,
    _cache_root: &Path,
    _request: ReviewRequest<'_>,
    _reporter: &Reporter,
) -> Result<ReviewSummary> {
    bail!("`nodus review` is currently supported only on macOS and Linux");
}
