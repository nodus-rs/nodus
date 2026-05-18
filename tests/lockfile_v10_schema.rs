//! Integration coverage for the lockfile v9 → v10 schema bump (Slice 1).
//!
//! The `nodus::lockfile` module is `pub(crate)`, so integration tests cannot
//! call `Lockfile::read` / `Lockfile::read_for_sync` directly. Instead, these
//! tests drive the `nodus` binary against on-disk lockfile fixtures and assert
//! on its observable behavior (exit status, stderr error message). The
//! mappings used here:
//!
//! - `nodus list` triggers `Lockfile::read` (strict v10) via
//!   `domain::dependency_status::load_lockfile`.
//! - `nodus sync` triggers `Lockfile::read_for_sync` (lenient v4..=v10) via the
//!   resolver runtime.
//!
//! Pure-serde behavior (`Lockfile::new` round-trip, deterministic ordering,
//! `ROOT_PACKAGE_NAME_SENTINEL` handling) lives in the inline `#[cfg(test)]`
//! module of `src/lockfile.rs` because it needs direct access to the type.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

/// Invoke the `nodus` binary in `cwd` with the given arguments and return the
/// captured output. The binary path is wired up by Cargo at build time.
fn run_nodus<I, S>(cwd: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    Command::new(env!("CARGO_BIN_EXE_nodus"))
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("spawn nodus binary")
}

/// Write `nodus.toml` + `nodus.lock` into a fresh temp workspace and return the
/// `TempDir` guard. The manifest stays minimal — we only need enough config to
/// keep the manifest loader happy while the lockfile path runs.
fn workspace_with_lockfile(manifest: &str, lockfile: &str) -> TempDir {
    let temp = TempDir::new().expect("workspace tempdir");
    fs::write(temp.path().join("nodus.toml"), manifest).expect("write nodus.toml");
    fs::write(temp.path().join("nodus.lock"), lockfile).expect("write nodus.lock");
    temp
}

/// Minimal manifest body. Empty contents are accepted by the loader.
const EMPTY_MANIFEST: &str = "";

/// Manifest with a single adapter declared, which is required by `nodus sync`
/// to get past the adapter-config check.
const CLAUDE_ADAPTER_MANIFEST: &str = "[adapters]\nenabled = [\"claude\"]\n";

// Acceptance: Slice 1 — lockfile v10 schema bump.
// `Lockfile::read_for_sync` must still accept the realistic v9-shape lockfile
// we see in the wild (top-level `managed_files`, no per-package owned_* fields,
// no `install_digest`).
#[test]
fn reads_real_world_v9_lockfile_via_read_for_sync() {
    // A v9 fixture modeled on the actual repo's pre-bump `nodus.lock`: bloated
    // top-level `managed_files`, two real packages with the v9 layout (no
    // owned_subtrees / owned_prefixes / owned_files / install_digest).
    let v9_lockfile = r#"version = 9
managed_files = [
    ".claude/skills/review",
    ".claude/hooks/nodus-hook-thing-12345678.sh",
    ".nodus/packages/foo/claude-plugin/skills/x",
]

[[packages]]
alias = "foo"
name = "foo"
version_tag = "v0.1.0"
digest = "blake3:abc"
skills = ["review"]
agents = []
rules = []
commands = []
mcp_servers = []
dependencies = []
capabilities = []

[packages.source]
kind = "git"
url = "https://github.com/example/foo"
tag = "v0.1.0"
rev = "01f556abcdef"

[[packages]]
alias = "bar"
name = "bar"
version_tag = "v0.2.0"
digest = "blake3:def"
skills = []
agents = ["reviewer"]
rules = []
commands = []
mcp_servers = []
dependencies = []
capabilities = []

[packages.source]
kind = "git"
url = "https://github.com/example/bar"
tag = "v0.2.0"
rev = "abcdef012345"
"#;

    // `nodus sync` is the front door for `Lockfile::read_for_sync`. It runs
    // the resolver, which on a v9 lockfile prints a `note: upgrading nodus.lock
    // from version 9 to 10` line — that note is the proof the sync read
    // succeeded and recognised version 9.
    let workspace = workspace_with_lockfile(CLAUDE_ADAPTER_MANIFEST, v9_lockfile);
    let store = TempDir::new().expect("store tempdir");
    let sync_output = run_nodus(
        workspace.path(),
        ["--store-path", store.path().to_str().unwrap(), "sync"],
    );

    let stderr = String::from_utf8_lossy(&sync_output.stderr);
    let stdout = String::from_utf8_lossy(&sync_output.stdout);
    let combined = format!("{stderr}{stdout}");
    assert!(
        combined.contains("upgrading nodus.lock from version 9 to 10"),
        "expected sync to recognise the v9 lockfile and announce an upgrade. \
         stdout: {stdout:?} stderr: {stderr:?}"
    );

    // The strict reader (used by `nodus list`) must reject the same v9
    // fixture and the error must name both the offending version and the
    // expected version. We rewrite the lockfile after sync since sync would
    // have upgraded it on disk.
    fs::write(workspace.path().join("nodus.lock"), v9_lockfile).expect("rewrite v9 lockfile");
    let list_output = run_nodus(workspace.path(), ["list"]);
    assert!(
        !list_output.status.success(),
        "expected `nodus list` to reject v9 lockfile under strict read"
    );
    let list_stderr = String::from_utf8_lossy(&list_output.stderr);
    assert!(
        list_stderr.contains("version 9"),
        "expected error to name the offending version. stderr: {list_stderr}"
    );
    assert!(
        list_stderr.contains("expected 10"),
        "expected error to name the expected version. stderr: {list_stderr}"
    );
}

// Acceptance: Slice 1 — install_digest validation requires the `blake3:`
// prefix.
#[test]
fn rejects_v10_lockfile_with_install_digest_missing_blake3_prefix() {
    let lockfile = r#"version = 10

[[packages]]
alias = "shared"
name = "shared"
digest = "blake3:abc"
install_digest = "deadbeef"

[packages.source]
kind = "git"
url = "https://github.com/example/shared"
tag = "v0.1.0"
rev = "01f556abcdef"
"#;

    let workspace = workspace_with_lockfile(EMPTY_MANIFEST, lockfile);
    let output = run_nodus(workspace.path(), ["list"]);

    assert!(
        !output.status.success(),
        "expected `nodus list` to reject the malformed install_digest"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("blake3:"),
        "error must mention required `blake3:` prefix. stderr: {stderr}"
    );
    assert!(
        stderr.contains("shared"),
        "error must name the offending package alias. stderr: {stderr}"
    );
    assert!(
        stderr.contains("deadbeef"),
        "error must include the offending digest value. stderr: {stderr}"
    );
}

// Acceptance: Slice 1 — only `blake3:` is accepted; `sha256:` is not a valid
// install_digest prefix in v10.
#[test]
fn rejects_v10_lockfile_with_install_digest_using_sha256_prefix() {
    let lockfile = r#"version = 10

[[packages]]
alias = "shared"
name = "shared"
digest = "blake3:abc"
install_digest = "sha256:abc"

[packages.source]
kind = "git"
url = "https://github.com/example/shared"
tag = "v0.1.0"
rev = "01f556abcdef"
"#;

    let workspace = workspace_with_lockfile(EMPTY_MANIFEST, lockfile);
    let output = run_nodus(workspace.path(), ["list"]);

    assert!(
        !output.status.success(),
        "expected `nodus list` to reject sha256-prefixed install_digest"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("blake3:"),
        "error must mention required `blake3:` prefix. stderr: {stderr}"
    );
    assert!(
        stderr.contains("sha256:abc"),
        "error must include the offending digest value. stderr: {stderr}"
    );
}

// Acceptance: Slice 1 — a properly-prefixed install_digest is accepted by the
// strict reader.
#[test]
fn accepts_v10_lockfile_with_blake3_prefixed_install_digest() {
    let lockfile = r#"version = 10

[[packages]]
alias = "shared"
name = "shared"
digest = "blake3:abc"
install_digest = "blake3:abc123"

[packages.source]
kind = "git"
url = "https://github.com/example/shared"
tag = "v0.1.0"
rev = "01f556abcdef"
"#;

    let workspace = workspace_with_lockfile(EMPTY_MANIFEST, lockfile);
    let output = run_nodus(workspace.path(), ["list"]);

    assert!(
        output.status.success(),
        "expected `nodus list` to accept blake3-prefixed install_digest. \
         stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// Acceptance: Slice 1 — v10 readers tolerate v10 lockfiles with the new
// per-package owned_* fields populated. This is a "happy path" smoke test for
// the optional ownership fields all together; it confirms the lockfile parses
// and `nodus list` surfaces the package, exercising both the schema-level
// deserialization and the package-listing read path.
#[test]
fn accepts_v10_lockfile_with_all_new_owned_fields_populated() {
    let lockfile = r#"version = 10

[[packages]]
alias = "foo"
name = "foo"
version_tag = "v0.1.0"
digest = "blake3:abc"
owned_subtrees = [".nodus/packages/foo/claude-plugin"]
owned_runtime_adapters = ["opencode"]
owned_files = [".claude/settings.json"]
install_digest = "blake3:0123456789abcdef"

[[packages.owned_prefixes]]
dir = ".claude/hooks"
prefix = "nodus-hook-foo-"

[packages.source]
kind = "git"
url = "https://github.com/example/foo"
tag = "v0.1.0"
rev = "01f556abcdef"
"#;

    let workspace = workspace_with_lockfile(EMPTY_MANIFEST, lockfile);
    let output = run_nodus(workspace.path(), ["list"]);

    assert!(
        output.status.success(),
        "expected `nodus list` to accept v10 lockfile with all owned_* fields. \
         stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// Acceptance: Slice 1 — the strict reader must surface a clear version
// mismatch for any v10-incompatible-but-still-numbered lockfile (here: v5,
// which is sync-compatible but not strict-compatible).
#[test]
fn strict_reader_rejects_v5_lockfile_with_version_mismatch_message() {
    let lockfile = r#"version = 5
packages = []
managed_files = []
"#;

    let workspace = workspace_with_lockfile(EMPTY_MANIFEST, lockfile);
    let output = run_nodus(workspace.path(), ["list"]);

    assert!(
        !output.status.success(),
        "expected `nodus list` to reject v5 lockfile under strict read"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("version 5"),
        "error must name the offending version. stderr: {stderr}"
    );
    assert!(
        stderr.contains("expected 10"),
        "error must name the expected version. stderr: {stderr}"
    );
}
