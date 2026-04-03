# Notify + BLAKE3 Relay Watch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace SHA-256 with BLAKE3 across the entire codebase and convert the polling-based relay watch to an async event-driven architecture using `notify`.

**Architecture:** Two-phase migration. Phase 1 swaps sha2 for blake3 in all hashing (computational + persisted formats with backwards compat). Phase 2 converts the synchronous polling watch loop to an async `notify`-driven loop with BLAKE3 fingerprints as the source of truth and a 30-second fallback sweep.

**Tech Stack:** Rust, blake3, notify 8, tokio

---

## File Structure

| File | Responsibility |
|------|----------------|
| `Cargo.toml` | Add `blake3`, `notify = "8"`; remove `sha2` |
| `src/hashing.rs` | **NEW** — shared `blake3_hex` and `content_digest` helpers |
| `src/git.rs` | Swap `sha2` → `blake3` in `short_hash` |
| `src/resolver/runtime/resolve.rs` | Swap `sha2` → `blake3` in `compute_digest`, produce `blake3:` prefix |
| `src/store.rs` | `STORE_ROOT` → `"store/blake3"` |
| `src/lockfile.rs` | Accept both `sha256:` and `blake3:` digest prefixes |
| `src/adapters.rs` | Accept both `sha256:` and `blake3:` digest prefixes |
| `src/local_config.rs` | Rename `source_sha256` → `source_hash` with `#[serde(alias)]` |
| `src/relay/runtime.rs` | `sha256_hex` → `blake3_hex` from hashing module, public watch fns become async |
| `src/relay/runtime/watch.rs` | Async notify loop with BLAKE3 fingerprints, new `RelayWatchOptions` |
| `src/clean.rs` | No code change needed (uses `STORE_ROOT` constant) |
| `src/cli/handlers/project.rs` | `.await` on async watch calls |
| `src/cli/tests.rs` | Update `store/sha256` path assertions |

---

## Phase 1: BLAKE3 Migration

### Task 1: Add blake3 dependency and create shared hashing module

**Files:**
- Modify: `Cargo.toml`
- Create: `src/hashing.rs`
- Modify: `src/lib.rs` (or `src/main.rs` — wherever modules are declared)

- [ ] **Step 1: Check the module declaration file**

Read `src/lib.rs` or `src/main.rs` to find where modules are declared, so we know where to add `mod hashing;`.

- [ ] **Step 2: Add blake3 to Cargo.toml**

In `Cargo.toml`, add `blake3` to `[dependencies]`:

```toml
blake3 = "1"
```

Do NOT remove `sha2` yet — other files still use it.

- [ ] **Step 3: Create src/hashing.rs**

```rust
/// Compute a hex-encoded BLAKE3 hash of the given bytes.
pub fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Compute a prefixed content digest over ordered file entries.
///
/// Each entry is `(relative_path_string, file_contents)`. The entries are
/// hashed in order with a NUL separator after the path and an 0xFF separator
/// after the contents, matching the prior SHA-256 scheme but using BLAKE3.
pub fn content_digest(entries: &[(&str, &[u8])]) -> String {
    let mut hasher = blake3::Hasher::new();
    for (path, contents) in entries {
        hasher.update(path.as_bytes());
        hasher.update(&[0]);
        hasher.update(contents);
        hasher.update(&[0xff]);
    }
    format!("blake3:{}", hasher.finalize().to_hex())
}
```

- [ ] **Step 4: Register the module**

Add `mod hashing;` (and `pub mod hashing;` if needed by other crates) in the module declaration file found in step 1.

- [ ] **Step 5: Verify it compiles**

Run: `cargo check`
Expected: compiles with no errors (blake3 and sha2 coexist temporarily).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/hashing.rs src/main.rs
git commit -m "feat(hashing): add blake3 dependency and shared hashing module"
```

---

### Task 2: Migrate git.rs from sha2 to blake3

**Files:**
- Modify: `src/git.rs:7,802-805`

- [ ] **Step 1: Write a test for short_hash behavior**

The `short_hash` function produces 8-char hex digests. Add a test at the bottom of `src/git.rs` (inside the existing `#[cfg(test)] mod tests` block, if one exists, or create one):

```rust
#[cfg(test)]
mod hashing_tests {
    use super::short_hash;

    #[test]
    fn short_hash_produces_eight_hex_chars() {
        let hash = short_hash("https://github.com/example/repo.git");
        assert_eq!(hash.len(), 8);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn short_hash_is_deterministic() {
        let a = short_hash("foo");
        let b = short_hash("foo");
        assert_eq!(a, b);
    }
}
```

- [ ] **Step 2: Run the test to verify it passes with current sha2 impl**

Run: `cargo test --lib -- hashing_tests`
Expected: PASS (the tests verify shape, not specific hash values).

- [ ] **Step 3: Replace sha2 with blake3 in git.rs**

Replace the import line:
```rust
// Remove:
use sha2::{Digest, Sha256};

// Add:
use crate::hashing::blake3_hex;
```

Replace the `short_hash` function body at line ~802:
```rust
fn short_hash(value: &str) -> String {
    blake3_hex(value.as_bytes())[..8].to_string()
}
```

- [ ] **Step 4: Run the test to verify it still passes**

Run: `cargo test --lib -- hashing_tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/git.rs
git commit -m "refactor(git): migrate short_hash from sha2 to blake3"
```

---

### Task 3: Migrate resolver digest computation to blake3

**Files:**
- Modify: `src/resolver/runtime/resolve.rs:7,1003-1011`

- [ ] **Step 1: Replace sha2 with blake3 in resolve.rs**

Replace the import:
```rust
// Remove:
use sha2::{Digest, Sha256};

// Add:
use crate::hashing::content_digest;
```

Replace the digest computation block (lines ~1003-1011):
```rust
    let entries: Vec<(&str, &[u8])> = file_payloads
        .iter()
        .map(|(path, contents)| (path.to_string_lossy().as_ref(), contents.as_slice()))
        .collect();
    // Note: content_digest needs owned strings since to_string_lossy returns Cow.
    // Adjust to collect owned strings first:
```

Actually, since `content_digest` takes `&[(&str, &[u8])]` and `to_string_lossy()` returns a `Cow<str>`, we need to adjust. Replace the entire final section of the function (from the `let mut hasher` line):

```rust
    let path_strings: Vec<String> = file_payloads
        .iter()
        .map(|(path, _)| path.to_string_lossy().into_owned())
        .collect();
    let entries: Vec<(&str, &[u8])> = path_strings
        .iter()
        .zip(file_payloads.iter())
        .map(|(path, (_, contents))| (path.as_str(), contents.as_slice()))
        .collect();
    Ok(content_digest(&entries))
```

- [ ] **Step 2: Run existing resolver tests**

Run: `cargo test --lib resolver`
Expected: PASS. Tests should not assert specific hash values — they use fixtures that get recomputed.

- [ ] **Step 3: Commit**

```bash
git add src/resolver/runtime/resolve.rs
git commit -m "refactor(resolver): migrate digest computation from sha2 to blake3"
```

---

### Task 4: Migrate relay runtime from sha2 to blake3

**Files:**
- Modify: `src/relay/runtime.rs:9,735,741,770,908-910`
- Modify: `src/local_config.rs:42`

- [ ] **Step 1: Rename source_sha256 to source_hash in local_config.rs**

At line 42, change the `RelayedFileState` struct:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayedFileState {
    #[serde(alias = "source_sha256")]
    pub source_hash: String,
}
```

- [ ] **Step 2: Update all references to source_sha256 in relay/runtime.rs**

Replace the import at line 9:
```rust
// Remove:
use sha2::{Digest, Sha256};

// Add:
use crate::hashing::blake3_hex;
```

Replace the `sha256_hex` function (lines 908-910):
```rust
fn content_hash(bytes: &[u8]) -> String {
    blake3_hex(bytes)
}
```

Then replace all occurrences of `sha256_hex(` with `content_hash(` in this file (~lines 735, 741, 770).

Replace all occurrences of `.source_sha256` with `.source_hash` in this file (~line 735).

- [ ] **Step 3: Update test assertions for source_sha256 field name**

In the test at line ~2188, change:
```rust
// From:
link.files["skills/review/SKILL.md"].source_sha256,
// To:
link.files["skills/review/SKILL.md"].source_hash,
```

Search for all `source_sha256` references in the test section and update them.

- [ ] **Step 4: Run relay tests**

Run: `cargo test --lib relay`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/relay/runtime.rs src/local_config.rs
git commit -m "refactor(relay): migrate from sha2 to blake3, rename source_sha256 to source_hash"
```

---

### Task 5: Migrate store and lockfile digest parsing for backwards compat

**Files:**
- Modify: `src/store.rs:11,177-181`
- Modify: `src/lockfile.rs:378`
- Modify: `src/adapters.rs:525,558`
- Modify: `src/cli/tests.rs:2013,2035`

- [ ] **Step 1: Update STORE_ROOT in store.rs**

At line 11:
```rust
pub const STORE_ROOT: &str = "store/blake3";
```

At lines 177-181, update `digest_directory_name` to accept both prefixes:
```rust
fn digest_directory_name(digest: &str) -> Result<&str> {
    digest
        .strip_prefix("blake3:")
        .or_else(|| digest.strip_prefix("sha256:"))
        .ok_or_else(|| anyhow::anyhow!("unsupported digest format `{digest}`"))
}
```

- [ ] **Step 2: Update lockfile.rs digest parsing**

At line ~378, update the `strip_prefix` call:
```rust
// Replace:
.strip_prefix("sha256:")
// With:
.strip_prefix("blake3:")
.or_else(|_| package.digest.strip_prefix("sha256:"))
```

Check the exact context — this is inside `locked_package_short_id`. The pattern is the same: try `blake3:` first, fall back to `sha256:`.

Actually, looking at the code, `strip_prefix` on `&str` returns `Option`, not `Result`. So:
```rust
.strip_prefix("blake3:")
.or_else(|| package.digest.strip_prefix("sha256:"))
```

- [ ] **Step 3: Update adapters.rs digest parsing**

At lines ~525 and ~558, apply the same pattern:
```rust
// Replace each:
.strip_prefix("sha256:")
// With:
.strip_prefix("blake3:")
.or_else(|| <var>.strip_prefix("sha256:"))
```

Where `<var>` is the string being stripped — check the surrounding context for the exact variable name (`package.digest` or `package.digest.as_str()`).

- [ ] **Step 4: Update CLI test assertions**

At line ~2013 in `src/cli/tests.rs`:
```rust
// From:
let snapshot_root = cache.path().join("store/sha256").join("sha");
// To:
let snapshot_root = cache.path().join("store/blake3").join("sha");
```

At line ~2035:
```rust
// From:
assert!(!cache.path().join("store/sha256").exists());
// To:
assert!(!cache.path().join("store/blake3").exists());
```

- [ ] **Step 5: Write a backwards-compat test for digest parsing**

Add to `src/store.rs` tests:
```rust
#[test]
fn digest_directory_name_accepts_blake3_prefix() {
    assert_eq!(digest_directory_name("blake3:abc123").unwrap(), "abc123");
}

#[test]
fn digest_directory_name_accepts_legacy_sha256_prefix() {
    assert_eq!(digest_directory_name("sha256:abc123").unwrap(), "abc123");
}

#[test]
fn digest_directory_name_rejects_unknown_prefix() {
    assert!(digest_directory_name("md5:abc123").is_err());
}
```

- [ ] **Step 6: Write a backwards-compat test for source_sha256 alias**

Add to `src/local_config.rs` tests:
```rust
#[test]
fn relayed_file_state_accepts_legacy_source_sha256_field() {
    let toml_str = r#"source_sha256 = "abc123""#;
    let state: RelayedFileState = toml::from_str(toml_str).unwrap();
    assert_eq!(state.source_hash, "abc123");
}

#[test]
fn relayed_file_state_uses_source_hash_field() {
    let toml_str = r#"source_hash = "def456""#;
    let state: RelayedFileState = toml::from_str(toml_str).unwrap();
    assert_eq!(state.source_hash, "def456");
}
```

- [ ] **Step 7: Run full test suite**

Run: `cargo test`
Expected: ALL PASS.

- [ ] **Step 8: Commit**

```bash
git add src/store.rs src/lockfile.rs src/adapters.rs src/local_config.rs src/cli/tests.rs
git commit -m "refactor(store): migrate store and digest parsing to blake3 with sha256 backwards compat"
```

---

### Task 6: Remove sha2 dependency

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/relay/runtime/watch.rs:8`

- [ ] **Step 1: Replace sha2 in watch.rs**

At line 8:
```rust
// Remove:
use sha2::{Digest, Sha256};

// Add:
use crate::hashing::blake3_hex;
```

At line ~273, replace the fingerprint computation:
```rust
// Remove:
let digest = Sha256::digest(contents);
let mut hash = [0u8; 32];
hash.copy_from_slice(&digest);
Ok(PathFingerprint::File(hash))

// Replace with:
let hash: [u8; 32] = *blake3::hash(&contents).as_bytes();
Ok(PathFingerprint::File(hash))
```

- [ ] **Step 2: Remove sha2 from Cargo.toml**

Remove the line:
```toml
sha2 = "0.10.9"
```

- [ ] **Step 3: Verify no remaining sha2 references**

Run: `cargo check`
Expected: compiles with no errors. No remaining `use sha2` anywhere.

Also search:
```bash
grep -r "use sha2" src/
```
Expected: no matches.

- [ ] **Step 4: Run full test suite**

Run: `cargo test`
Expected: ALL PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/relay/runtime/watch.rs
git commit -m "chore: remove sha2 dependency, complete blake3 migration"
```

---

## Phase 2: Async Notify Watch

### Task 7: Add notify dependency and update RelayWatchOptions

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/relay/runtime/watch.rs`

- [ ] **Step 1: Add notify to Cargo.toml**

In the platform-gated dependencies section:
```toml
[target.'cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))'.dependencies]
async-trait = "0.1.89"
mentra = "0.5.0"
notify = "8"
tokio = { version = "1.50.0", features = ["macros", "rt-multi-thread", "time", "sync", "signal"] }
```

Note: add `"time"`, `"sync"`, and `"signal"` features to tokio (needed for `tokio::time::sleep`, `tokio::sync::mpsc`, and `tokio::signal::ctrl_c`).

- [ ] **Step 2: Update RelayWatchOptions in watch.rs**

Replace the existing `RelayWatchOptions` struct:

```rust
#[derive(Debug, Clone, Copy)]
pub(super) struct RelayWatchOptions {
    pub(super) debounce: Duration,
    pub(super) fallback_interval: Duration,
    pub(super) max_events: Option<usize>,
    pub(super) timeout: Option<Duration>,
}

impl Default for RelayWatchOptions {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(100),
            fallback_interval: Duration::from_secs(30),
            max_events: None,
            timeout: None,
        }
    }
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check`
Expected: compile errors in watch.rs body (still references `poll_interval` and `max_polls`) — that's expected, we'll fix in the next task.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock src/relay/runtime/watch.rs
git commit -m "feat(watch): add notify dependency and update RelayWatchOptions"
```

---

### Task 8: Rewrite the watch loop to async notify

**Files:**
- Modify: `src/relay/runtime/watch.rs` (full rewrite of the loop)

This is the core change. The synchronous `thread::sleep` polling loop becomes an async `tokio::select!` loop driven by `notify` events.

- [ ] **Step 1: Update imports in watch.rs**

Replace the entire imports section at the top:

```rust
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use super::{
    RelaySummary, build_mappings, dependency_context, display_relative, load_workspace,
    relay_dependency_in_dir, resolve_existing_link,
};
use crate::adapters::Adapter;
use crate::lockfile::LOCKFILE_NAME;
use crate::manifest::MANIFEST_FILE;
use crate::report::Reporter;
```

- [ ] **Step 2: Make the watch functions async**

Replace `watch_dependency_in_dir_with_options`:

```rust
pub(super) async fn watch_dependency_in_dir_with_options(
    project_root: &Path,
    cache_root: &Path,
    package: &str,
    invocation: RelayWatchInvocation<'_>,
    reporter: &Reporter,
) -> Result<Vec<RelaySummary>> {
    let packages = vec![package.to_string()];
    watch_dependencies_in_dir_impl_with_options(
        project_root,
        cache_root,
        &packages,
        invocation,
        reporter,
    )
    .await
}
```

Replace `watch_dependencies_in_dir_with_options`:

```rust
pub(super) async fn watch_dependencies_in_dir_with_options(
    project_root: &Path,
    cache_root: &Path,
    packages: &[String],
    invocation: RelayWatchInvocation<'_>,
    reporter: &Reporter,
) -> Result<Vec<RelaySummary>> {
    watch_dependencies_in_dir_impl_with_options(
        project_root,
        cache_root,
        packages,
        invocation,
        reporter,
    )
    .await
}
```

- [ ] **Step 3: Rewrite the main watch loop**

Replace `watch_dependencies_in_dir_impl_with_options` entirely:

```rust
async fn watch_dependencies_in_dir_impl_with_options(
    project_root: &Path,
    cache_root: &Path,
    packages: &[String],
    invocation: RelayWatchInvocation<'_>,
    reporter: &Reporter,
) -> Result<Vec<RelaySummary>> {
    if packages.is_empty() {
        bail!("relay watch requires at least one dependency");
    }
    if packages.len() > 1 && invocation.repo_path_override.is_some() {
        bail!("`nodus relay --repo-path` requires exactly one dependency");
    }

    // Initial relay pass.
    let mut summaries = Vec::with_capacity(packages.len());
    for package in packages {
        let summary = relay_dependency_in_dir(
            project_root,
            cache_root,
            package,
            invocation.repo_path_override,
            invocation.via_override,
            invocation.create_missing,
            reporter,
        )?;
        reporter.finish(format!(
            "relayed {} into {}; created {} and updated {} source files",
            summary.alias,
            display_relative(project_root, &summary.linked_repo),
            summary.created_file_count,
            summary.updated_file_count,
        ))?;
        summaries.push(summary);
    }

    let mut state = capture_watch_state(project_root, cache_root, packages, reporter)?;

    // Set up notify watcher.
    let (tx, mut rx) = mpsc::channel(256);
    let watcher_result = setup_watcher(&state, project_root, tx);
    let _watcher = match watcher_result {
        Ok(watcher) => {
            reporter.note("watching managed outputs for changes; press Ctrl-C to stop")?;
            Some(watcher)
        }
        Err(err) => {
            reporter.warn(format!(
                "failed to initialize file watcher ({err:#}); falling back to periodic polling"
            ))?;
            reporter.note("watching managed outputs for changes; press Ctrl-C to stop")?;
            None
        }
    };

    let deadline = invocation
        .options
        .timeout
        .map(|t| tokio::time::Instant::now() + t);

    loop {
        if invocation
            .options
            .max_events
            .is_some_and(|max| summaries.len() >= max)
        {
            return Ok(summaries);
        }

        // Wait for a notify event, fallback timeout, deadline, or ctrl-c.
        tokio::select! {
            _ = rx.recv() => {
                // Debounce: wait briefly then drain any queued events.
                tokio::time::sleep(invocation.options.debounce).await;
                while rx.try_recv().is_ok() {}
            }
            _ = tokio::time::sleep(invocation.options.fallback_interval) => {}
            _ = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            } => {
                return Ok(summaries);
            }
            _ = tokio::signal::ctrl_c() => {
                return Ok(summaries);
            }
        }

        let next_state = capture_watch_state(project_root, cache_root, packages, reporter)?;
        let config_changed = next_state.config != state.config;
        let changed_packages = changed_watch_packages(&state, &next_state);
        if !config_changed && changed_packages.is_empty() {
            continue;
        }

        state = next_state;
        if changed_packages.is_empty() {
            reporter.note("reloaded relay watch inputs")?;
            continue;
        }

        for package in changed_packages {
            reporter.status("Watching", format!("detected managed edits for {package}"))?;
            let summary = relay_dependency_in_dir(
                project_root,
                cache_root,
                &package,
                None,
                None,
                invocation.create_missing,
                reporter,
            )?;
            reporter.finish(format!(
                "relayed {} into {}; created {} and updated {} source files",
                summary.alias,
                display_relative(project_root, &summary.linked_repo),
                summary.created_file_count,
                summary.updated_file_count,
            ))?;
            summaries.push(summary);
        }
    }
}
```

- [ ] **Step 4: Add the setup_watcher helper**

Add this function after the main loop:

```rust
fn setup_watcher(
    state: &RelayWatchState,
    project_root: &Path,
    tx: mpsc::Sender<()>,
) -> Result<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            let _ = tx.blocking_send(());
        }
    })?;

    // Watch config files (non-recursive).
    for path in state.config.keys() {
        if path.exists() {
            let watch_path = if path.is_file() {
                path.parent().unwrap_or(project_root)
            } else {
                path.as_path()
            };
            // Ignore errors for individual paths — some may not exist yet.
            let _ = watcher.watch(watch_path, RecursiveMode::NonRecursive);
        }
    }

    // Watch managed output directories (recursive).
    let mut watched_dirs = BTreeSet::new();
    for package_managed in state.managed.values() {
        for path in package_managed.keys() {
            if let Some(parent) = path.parent() {
                // Walk up to find the shallowest managed directory to watch.
                let mut candidate = parent;
                while let Some(grandparent) = candidate.parent() {
                    if grandparent == project_root || !grandparent.starts_with(project_root) {
                        break;
                    }
                    candidate = grandparent;
                }
                if watched_dirs.insert(candidate.to_path_buf()) && candidate.exists() {
                    watcher.watch(candidate, RecursiveMode::Recursive)?;
                }
            }
        }
    }

    Ok(watcher)
}
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo check`
Expected: compile errors in the public wrappers and tests (they still call sync versions). We fix those next.

- [ ] **Step 6: Commit**

```bash
git add src/relay/runtime/watch.rs
git commit -m "feat(watch): rewrite watch loop to async notify with blake3 fingerprints"
```

---

### Task 9: Update public API and CLI handler for async watch

**Files:**
- Modify: `src/relay/runtime.rs:288-333`
- Modify: `src/relay.rs`
- Modify: `src/cli/handlers/project.rs:68-88`

- [ ] **Step 1: Make public watch functions async in runtime.rs**

Replace lines ~288-333:

```rust
pub async fn watch_dependency_in_dir(
    project_root: &Path,
    cache_root: &Path,
    package: &str,
    repo_path_override: Option<&Path>,
    via_override: Option<Adapter>,
    create_missing: bool,
    reporter: &Reporter,
) -> Result<()> {
    watch_dependency_in_dir_with_options(
        project_root,
        cache_root,
        package,
        RelayWatchInvocation {
            repo_path_override,
            via_override,
            create_missing,
            options: RelayWatchOptions::default(),
        },
        reporter,
    )
    .await
    .map(|_| ())
}

pub async fn watch_dependencies_in_dir(
    project_root: &Path,
    cache_root: &Path,
    packages: &[String],
    via_override: Option<Adapter>,
    create_missing: bool,
    reporter: &Reporter,
) -> Result<()> {
    watch_dependencies_in_dir_with_options(
        project_root,
        cache_root,
        packages,
        RelayWatchInvocation {
            repo_path_override: None,
            via_override,
            create_missing,
            options: RelayWatchOptions::default(),
        },
        reporter,
    )
    .await
    .map(|_| ())
}
```

- [ ] **Step 2: Update re-exports in relay.rs**

No change needed — the re-exports stay the same, async functions are re-exported as-is.

- [ ] **Step 3: Update CLI handler to use tokio runtime**

In `src/cli/handlers/project.rs`, the `handle_relay` function is synchronous. Wrap the async watch calls in a tokio runtime block. Replace the watch branch (~lines 68-88):

```rust
    if watch {
        let rt = tokio::runtime::Runtime::new()
            .context("failed to create async runtime for relay watch")?;
        if packages.len() == 1 {
            rt.block_on(crate::relay::watch_dependency_in_dir(
                context.cwd,
                context.cache_root,
                &packages[0],
                repo_path.as_deref(),
                via,
                create_missing,
                context.reporter,
            ))
        } else {
            rt.block_on(crate::relay::watch_dependencies_in_dir(
                context.cwd,
                context.cache_root,
                &packages,
                via,
                create_missing,
                context.reporter,
            ))
        }
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check`
Expected: compile errors only in tests (they still use old options). Main code compiles.

- [ ] **Step 5: Commit**

```bash
git add src/relay/runtime.rs src/relay.rs src/cli/handlers/project.rs
git commit -m "feat(watch): update public API and CLI handler for async watch"
```

---

### Task 10: Update watch tests for async notify

**Files:**
- Modify: `src/relay/runtime.rs` (test section, lines ~2946-3217)

- [ ] **Step 1: Update relay_watch_syncs_follow_up_managed_edits test**

Replace the test at line ~2946:

```rust
    #[tokio::test]
    async fn relay_watch_syncs_follow_up_managed_edits() {
        let (_remote_root, remote_repo) = create_remote_dependency();
        let linked = clone_linked_repo(&remote_repo);
        let linked_repo = linked.path().join("linked");
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        install_dependency(
            project.path(),
            cache.path(),
            &remote_repo,
            &[Adapter::Claude],
        );

        let package = resolved_package(project.path(), cache.path(), &[Adapter::Claude]);
        let managed_skill = managed_skill_root(project.path(), Adapter::Claude, &package, "review")
            .join("SKILL.md");

        let project_root = project.path().to_path_buf();
        let cache_root = cache.path().to_path_buf();
        let linked_repo_for_watch = linked_repo.clone();
        let output = SharedBuffer::default();
        let output_for_watch = output.clone();
        let watch_handle = tokio::spawn(async move {
            watch_dependency_in_dir_with_options(
                &project_root,
                &cache_root,
                "playbook_ios",
                RelayWatchInvocation {
                    repo_path_override: Some(&linked_repo_for_watch),
                    via_override: None,
                    create_missing: false,
                    options: RelayWatchOptions {
                        debounce: Duration::from_millis(10),
                        fallback_interval: Duration::from_secs(30),
                        max_events: Some(2),
                        timeout: Some(Duration::from_secs(5)),
                    },
                },
                &Reporter::sink(ColorMode::Never, output_for_watch),
            )
            .await
            .unwrap()
        });

        let mut ready = false;
        for _ in 0..500 {
            if output
                .contents()
                .contains("watching managed outputs for changes")
            {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ready, "watcher never reported readiness");
        append_file(&managed_skill, "\nWatched relay update.\n");

        let summaries = watch_handle.await.unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[1].updated_file_count, 1);
        assert!(
            fs::read_to_string(linked_repo.join("skills/review/SKILL.md"))
                .unwrap()
                .ends_with("\nWatched relay update.\n")
        );
    }
```

- [ ] **Step 2: Update relay_watch_syncs_multiple_follow_up_edits_to_same_file test**

Same pattern — change `#[test]` to `#[tokio::test]`, `thread::spawn` to `tokio::spawn`, `thread::sleep` to `tokio::time::sleep`, replace `RelayWatchOptions` fields:

```rust
    options: RelayWatchOptions {
        debounce: Duration::from_millis(10),
        fallback_interval: Duration::from_secs(30),
        max_events: Some(3),
        timeout: Some(Duration::from_secs(5)),
    },
```

Replace `wait_until` calls with async equivalents (or rewrite `wait_until` as an async helper):

```rust
    async fn wait_until_async<F: Fn() -> bool>(predicate: F, message: &str) {
        for _ in 0..500 {
            if predicate() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{message}");
    }
```

Use `wait_until_async` instead of `wait_until` in async tests. Make the function body `async fn` and `.await` on the watch handle and sleeps.

- [ ] **Step 3: Update relay_watch_syncs_follow_up_managed_edits_for_multiple_dependencies test**

Same pattern as above. Change to `#[tokio::test] async fn`, replace thread primitives with tokio equivalents, update `RelayWatchOptions`.

- [ ] **Step 4: Run all watch tests**

Run: `cargo test --lib relay_watch`
Expected: ALL PASS.

- [ ] **Step 5: Run full test suite**

Run: `cargo test`
Expected: ALL PASS.

- [ ] **Step 6: Commit**

```bash
git add src/relay/runtime.rs
git commit -m "test(watch): update watch tests for async notify architecture"
```

---

### Task 11: Final cleanup and verification

**Files:**
- Verify all files

- [ ] **Step 1: Search for any remaining sha2 references**

```bash
grep -r "sha2\|Sha256\|sha256_hex" src/
```

Expected: Only `sha256:` string literals in backwards-compat code paths (strip_prefix, alias, test fixtures). No `use sha2` imports.

- [ ] **Step 2: Search for any remaining poll_interval or max_polls references**

```bash
grep -r "poll_interval\|max_polls" src/
```

Expected: no matches.

- [ ] **Step 3: Run full test suite**

Run: `cargo test`
Expected: ALL PASS.

- [ ] **Step 4: Run clippy**

Run: `cargo clippy -- -D warnings`
Expected: no warnings.

- [ ] **Step 5: Commit any final fixes**

If clippy or tests revealed issues, fix and commit:
```bash
git commit -m "fix: address clippy warnings from blake3/notify migration"
```
