pub(super) mod dependency;
pub(super) mod project;
pub(super) mod query;
pub(super) mod system;

use std::path::Path;

use crate::report::Reporter;

pub(crate) struct CommandContext<'a> {
    pub(crate) cwd: &'a Path,
    pub(crate) cache_root: &'a Path,
    pub(crate) reporter: &'a Reporter,
}
