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
- write managed files into `.codex/`, `.claude/`, `.cursor/`, `.agents/`, `.github/`, or `.opencode/`
- prune stale generated files without touching unmanaged ones

For most users, the main command is:

```bash
nodus add <package>
```

If you want your agent to learn how to use Nodus automatically inside this repo, start with:

```bash
nodus add nodus-rs/nodus
```

That installs Nodus's own package into the repo so the agent can pick up the managed skills and instructions it publishes. If this is a brand-new repo and Nodus cannot infer your tool yet, add an adapter on the first run, for example `--adapter <adapter>`.

## Install

Install from crates.io:

```bash
cargo install nodus
```

Install the latest prebuilt binary on macOS or Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/nodus-rs/nodus/main/install.sh | bash
```

Install with Homebrew:

```bash
brew install nodus-rs/nodus/nodus
```

Install the latest prebuilt binary on Windows with PowerShell:

```powershell
irm https://raw.githubusercontent.com/nodus-rs/nodus/main/install.ps1 | iex
```

<details>
<summary>Windows install command failed?</summary>

If the command fails (for example, `pwsh` is not recognized), install PowerShell 7, restart your terminal, then run with `pwsh`:

```powershell
winget install --id Microsoft.PowerShell --source winget
# Restart terminal first so `pwsh` is available on PATH.
pwsh -NoProfile -Command "irm https://raw.githubusercontent.com/nodus-rs/nodus/main/install.ps1 | iex"
```

</details>

## Quick Start

If you want the agent in this repo to learn Nodus first, run:

```bash
nodus add nodus-rs/nodus
```

That one command:

- creates `nodus.toml` if your repo does not have one yet
- records the dependency in `nodus.toml`
- resolves the latest tag by default
- locks the exact resolved revision in `nodus.lock`
- writes managed runtime files for the detected or configured adapter

If your repo does not already expose adapter signals such as `.codex/`, `.claude/`, or `.github/skills`, make the first install explicit:

```bash
nodus add nodus-rs/nodus --adapter <adapter>
```

Validate the result:

```bash
nodus doctor
```

Typical output files look like this:

```text
.codex/skills/<skill-id>_<source-id>/
.claude/skills/<skill-id>_<source-id>/
.github/skills/<skill-id>_<source-id>/
.github/agents/<agent-id>_<source-id>.agent.md
.cursor/rules/<rule-id>_<source-id>.mdc
```

## `nodus add`

Add from GitHub:

```bash
nodus add owner/repo --adapter <adapter>
```

Add from a local path:

```bash
nodus add ./vendor/playbook --adapter <adapter>
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
nodus add owner/repo --adapter <adapter> --component skills
nodus add owner/repo --adapter <adapter> --component skills --component rules
```

Add a dev-only dependency:

```bash
nodus add owner/repo --dev --adapter <adapter>
```

Start tools with automatic sync:

```bash
nodus add owner/repo --adapter <adapter> --sync-on-launch
```

Install globally into all detected supported home-scoped agent roots:

```bash
nodus add owner/repo --global
```

Install globally into an explicit home-scoped agent root:

```bash
nodus add owner/repo --global --adapter <adapter>
```

Preview changes without writing them:

```bash
nodus add owner/repo --adapter <adapter> --dry-run
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

Remove a global dependency:

```bash
nodus remove nodus --global
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
- Windows (`x86_64`, `arm64`)

By default, Nodus stores shared mirrors, checkouts, and snapshots here:

```text
macOS:   ~/Library/Application Support/nodus/
Linux:   ~/.local/state/nodus/              (or $XDG_STATE_HOME/nodus/)
Windows: %LOCALAPPDATA%\nodus\
```

Override that location for any command with `--store-path <path>`.

Global installs keep their `nodus.toml` and `nodus.lock` under `<store-path>/global/` and
write managed runtime files into your home-scoped agent folders such as `~/.codex`,
`~/.claude`, `~/.cursor`, `~/.opencode`, and `~/.agents`.

Global installs support these adapters:

- `agents`
- `claude`
- `codex`
- `cursor`
- `opencode`

Global installs do not support `copilot`, and `--global` cannot be combined with
`--sync-on-launch`.

Install a specific release with the Unix installer script:

```bash
curl -fsSL https://raw.githubusercontent.com/nodus-rs/nodus/main/install.sh | bash -s -- --version v0.1.0
```

Install a specific release on Windows:

```powershell
$env:NODUS_VERSION='v0.1.0'; irm https://raw.githubusercontent.com/nodus-rs/nodus/main/install.ps1 | iex
```

If this command fails on Windows, install PowerShell 7, restart your terminal, then run through `pwsh`:

```powershell
$env:NODUS_VERSION='v0.1.0'
pwsh -NoProfile -Command "irm https://raw.githubusercontent.com/nodus-rs/nodus/main/install.ps1 | iex"
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
- `copilot`
- `cursor`
- `opencode`

You can choose adapters explicitly with `--adapter`, persist them in `nodus.toml`, or let Nodus detect them from existing repo roots such as `.codex/`, `.claude/`, or `.github/skills`.

`copilot` manages GitHub Copilot project assets under `.github/skills/` and `.github/agents/`. In v1 it supports skills and custom agents only; rules and commands are not emitted for Copilot.

## Manifest

The smallest useful consumer manifest looks like this:

```toml
[adapters]
enabled = ["codex"]

[dependencies]
nodus = { github = "nodus-rs/nodus", tag = "v0.3.2" }
```

Common dependency forms:

```toml
[dependencies]
playbook = { path = "vendor/playbook" }
tooling = { github = "owner/tooling", version = "^1.2.0" }
shared = { github = "owner/shared", tag = "v1.4.0", components = ["skills"] }
paused = { github = "owner/paused", tag = "v1.0.0", enabled = false }

[dev-dependencies]
internal = { path = "vendor/internal" }
```

Set `enabled = false` to keep a dependency declared in `nodus.toml` without resolving it, syncing its managed outputs, or tracking it in `nodus.lock`.

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
