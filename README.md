<p align="center">
  <img src="./assets/nodus-mark.svg" alt="Nodus mark" width="144">
</p>

<h1 align="center">Nodus</h1>

<p align="center"><strong>Add agent packages to your repo with one command.</strong></p>

<p align="center">
  Install skills, agents, rules, and commands from GitHub or a local path,
  lock the exact version, and write the runtime files your repo actually uses.
</p>

<p align="center">
  English • <a href="./README.cn.md">简体中文</a>
</p>

<p align="center">
  <a href="#install">Install</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#common-tasks">Common Tasks</a> •
  <a href="#advanced">Advanced</a> •
  <a href="#manifest">Manifest</a> •
  <a href="./CONTRIBUTING.md">Contributing</a>
</p>

## What Is Nodus?

Nodus is a package manager for repo-scoped AI tooling.

If a package publishes content under folders like `skills/`, `agents/`, `rules/`, or `commands/`, Nodus can:

- add it from GitHub, Git, or a local path
- pin what you asked for in `nodus.toml`
- lock the exact resolved revision in `nodus.lock`
- write managed files into `.codex/`, `.claude/`, `.cursor/`, `.agents/`, or `.opencode/`
- prune stale generated files without touching unmanaged ones

For most users, the main command is:

```bash
nodus add <package>
```

## Install

Install from crates.io:

```bash
cargo install nodus
```

Install the latest prebuilt binary on macOS or Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/WendellXY/nodus/main/install.sh | bash
```

Install with Homebrew:

```bash
brew install WendellXY/nodus/nodus
```

## Quick Start

Install a package for Codex:

```bash
nodus add WendellXY/nodus --adapter codex
```

That one command:

- creates `nodus.toml` if your repo does not have one yet
- records the dependency in `nodus.toml`
- resolves the latest tag by default
- locks the exact resolved revision in `nodus.lock`
- writes managed runtime files for the selected adapter

Validate the result:

```bash
nodus doctor
```

Typical output files look like this:

```text
.codex/skills/<skill-id>_<source-id>/
.claude/skills/<skill-id>_<source-id>/
.cursor/rules/<rule-id>_<source-id>.mdc
```

## `nodus add`

Add from GitHub:

```bash
nodus add owner/repo --adapter codex
```

Add from a local path:

```bash
nodus add ./vendor/playbook --adapter codex
```

Pin a tag, branch, commit, or semver range:

```bash
nodus add owner/repo --tag v1.2.3
nodus add owner/repo --branch main
nodus add owner/repo --revision 0123456789abcdef
nodus add owner/repo --version '^1.2.0'
```

Install only part of a package:

```bash
nodus add owner/repo --adapter claude --component skills
nodus add owner/repo --adapter claude --component skills --component rules
```

Add a dev-only dependency:

```bash
nodus add owner/repo --dev --adapter codex
```

Start tools with automatic sync:

```bash
nodus add owner/repo --adapter codex --sync-on-launch
```

Preview changes without writing them:

```bash
nodus add owner/repo --adapter codex --dry-run
```

## Common Tasks

Inspect a package without changing your repo:

```bash
nodus info owner/repo
nodus info ./vendor/playbook
nodus info installed_alias
```

See what can be updated:

```bash
nodus outdated
```

Update dependencies and rewrite managed files:

```bash
nodus update
```

Rebuild managed files from what is already recorded:

```bash
nodus sync
```

Use these in CI:

```bash
nodus sync --locked
nodus sync --frozen
```

Remove a dependency:

```bash
nodus remove nodus
```

Check that your manifest, lockfile, store, and managed files are consistent:

```bash
nodus doctor
```

Generate shell completions:

```bash
nodus completion bash
nodus completion zsh
nodus completion fish
```

## Advanced

Supported platforms:

- macOS (`x86_64`, `arm64`)
- Linux (`x86_64`, `arm64`/`aarch64`)
- Windows (`x86_64`)

By default, Nodus stores shared mirrors, checkouts, and snapshots here:

```text
macOS:   ~/Library/Application Support/nodus/
Linux:   ~/.local/state/nodus/              (or $XDG_STATE_HOME/nodus/)
Windows: %LOCALAPPDATA%\nodus\
```

Override that location for any command with `--store-path <path>`.

Install a specific release with the installer script:

```bash
curl -fsSL https://raw.githubusercontent.com/WendellXY/nodus/main/install.sh | bash -s -- --version v0.1.0
```

## When To Use `sync` vs `update`

Use `nodus sync` when you want Nodus to make the repo match your current manifest and lockfile.

Use `nodus update` when you want Nodus to look for newer allowed versions first, then sync to those newer results.

Use `nodus sync --locked` in CI when the lockfile must not change.

Use `nodus sync --frozen` when installs must use the exact revisions already recorded in `nodus.lock`.

## Adapters

Nodus writes outputs only for the adapters your repo uses.

Supported adapters today:

- `agents`
- `claude`
- `codex`
- `cursor`
- `opencode`

You can choose adapters explicitly with `--adapter`, persist them in `nodus.toml`, or let Nodus detect them from existing repo roots such as `.codex/` or `.claude/`.

## Manifest

The smallest useful consumer manifest looks like this:

```toml
[adapters]
enabled = ["codex"]

[dependencies]
nodus = { github = "WendellXY/nodus", tag = "v0.3.2" }
```

Common dependency forms:

```toml
[dependencies]
playbook = { path = "vendor/playbook" }
tooling = { github = "owner/tooling", version = "^1.2.0" }
shared = { github = "owner/shared", tag = "v1.4.0", components = ["skills"] }

[dev-dependencies]
internal = { path = "vendor/internal" }
```

Direct dependencies can also map files or directories into the consuming repo:

```toml
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
```

For a fuller example, see [examples/nodus.toml](./examples/nodus.toml).

## Package Layout

Nodus discovers package content from these conventional paths:

- `skills/<id>/SKILL.md`
- `agents/<id>.md`
- `rules/<id>.*`
- `commands/<id>.md`

Packages can also declare:

- `content_roots` to publish additional folders
- `publish_root = true` to emit the root package itself
- `capabilities` for privileged behavior such as high-sensitivity actions

If a package declares `high` sensitivity capabilities, install or update with:

```bash
nodus sync --allow-high-sensitivity
nodus update --allow-high-sensitivity
```

## Relay

`nodus relay` is for package maintainers who edit generated runtime files in a consumer repo and want to copy those edits back into the source checkout.

```bash
nodus relay nodus --repo-path ../nodus
nodus relay nodus --watch
```

This is an advanced workflow. Most users only need `add`, `info`, `outdated`, `update`, `sync`, `remove`, and `doctor`.

## Why Teams Use Nodus

- one command to add repo-scoped AI tooling
- exact revisions locked in `nodus.lock`
- generated files stay managed and pruneable
- unmanaged files are never overwritten
- mirrors, checkouts, and snapshots are shared across repos

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for local development and release checks.

## License

Licensed under [Apache-2.0](LICENSE).
