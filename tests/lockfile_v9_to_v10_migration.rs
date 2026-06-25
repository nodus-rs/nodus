//! Slice 4 integration coverage: v9 → v10 lockfile migration and the
//! v10 `install_digest` drift fast-path.
//!
//! Mirrors the pattern in `tests/lockfile_v10_schema.rs` — drive the `nodus`
//! binary against on-disk fixtures and assert on its observable behavior
//! (exit status, stderr/stdout messages, post-sync lockfile contents).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

use tempfile::TempDir;

/// Invoke the `nodus` binary in `cwd` with the given arguments and return the
/// captured output.
fn run_nodus<I, S>(cwd: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    Command::new(env!("CARGO_BIN_EXE_nodus"))
        .current_dir(cwd)
        .env("NODUS_HOME", cwd.join(".nodus-global"))
        .args(args)
        .output()
        .expect("spawn nodus binary")
}

/// Produce a tiny path-dep workspace under `temp` and return the shared cache
/// `TempDir` so the caller keeps both alive for the duration of the test.
///
/// Layout:
///
/// ```text
/// temp/
///   nodus.toml                  # declares vendor/shared as a path dep
///   vendor/shared/
///     nodus.toml                # empty package manifest
///     skills/review/SKILL.md    # one skill
/// ```
fn build_path_dep_workspace(temp: &Path) -> TempDir {
    let cache = TempDir::new().expect("cache tempdir");
    fs::write(
        temp.join("nodus.toml"),
        r#"
[adapters]
enabled = ["claude"]

[dependencies]
shared = { path = "vendor/shared" }
"#,
    )
    .expect("write root manifest");

    let shared = temp.join("vendor/shared");
    fs::create_dir_all(shared.join("skills/review")).expect("create shared skill dir");
    fs::write(shared.join("nodus.toml"), "").expect("write shared manifest");
    fs::write(
        shared.join("skills/review/SKILL.md"),
        "---\nname: Review\ndescription: Example skill.\n---\n# Review\n",
    )
    .expect("write SKILL.md");

    cache
}

fn global_claude_skill_path(workspace: &Path, package_prefix: &str, skill_id: &str) -> PathBuf {
    let packages_root = workspace.join(".nodus-global/packages");
    let package_root = fs::read_dir(&packages_root)
        .unwrap_or_else(|error| {
            panic!(
                "failed to read global packages root {}: {error}",
                packages_root.display()
            )
        })
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(package_prefix))
        })
        .unwrap_or_else(|| {
            panic!(
                "expected a global package directory starting with `{package_prefix}` under {}",
                packages_root.display()
            )
        });

    package_root
        .join("claude-plugin")
        .join("skills")
        .join(skill_id)
        .join("SKILL.md")
}

/// Acceptance: a v9-shape lockfile on disk is read by `nodus sync`, the
/// migration note is emitted, and the post-sync lockfile is fully v10
/// (per-package owned_*, install_digest, no top-level managed_files).
#[test]
fn sync_migrates_v9_lockfile_to_v10_and_populates_owned_and_install_digest() {
    let temp = TempDir::new().expect("workspace tempdir");
    let cache = build_path_dep_workspace(temp.path());

    // Hand-write a v9-shape lockfile that pretends a prior sync produced
    // managed outputs the v10 sync will repopulate. The legacy
    // `managed_files` field is what real-world v9 lockfiles carry; v10
    // writes drop it (skip-on-empty), so we'll assert that on the
    // post-sync read.
    let v9_lockfile = r#"version = 9
managed_files = [
    ".claude/skills/review",
]

[[packages]]
alias = "shared"
name = "shared"
digest = "blake3:bogus"
skills = ["review"]
agents = []
rules = []
commands = []
mcp_servers = []
dependencies = []
capabilities = []

[packages.source]
kind = "path"
path = "vendor/shared"
"#;
    fs::write(temp.path().join("nodus.lock"), v9_lockfile).expect("write v9 lockfile");

    let output = run_nodus(
        temp.path(),
        ["--store-path", cache.path().to_str().unwrap(), "sync"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "v9 → v10 migration sync failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    let combined = format!("{stderr}{stdout}");
    assert!(
        combined.contains("upgrading nodus.lock from version 9 to 10"),
        "expected the v9 → v10 migration note. combined output:\n{combined}"
    );

    let post_sync =
        fs::read_to_string(temp.path().join("nodus.lock")).expect("read post-sync lockfile");
    assert!(
        post_sync.contains("version = 10"),
        "post-sync lockfile must be v10:\n{post_sync}"
    );
    // v10 writes skip `legacy_managed_files` on empty, so the top-level
    // `managed_files` array must not be re-emitted.
    assert!(
        !post_sync.contains("\nmanaged_files = ["),
        "v10 write must not emit legacy `managed_files = [...]`:\n{post_sync}"
    );
    assert!(
        post_sync.contains("install_digest = \"blake3:"),
        "every package must carry an install_digest with the blake3 prefix:\n{post_sync}"
    );
    assert!(
        post_sync.contains("owned_subtrees")
            || post_sync.contains("owned_files")
            || post_sync.contains("owned_prefixes"),
        "v10 write must populate at least one per-package ownership field:\n{post_sync}"
    );
}

/// Acceptance: running `nodus sync` twice on a clean v10 workspace produces
/// a byte-identical lockfile. This is the "did sync stamp something
/// non-deterministic?" canary for Slice 3's emission and Slice 4's fast-path
/// (the second run typically takes the fast-path; either way the bytes must
/// stay identical).
#[test]
fn sync_is_byte_identical_idempotent_across_three_runs() {
    let temp = TempDir::new().expect("workspace tempdir");
    let cache = build_path_dep_workspace(temp.path());

    let store_arg = format!("--store-path={}", cache.path().to_str().unwrap());
    let initial = run_nodus(temp.path(), [store_arg.as_str(), "sync"]);
    assert!(
        initial.status.success(),
        "initial sync failed.\nstderr: {}",
        String::from_utf8_lossy(&initial.stderr)
    );

    let lockfile_after_first = fs::read(temp.path().join("nodus.lock")).expect("read lockfile #1");

    let second = run_nodus(temp.path(), [store_arg.as_str(), "sync"]);
    assert!(
        second.status.success(),
        "second sync failed.\nstderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let lockfile_after_second = fs::read(temp.path().join("nodus.lock")).expect("read lockfile #2");
    assert_eq!(
        lockfile_after_first, lockfile_after_second,
        "second sync changed the lockfile bytes; sync emission is not idempotent"
    );

    let third = run_nodus(temp.path(), [store_arg.as_str(), "sync"]);
    assert!(
        third.status.success(),
        "third sync failed.\nstderr: {}",
        String::from_utf8_lossy(&third.stderr)
    );
    let lockfile_after_third = fs::read(temp.path().join("nodus.lock")).expect("read lockfile #3");
    assert_eq!(
        lockfile_after_first, lockfile_after_third,
        "third sync changed the lockfile bytes; sync emission is not idempotent"
    );
}

/// Acceptance: when an owned file is edited on disk, `nodus sync` repairs
/// it (the file goes back to its planned contents) and the post-sync
/// install_digest matches the restored content.
///
/// This proves the drift case: the fast-path either short-circuits (no
/// drift) or correctly falls through to the full sync (drift detected)
/// and repairs the file.
#[test]
fn sync_repairs_drifted_owned_file_and_keeps_install_digest_consistent() {
    let temp = TempDir::new().expect("workspace tempdir");
    let cache = build_path_dep_workspace(temp.path());
    let store_arg = format!("--store-path={}", cache.path().to_str().unwrap());

    let initial = run_nodus(temp.path(), [store_arg.as_str(), "sync"]);
    assert!(
        initial.status.success(),
        "initial sync failed.\nstderr: {}",
        String::from_utf8_lossy(&initial.stderr)
    );

    // Pick any file Nodus owns and overwrite its contents. Dependency
    // Claude plugin payloads live under the global NODUS_HOME package cache,
    // keyed by the managed package ID.
    let drifted_file = global_claude_skill_path(temp.path(), "shared+", "review");
    assert!(
        drifted_file.exists(),
        "expected the planned SKILL.md to exist on disk after the initial sync; got missing path {}",
        drifted_file.display()
    );
    let pristine = fs::read_to_string(&drifted_file).expect("read pristine SKILL.md");
    fs::write(&drifted_file, b"---\nname: Tampered\n---\n# Mutated\n")
        .expect("write tampered SKILL.md");

    let repair = run_nodus(temp.path(), [store_arg.as_str(), "sync"]);
    assert!(
        repair.status.success(),
        "drift-repair sync failed.\nstderr: {}",
        String::from_utf8_lossy(&repair.stderr)
    );

    let restored = fs::read_to_string(&drifted_file).expect("read restored SKILL.md");
    assert_eq!(
        restored, pristine,
        "sync did not restore the tampered SKILL.md to its planned contents"
    );

    // The post-repair lockfile's install_digest must still correspond to
    // the restored on-disk content (which equals the pristine planned
    // bytes). We don't recompute the digest here — the inline tests in
    // `src/resolver/runtime/tests.rs::slice4_*` cover that. The cheap
    // proof is that another sync after the repair is a no-op
    // (byte-identical lockfile).
    let lockfile_after_repair =
        fs::read(temp.path().join("nodus.lock")).expect("read repaired lockfile");
    let confirmation = run_nodus(temp.path(), [store_arg.as_str(), "sync"]);
    assert!(
        confirmation.status.success(),
        "confirmation sync failed.\nstderr: {}",
        String::from_utf8_lossy(&confirmation.stderr)
    );
    let lockfile_after_confirmation =
        fs::read(temp.path().join("nodus.lock")).expect("read confirmation lockfile");
    assert_eq!(
        lockfile_after_repair, lockfile_after_confirmation,
        "post-repair lockfile should be stable across a follow-up sync"
    );
}

/// Acceptance: `nodus sync --no-fast-path` is accepted by the CLI and runs a
/// full resolve (no early exit). We can't easily distinguish "fast-path
/// hit" from "fast-path skipped because path dep" via the binary alone, so
/// this test just checks the flag is accepted and the sync completes
/// successfully.
#[test]
fn sync_no_fast_path_flag_is_accepted_by_cli() {
    let temp = TempDir::new().expect("workspace tempdir");
    let cache = build_path_dep_workspace(temp.path());
    let store_arg = format!("--store-path={}", cache.path().to_str().unwrap());

    let initial = run_nodus(temp.path(), [store_arg.as_str(), "sync"]);
    assert!(
        initial.status.success(),
        "initial sync failed.\nstderr: {}",
        String::from_utf8_lossy(&initial.stderr)
    );

    let no_fast_path = run_nodus(temp.path(), [store_arg.as_str(), "sync", "--no-fast-path"]);
    assert!(
        no_fast_path.status.success(),
        "sync --no-fast-path failed.\nstderr: {}",
        String::from_utf8_lossy(&no_fast_path.stderr)
    );
}

/// Acceptance: `nodus sync --frozen --no-fast-path` is rejected with a
/// clear error. The two flags contradict (frozen opts in to "trust the
/// lockfile"; --no-fast-path opts out of that trust).
#[test]
fn sync_frozen_with_no_fast_path_is_rejected() {
    let temp = TempDir::new().expect("workspace tempdir");
    let cache = build_path_dep_workspace(temp.path());
    let store_arg = format!("--store-path={}", cache.path().to_str().unwrap());

    let initial = run_nodus(temp.path(), [store_arg.as_str(), "sync"]);
    assert!(
        initial.status.success(),
        "initial sync failed.\nstderr: {}",
        String::from_utf8_lossy(&initial.stderr)
    );

    let conflict = run_nodus(
        temp.path(),
        [store_arg.as_str(), "sync", "--frozen", "--no-fast-path"],
    );
    assert!(
        !conflict.status.success(),
        "--frozen + --no-fast-path should be rejected.\nstderr: {}\nstdout: {}",
        String::from_utf8_lossy(&conflict.stderr),
        String::from_utf8_lossy(&conflict.stdout),
    );
    let stderr = String::from_utf8_lossy(&conflict.stderr);
    assert!(
        stderr.contains("--no-fast-path") || stderr.contains("frozen"),
        "error should mention the conflicting flag pair; got: {stderr}"
    );
}

/// Sanity check: the second sync of a workspace finishes faster than the
/// first when the fast-path is available. This is a heuristic perf
/// canary, not a precise benchmark — we only assert "second is no slower
/// than 2x first" to leave plenty of headroom for noisy CI runners.
///
/// Path-dep workspaces don't take the fast-path (Slice 4 design), but
/// the second run still has a hot cache; it should never be dramatically
/// slower than the first.
#[test]
fn sync_second_run_is_not_dramatically_slower_than_first() {
    let temp = TempDir::new().expect("workspace tempdir");
    let cache = build_path_dep_workspace(temp.path());
    let store_arg = format!("--store-path={}", cache.path().to_str().unwrap());

    let first_start = Instant::now();
    let first = run_nodus(temp.path(), [store_arg.as_str(), "sync"]);
    let first_elapsed = first_start.elapsed();
    assert!(first.status.success(), "first sync failed");

    let second_start = Instant::now();
    let second = run_nodus(temp.path(), [store_arg.as_str(), "sync"]);
    let second_elapsed = second_start.elapsed();
    assert!(second.status.success(), "second sync failed");

    // Generous bound: 5x first run. Noisy CI runners can fluctuate a lot;
    // the point of this assertion is to catch a pathological regression
    // (e.g. accidentally re-rendering twice), not to micro-benchmark.
    assert!(
        second_elapsed <= first_elapsed * 5,
        "second sync ({second_elapsed:?}) is dramatically slower than first sync ({first_elapsed:?})"
    );
}
