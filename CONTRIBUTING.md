# Contributing to Nodus

## Development Workflow

Nodus is a Rust CLI. Use the standard Cargo workflow while iterating:

```bash
cargo check
cargo test
```

Before opening a pull request or publishing a release, run the full local preflight:

```bash
bash scripts/rust_ci_preflight.sh
```

That script mirrors the repository CI and checks:

- formatting with `cargo fmt --all --check`
- lints with `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- tests with `cargo test --workspace --all-targets`
- packaging with `cargo package --locked`

## Release Notes

For a crates.io release:

1. Run `bash scripts/rust_ci_preflight.sh`.
2. Run `cargo publish --dry-run`.
3. Confirm the package metadata in `Cargo.toml` still reflects the release.
4. Publish with `cargo publish`.
5. Publish the matching GitHub Release. The `Release` workflow will build and attach binary archives for each supported platform.
6. Keep the `NODUS_RELEASE_AUTOMATION_TOKEN` GitHub Actions secret configured in `nodus`. The release workflow uses it to dispatch follow-up updates to `homebrew-nodus` and `nodus-website`.

To backfill binary assets onto an existing GitHub Release, run the `Release` workflow manually from GitHub Actions and provide the release tag in the `tag_name` input.
