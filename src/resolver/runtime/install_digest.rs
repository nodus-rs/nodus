//! On-disk re-computation of v10 per-package `install_digest` values.
//!
//! Slice 3 stamped each `LockedPackage` with `install_digest = blake3:...`,
//! computed from the planned output bytes attributed to that package. Slice 4
//! adds the symmetric helper: given a package's recorded `owned_*` rules,
//! re-derive the digest from what's actually on disk so `nodus sync` can take
//! a fast-path exit when the disk matches the lockfile.
//!
//! The hashing shape MUST match
//! [`crate::resolver::runtime::install_digests_by_package`] exactly:
//!
//! - entries are `(path_string_relative_to_project_root, file_bytes)`,
//! - paths are sorted ascending by their `display_path` string form,
//! - the hash is fed via [`crate::hashing::content_digest`] (BLAKE3).
//!
//! Drift posture (caller contract):
//!
//! - Missing `owned_files` entry → return `Ok(None)` (drift; caller falls back
//!   to full sync).
//! - Missing `owned_subtrees` directory → treated as zero files in that
//!   subtree. An expected-non-empty subtree therefore produces a different
//!   digest than the recorded one, which the caller compares against and
//!   treats as drift. (Returning `None` here would conflate "empty subtree as
//!   designed" with "package owns nothing on disk".)
//! - Missing `owned_prefixes` directory → treated as zero matching files.
//!   Same comparison-based drift semantics as subtrees.
//! - Walk errors (permission denied, symlink loop, etc.) → propagated as
//!   `Err`, since they indicate a genuinely broken workspace.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use walkdir::WalkDir;

use crate::hashing::content_digest;
use crate::lockfile::{LockedPackage, Lockfile};
use crate::paths::display_path;

/// Compute the `install_digest` (`"blake3:<hex>"`) for `package` by reading the
/// paths it claims (`owned_files` / `owned_subtrees` / `owned_prefixes`) from
/// disk, hashing them in the same canonical shape as
/// `install_digests_by_package`.
///
/// Returns `Ok(None)` when an `owned_files` entry is missing from disk — the
/// caller treats that as drift and falls back to a full sync. Other shapes of
/// drift (extra files in a subtree, mutated bytes, prefix-rule files missing
/// from a present directory) produce a digest that simply does not equal the
/// recorded value; the caller's `Some(disk) == Some(recorded)` comparison
/// handles those.
pub(crate) fn install_digest_from_disk(
    project_root: &Path,
    package: &LockedPackage,
) -> Result<Option<String>> {
    // BTreeMap gives us deterministic path ordering (matches
    // `install_digests_by_package` which seeds a `BTreeMap<PathBuf, _>`).
    let mut entries: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();

    for owned in &package.owned_files {
        // Reject lockfile entries that escape the project root before we
        // touch the disk. A hand-edited or malicious lockfile carrying an
        // absolute path (e.g. "/etc/passwd") or `..` segments would
        // otherwise let the fast-path probe read arbitrary files outside
        // the workspace, because `project_root.join(absolute)` discards
        // `project_root`.
        let relative = Lockfile::validate_managed_relative(owned, project_root)
            .with_context(|| {
                format!(
                    "package `{}` declares owned file `{}` outside the project root",
                    package.alias, owned
                )
            })?
            .to_path_buf();
        let absolute = project_root.join(&relative);
        let contents = match std::fs::read(&absolute) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // An exact file we claim to own is absent on disk. The caller
                // should treat this as drift, not as "empty contribution".
                return Ok(None);
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to read owned file {} for package `{}`",
                        absolute.display(),
                        package.alias
                    )
                });
            }
        };
        entries.insert(relative, contents);
    }

    for subtree in &package.owned_subtrees {
        let subtree_relative = Lockfile::validate_managed_relative(subtree, project_root)
            .with_context(|| {
                format!(
                    "package `{}` declares owned subtree `{}` outside the project root",
                    package.alias, subtree
                )
            })?
            .to_path_buf();
        let subtree_abs = project_root.join(&subtree_relative);
        collect_subtree_files(project_root, &subtree_abs, &mut entries)?;
    }

    for rule in &package.owned_prefixes {
        let dir_relative = Lockfile::validate_managed_relative(&rule.dir, project_root)
            .with_context(|| {
                format!(
                    "package `{}` declares owned-prefix dir `{}` outside the project root",
                    package.alias, rule.dir
                )
            })?
            .to_path_buf();
        let dir_abs = project_root.join(&dir_relative);
        collect_prefix_files(project_root, &dir_abs, &rule.prefix, &mut entries)?;
    }

    let entries_for_digest: Vec<(String, Vec<u8>)> = entries
        .into_iter()
        .map(|(path, contents)| (display_path(&path), contents))
        .collect();
    let digest_input: Vec<(&str, &[u8])> = entries_for_digest
        .iter()
        .map(|(path, contents)| (path.as_str(), contents.as_slice()))
        .collect();
    Ok(Some(content_digest(&digest_input)))
}

/// Walk every regular file under `subtree_abs` and insert
/// `(project-root-relative path, bytes)` into `entries`. A missing or
/// non-directory `subtree_abs` is treated as zero files — see module docs for
/// the drift rationale.
fn collect_subtree_files(
    project_root: &Path,
    subtree_abs: &Path,
    entries: &mut BTreeMap<PathBuf, Vec<u8>>,
) -> Result<()> {
    if !subtree_abs.exists() {
        return Ok(());
    }
    if !subtree_abs.is_dir() {
        // A subtree root that's a file on disk is a structural anomaly we
        // surface as "not contributing" — the digest comparison will fail and
        // the caller will run a full sync to repair it.
        return Ok(());
    }
    for entry in WalkDir::new(subtree_abs)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry
            .with_context(|| format!("failed to walk owned subtree {}", subtree_abs.display()))?;
        let file_type = entry.file_type();
        if !file_type.is_file() {
            continue;
        }
        if entry.path_is_symlink() {
            continue;
        }
        let absolute = entry.path();
        let relative = absolute
            .strip_prefix(project_root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| absolute.to_path_buf());
        let contents = std::fs::read(absolute)
            .with_context(|| format!("failed to read owned subtree file {}", absolute.display()))?;
        entries.insert(relative, contents);
    }
    Ok(())
}

/// Read the direct children of `dir_abs`, keep those whose `file_name` starts
/// with `prefix`, and insert their `(project-root-relative path, bytes)` into
/// `entries`. Mirrors `OwnedSet::contains` prefix semantics (direct children
/// only, strict prefix match, no globbing).
fn collect_prefix_files(
    project_root: &Path,
    dir_abs: &Path,
    prefix: &str,
    entries: &mut BTreeMap<PathBuf, Vec<u8>>,
) -> Result<()> {
    if !dir_abs.exists() {
        return Ok(());
    }
    if !dir_abs.is_dir() {
        return Ok(());
    }
    let read_dir = std::fs::read_dir(dir_abs)
        .with_context(|| format!("failed to read owned prefix dir {}", dir_abs.display()))?;
    for entry in read_dir {
        let entry = entry
            .with_context(|| format!("failed to iterate owned prefix dir {}", dir_abs.display()))?;
        let metadata = entry
            .file_type()
            .with_context(|| format!("failed to stat {}", entry.path().display()))?;
        if !metadata.is_file() || metadata.is_symlink() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(name_str) = file_name.to_str() else {
            continue;
        };
        if !name_str.starts_with(prefix) {
            continue;
        }
        let absolute = entry.path();
        let relative = absolute
            .strip_prefix(project_root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| absolute.clone());
        let contents = std::fs::read(&absolute)
            .with_context(|| format!("failed to read owned prefix file {}", absolute.display()))?;
        entries.insert(relative, contents);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockfile::{LockedSource, OwnedPrefix};
    use std::fs;
    use tempfile::TempDir;

    fn minimal_package(alias: &str) -> LockedPackage {
        LockedPackage {
            alias: alias.into(),
            name: alias.into(),
            version_tag: None,
            source: LockedSource {
                kind: "path".into(),
                path: Some(".".into()),
                url: None,
                tag: None,
                branch: None,
                rev: None,
            },
            digest: "blake3:abc".into(),
            selected_components: None,
            skills: vec![],
            agents: vec![],
            rules: vec![],
            commands: vec![],
            mcp_servers: vec![],
            dependencies: vec![],
            capabilities: vec![],
            owned_subtrees: vec![],
            owned_prefixes: vec![],
            owned_files: vec![],
            install_digest: None,
        }
    }

    #[test]
    fn empty_package_digest_matches_content_digest_of_empty_slice() {
        let temp = TempDir::new().unwrap();
        let pkg = minimal_package("empty");

        let digest = install_digest_from_disk(temp.path(), &pkg)
            .unwrap()
            .expect("empty package contributes empty entries, not None");

        assert_eq!(digest, content_digest(&[]));
    }

    #[test]
    fn missing_owned_file_returns_none() {
        let temp = TempDir::new().unwrap();
        let mut pkg = minimal_package("foo");
        pkg.owned_files = vec!["missing.txt".into()];

        let digest = install_digest_from_disk(temp.path(), &pkg).unwrap();

        assert!(
            digest.is_none(),
            "expected Ok(None) for missing owned file, got {digest:?}"
        );
    }

    #[test]
    fn owned_file_contributes_to_digest() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("hello.txt"), b"hi").unwrap();
        let mut pkg = minimal_package("foo");
        pkg.owned_files = vec!["hello.txt".into()];

        let digest = install_digest_from_disk(temp.path(), &pkg)
            .unwrap()
            .unwrap();

        let expected = content_digest(&[("hello.txt", b"hi")]);
        assert_eq!(digest, expected);
    }

    #[test]
    fn owned_subtree_walks_recursively_and_sorts_by_path() {
        let temp = TempDir::new().unwrap();
        let subtree = temp.path().join(".nodus/packages/foo");
        fs::create_dir_all(subtree.join("nested")).unwrap();
        fs::write(subtree.join("b.txt"), b"BB").unwrap();
        fs::write(subtree.join("a.txt"), b"AA").unwrap();
        fs::write(subtree.join("nested/c.txt"), b"CC").unwrap();

        let mut pkg = minimal_package("foo");
        pkg.owned_subtrees = vec![".nodus/packages/foo".into()];

        let digest = install_digest_from_disk(temp.path(), &pkg)
            .unwrap()
            .unwrap();

        let expected = content_digest(&[
            (".nodus/packages/foo/a.txt", b"AA" as &[u8]),
            (".nodus/packages/foo/b.txt", b"BB"),
            (".nodus/packages/foo/nested/c.txt", b"CC"),
        ]);
        assert_eq!(digest, expected);
    }

    #[test]
    fn missing_subtree_directory_contributes_zero_files() {
        let temp = TempDir::new().unwrap();
        let mut pkg = minimal_package("foo");
        pkg.owned_subtrees = vec![".nodus/packages/foo".into()];

        let digest = install_digest_from_disk(temp.path(), &pkg)
            .unwrap()
            .unwrap();

        assert_eq!(digest, content_digest(&[]));
    }

    #[test]
    fn owned_prefix_matches_direct_children_only() {
        let temp = TempDir::new().unwrap();
        let hooks_dir = temp.path().join(".claude/hooks");
        fs::create_dir_all(hooks_dir.join("subdir")).unwrap();
        fs::write(hooks_dir.join("nodus-hook-a.sh"), b"A").unwrap();
        fs::write(hooks_dir.join("nodus-hook-b.sh"), b"B").unwrap();
        fs::write(hooks_dir.join("user-thing.sh"), b"USER").unwrap();
        fs::write(hooks_dir.join("subdir/nodus-hook-nested.sh"), b"NESTED").unwrap();

        let mut pkg = minimal_package("hooks");
        pkg.owned_prefixes = vec![OwnedPrefix {
            dir: ".claude/hooks".into(),
            prefix: "nodus-hook-".into(),
        }];

        let digest = install_digest_from_disk(temp.path(), &pkg)
            .unwrap()
            .unwrap();

        // Only the two direct children with the prefix contribute. The user
        // file and the nested file under `subdir/` are excluded.
        let expected = content_digest(&[
            (".claude/hooks/nodus-hook-a.sh", b"A" as &[u8]),
            (".claude/hooks/nodus-hook-b.sh", b"B"),
        ]);
        assert_eq!(digest, expected);
    }

    #[test]
    fn mutated_owned_file_changes_digest() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("hello.txt"), b"hi").unwrap();
        let mut pkg = minimal_package("foo");
        pkg.owned_files = vec!["hello.txt".into()];

        let original = install_digest_from_disk(temp.path(), &pkg)
            .unwrap()
            .unwrap();

        fs::write(temp.path().join("hello.txt"), b"changed").unwrap();
        let mutated = install_digest_from_disk(temp.path(), &pkg)
            .unwrap()
            .unwrap();

        assert_ne!(original, mutated);
    }

    #[test]
    fn rejects_owned_files_entry_with_absolute_path() {
        let temp = TempDir::new().unwrap();
        let mut pkg = minimal_package("foo");
        // Absolute path that would otherwise slip past `project_root.join`
        // (Path::join drops the LHS when the RHS is absolute) and let the
        // probe try to read /etc/passwd. We never want to touch the disk
        // outside the workspace.
        pkg.owned_files = vec!["/etc/passwd".into()];

        let err = install_digest_from_disk(temp.path(), &pkg).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("escapes project root")
                || message.contains("outside the project root"),
            "expected escape diagnostic, got: {message}"
        );
    }

    #[test]
    fn rejects_owned_subtrees_entry_with_parent_dir_segment() {
        let temp = TempDir::new().unwrap();
        let mut pkg = minimal_package("foo");
        pkg.owned_subtrees = vec!["../../etc".into()];

        let err = install_digest_from_disk(temp.path(), &pkg).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("escapes project root")
                || message.contains("outside the project root"),
            "expected escape diagnostic, got: {message}"
        );
    }

    #[test]
    fn rejects_owned_prefix_dir_with_absolute_path() {
        let temp = TempDir::new().unwrap();
        let mut pkg = minimal_package("foo");
        pkg.owned_prefixes = vec![OwnedPrefix {
            dir: "/etc".into(),
            prefix: "passwd".into(),
        }];

        let err = install_digest_from_disk(temp.path(), &pkg).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("escapes project root")
                || message.contains("outside the project root"),
            "expected escape diagnostic, got: {message}"
        );
    }

    #[test]
    fn extra_file_in_owned_subtree_changes_digest() {
        let temp = TempDir::new().unwrap();
        let subtree = temp.path().join(".nodus/packages/foo");
        fs::create_dir_all(&subtree).unwrap();
        fs::write(subtree.join("a.txt"), b"AA").unwrap();

        let mut pkg = minimal_package("foo");
        pkg.owned_subtrees = vec![".nodus/packages/foo".into()];

        let baseline = install_digest_from_disk(temp.path(), &pkg)
            .unwrap()
            .unwrap();

        fs::write(subtree.join("b.txt"), b"BB").unwrap();
        let after_extra = install_digest_from_disk(temp.path(), &pkg)
            .unwrap()
            .unwrap();

        assert_ne!(baseline, after_extra);
    }
}
