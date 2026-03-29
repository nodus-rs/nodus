use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rayon::prelude::*;
use tempfile::{Builder, NamedTempFile};

pub const STORE_ROOT: &str = "store/sha256";

#[derive(Debug, Clone)]
pub struct StoredPackage {
    pub digest: String,
    pub snapshot_root: PathBuf,
}

pub trait SnapshotSource: Sync {
    fn digest(&self) -> &str;
    fn package_root(&self) -> &Path;
    fn package_files(&self) -> Result<Vec<PathBuf>>;
    fn read_package_file(&self, path: &Path) -> Result<Vec<u8>>;
}

pub fn snapshot_packages<T: SnapshotSource>(
    cache_root: &Path,
    packages: &[T],
) -> Result<Vec<StoredPackage>> {
    let store_root = cache_root.join(STORE_ROOT);
    fs::create_dir_all(&store_root)
        .with_context(|| format!("failed to create store root {}", store_root.display()))?;

    packages
        .par_iter()
        .map(|package| {
            let snapshot_root = snapshot_package(&store_root, package)?;
            Ok(StoredPackage {
                digest: package.digest().to_string(),
                snapshot_root,
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect()
}

pub fn snapshot_path(cache_root: &Path, digest: &str) -> Result<PathBuf> {
    Ok(cache_root
        .join(STORE_ROOT)
        .join(digest_directory_name(digest)?))
}

pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("cannot atomically write {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create parent directory {}", parent.display()))?;

    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temp file in {}", parent.display()))?;
    temp.write_all(contents)
        .with_context(|| format!("failed to write temp file for {}", path.display()))?;
    temp.flush()
        .with_context(|| format!("failed to flush temp file for {}", path.display()))?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| {
            format!(
                "failed to persist atomically written file to {}",
                path.display()
            )
        })?;

    Ok(())
}

fn snapshot_package<T: SnapshotSource>(store_root: &Path, package: &T) -> Result<PathBuf> {
    let digest_dir_name = digest_directory_name(package.digest())?;
    let digest_dir = store_root.join(digest_dir_name);
    let files = package.package_files()?;
    if digest_dir.exists() {
        if snapshot_is_complete(&digest_dir, package.package_root(), &files)? {
            return Ok(digest_dir);
        }

        match fs::remove_dir_all(&digest_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to remove incomplete snapshot {}",
                        digest_dir.display()
                    )
                });
            }
        }
    }

    if digest_dir.exists() {
        return Ok(digest_dir);
    }

    let staging = Builder::new()
        .prefix(&format!(".tmp-{}-", digest_dir_name.replace('/', "_")))
        .tempdir_in(store_root)
        .with_context(|| {
            format!(
                "failed to create staging dir for snapshot {}",
                digest_dir.display()
            )
        })?;
    let staging_root = staging.path().to_path_buf();

    for file in files {
        let relative = file.strip_prefix(package.package_root()).with_context(|| {
            format!("failed to make {} relative to package root", file.display())
        })?;
        let target = staging_root.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create snapshot directory {}", parent.display())
            })?;
        }
        let contents = package
            .read_package_file(&file)
            .with_context(|| format!("failed to read {} for snapshot", file.display()))?;
        write_atomic(&target, &contents).with_context(|| {
            format!(
                "failed to copy {} into snapshot {}",
                file.display(),
                target.display()
            )
        })?;
    }

    if let Some(parent) = digest_dir.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create store parent {}", parent.display()))?;
    }

    match fs::rename(&staging_root, &digest_dir) {
        Ok(()) => {
            let _ = staging.keep();
            Ok(digest_dir)
        }
        Err(_) if digest_dir.exists() => Ok(digest_dir),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to promote snapshot {} into {}",
                staging_root.display(),
                digest_dir.display()
            )
        }),
    }
}

fn snapshot_is_complete(
    snapshot_root: &Path,
    package_root: &Path,
    files: &[PathBuf],
) -> Result<bool> {
    for file in files {
        let relative = file.strip_prefix(package_root).with_context(|| {
            format!("failed to make {} relative to package root", file.display())
        })?;
        if !snapshot_root.join(relative).is_file() {
            return Ok(false);
        }
    }

    Ok(true)
}

fn digest_directory_name(digest: &str) -> Result<&str> {
    digest
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow::anyhow!("unsupported digest format `{digest}`"))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::TempDir;

    use super::*;
    use crate::report::Reporter;
    use crate::resolver::resolve_project_for_sync;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::File::create(path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn snapshots_package_contents_into_the_local_store() {
        let temp = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        write_file(
            &temp.path().join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Example.\n---\n# Review\n",
        );

        let reporter = Reporter::silent();
        let resolution = resolve_project_for_sync(temp.path(), cache.path(), &reporter).unwrap();
        let stored = snapshot_packages(cache.path(), &resolution.packages).unwrap();

        assert_eq!(stored.len(), 1);
        assert!(
            stored[0]
                .snapshot_root
                .starts_with(cache.path().join(STORE_ROOT))
        );
        assert!(!stored[0].snapshot_root.starts_with(temp.path()));
        assert!(
            stored[0]
                .snapshot_root
                .join("skills/review/SKILL.md")
                .exists()
        );
    }

    #[test]
    fn recreates_incomplete_snapshots() {
        let temp = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        write_file(
            &temp.path().join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: Example.\n---\n# Review\n",
        );
        write_file(
            &temp.path().join("rules/common/coding-style.md"),
            "be consistent\n",
        );

        let reporter = Reporter::silent();
        let resolution = resolve_project_for_sync(temp.path(), cache.path(), &reporter).unwrap();
        let stored = snapshot_packages(cache.path(), &resolution.packages).unwrap();
        let snapshot_root = &stored[0].snapshot_root;

        fs::remove_file(snapshot_root.join("rules/common/coding-style.md")).unwrap();
        let rebuilt = snapshot_packages(cache.path(), &resolution.packages).unwrap();

        assert_eq!(rebuilt[0].snapshot_root, *snapshot_root);
        assert!(
            rebuilt[0]
                .snapshot_root
                .join("rules/common/coding-style.md")
                .exists()
        );
    }

    #[test]
    fn reuses_the_same_snapshot_for_duplicate_package_digests() {
        let temp = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        write_file(
            &temp.path().join("nodus.toml"),
            r#"
[dependencies]
alpha = { path = "vendor/alpha" }
beta = { path = "vendor/beta" }
"#,
        );
        write_file(
            &temp.path().join("vendor/alpha/skills/shared/SKILL.md"),
            "---\nname: Shared\ndescription: Example.\n---\n# Shared\n",
        );
        write_file(
            &temp.path().join("vendor/beta/skills/shared/SKILL.md"),
            "---\nname: Shared\ndescription: Example.\n---\n# Shared\n",
        );

        let reporter = Reporter::silent();
        let resolution = resolve_project_for_sync(temp.path(), cache.path(), &reporter).unwrap();
        let stored = snapshot_packages(cache.path(), &resolution.packages).unwrap();

        let mut dependency_digests = resolution
            .packages
            .iter()
            .filter(|package| matches!(package.alias.as_str(), "alpha" | "beta"))
            .map(|package| package.digest.clone())
            .collect::<Vec<_>>();
        dependency_digests.sort();
        dependency_digests.dedup();
        assert_eq!(dependency_digests.len(), 1);

        let dependency_snapshots = stored
            .iter()
            .filter(|package| package.digest == dependency_digests[0])
            .map(|package| package.snapshot_root.clone())
            .collect::<Vec<_>>();
        assert_eq!(dependency_snapshots.len(), 2);
        assert_eq!(dependency_snapshots[0], dependency_snapshots[1]);
        assert!(
            dependency_snapshots[0]
                .join("skills/shared/SKILL.md")
                .is_file()
        );
    }

    #[test]
    fn atomically_writes_files() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("nested/output.txt");

        write_atomic(&target, b"hello").unwrap();

        assert_eq!(fs::read_to_string(target).unwrap(), "hello");
    }
}
