use std::path::Path;

use anyhow::Result;

use crate::lockfile::{LOCKFILE_NAME, LockedPackage, Lockfile};
use crate::manifest::DependencyKind;

pub(crate) fn load_lockfile(cwd: &Path) -> Result<Option<Lockfile>> {
    let path = cwd.join(LOCKFILE_NAME);
    if path.exists() {
        Ok(Some(Lockfile::read(&path)?))
    } else {
        Ok(None)
    }
}

pub(crate) fn find_locked_package<'a>(
    lockfile: &'a Lockfile,
    alias: &str,
) -> Option<&'a LockedPackage> {
    lockfile
        .packages
        .iter()
        .find(|package| package.alias == alias)
}

pub(crate) fn locked_rev(lockfile: Option<&Lockfile>, alias: &str) -> Option<String> {
    lockfile
        .and_then(|lockfile| find_locked_package(lockfile, alias))
        .and_then(|package| package.source.rev.clone())
}

pub(crate) fn locked_tag(lockfile: Option<&Lockfile>, alias: &str) -> Option<String> {
    lockfile
        .and_then(|lockfile| find_locked_package(lockfile, alias))
        .and_then(|package| package.source.tag.clone())
}

pub(crate) fn display_dependency_alias(alias: &str, kind: DependencyKind) -> String {
    if kind.is_dev() {
        format!("{alias} [dev]")
    } else {
        alias.to_string()
    }
}

pub(crate) fn short_identifier(value: &str) -> String {
    value.chars().take(12).collect()
}
