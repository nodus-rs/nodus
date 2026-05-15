//! Integration coverage for the Slice 1 manifest-side change: the manifest
//! validator now rejects `name` values containing `<` or `>` so that no
//! user-supplied manifest can collide with the lockfile root sentinel
//! (`<root>`).
//!
//! The validation runs whenever the manifest is loaded, so any command that
//! reads `nodus.toml` will trip it. We drive `nodus list` because it's the
//! cheapest read-only entrypoint that surfaces the error to stderr.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

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

fn workspace_with_manifest(manifest: &str) -> TempDir {
    let temp = TempDir::new().expect("workspace tempdir");
    fs::write(temp.path().join("nodus.toml"), manifest).expect("write nodus.toml");
    temp
}

// Acceptance: Slice 1 — `<` and `>` are reserved in manifest `name` values.
// They're the delimiters of the lockfile root sentinel and must not appear in
// user-supplied names.
#[test]
fn manifest_loader_rejects_package_name_with_angle_brackets() {
    let manifest = r#"name = "foo<bar>"
"#;
    let workspace = workspace_with_manifest(manifest);

    let output = run_nodus(workspace.path(), ["list"]);

    assert!(
        !output.status.success(),
        "expected manifest with `<` and `>` to be rejected at load time"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("foo<bar>"),
        "error should quote the offending name. stderr: {stderr}"
    );
    assert!(
        stderr.contains("reserved") || (stderr.contains('<') && stderr.contains('>')),
        "error should explain why the name is rejected (reserved / < / >). \
         stderr: {stderr}"
    );
}

// Acceptance: Slice 1 — a name containing only `<` (no closing bracket) is
// still rejected. Validation must guard on either character, not just the
// pair.
#[test]
fn manifest_loader_rejects_package_name_with_only_opening_bracket() {
    let manifest = r#"name = "foo<bar"
"#;
    let workspace = workspace_with_manifest(manifest);

    let output = run_nodus(workspace.path(), ["list"]);

    assert!(
        !output.status.success(),
        "expected manifest with `<` to be rejected at load time"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("foo<bar"),
        "error should quote the offending name. stderr: {stderr}"
    );
}

// Acceptance: Slice 1 — the literal sentinel `<root>` cannot be a
// user-supplied manifest name. This is the headline collision the rule
// prevents.
#[test]
fn manifest_loader_rejects_literal_root_sentinel_as_package_name() {
    let manifest = r#"name = "<root>"
"#;
    let workspace = workspace_with_manifest(manifest);

    let output = run_nodus(workspace.path(), ["list"]);

    assert!(
        !output.status.success(),
        "expected manifest with literal `<root>` name to be rejected"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("<root>"),
        "error should quote the offending name. stderr: {stderr}"
    );
}

// Acceptance: Slice 1 — non-bracket names that previously worked must still
// be accepted. This guards against the validation rule being too aggressive.
#[test]
fn manifest_loader_accepts_normal_package_name() {
    let manifest = r#"name = "perfectly-fine-name"
"#;
    let workspace = workspace_with_manifest(manifest);

    let output = run_nodus(workspace.path(), ["list"]);

    assert!(
        output.status.success(),
        "expected manifest with a normal name to load cleanly. \
         stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
