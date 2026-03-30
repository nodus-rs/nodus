<p align="center">
  <img src="./assets/nodus-mark.svg" alt="Nodus mark" width="144">
</p>

<h1 align="center">Nodus</h1>

<p align="center"><strong>Add agent packages to your repo with one command.</strong></p>

<p align="center">
  Nodus installs agent packages from GitHub, Git URLs, or local paths, locks the exact revision,
  and writes only the adapter runtime files your repo actually uses.
</p>

<p align="center">
  English • <a href="./README.cn.md">简体中文</a>
</p>

<p align="center">
  <a href="#install">Install</a> •
  <a href="#for-ai-assistants">For AI Assistants</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#cli-help">CLI Help</a> •
  <a href="#learn-more">Learn More</a> •
  <a href="./CONTRIBUTING.md">Contributing</a>
</p>

## What Is Nodus?

Nodus is a package manager for repo-scoped agent tooling.

If a package publishes content such as `skills/`, `agents/`, `rules/`, or `commands/`, Nodus can:

- add it from GitHub, Git, or a local path
- record what you asked for in `nodus.toml`
- lock the exact resolved revision in `nodus.lock`
- write managed files into adapter roots such as `.codex/`, `.claude/`, `.cursor/`, `.github/`, `.agents/`, or `.opencode/`
- prune stale generated files without touching unmanaged ones

For most teams, the normal flow is:

```bash
nodus add <package> --adapter <adapter>
nodus doctor
```

## Install

Install from crates.io:

```bash
cargo install nodus
```

Install the latest prebuilt binary on macOS or Linux:

```bash
curl -fsSL https://nodus.elata.ai/install.sh | bash
```

Install with Homebrew:

```bash
brew install nodus-rs/nodus/nodus
```

Install the latest prebuilt binary on Windows with PowerShell:

```powershell
irm https://nodus.elata.ai/install.ps1 | iex
```

<details>
<summary>Windows install command failed?</summary>

If the command fails, install PowerShell 7, restart your terminal, then run:

```powershell
winget install --id Microsoft.PowerShell --source winget
pwsh -NoProfile -Command "irm https://nodus.elata.ai/install.ps1 | iex"
```

</details>

## For AI Assistants

If you want an AI assistant to operate Nodus for you, give it the fetchable prompt URL:

- <https://nodus.elata.ai/prompts/README.md>

That prompt gives the assistant concrete Nodus operating instructions, helps it choose the right `nodus add` command, and still ends with `nodus doctor`.

## Quick Start

Install a package into the current repo and verify the result:

```bash
nodus add nodus-rs/nodus --adapter codex
nodus doctor
```

That flow:

- creates `nodus.toml` if the repo does not have one yet
- records the dependency in `nodus.toml`
- resolves and locks the exact revision in `nodus.lock`
- writes the managed runtime files for the selected or detected adapter

If the package is a wrapper that exposes multiple child packages, `nodus add` now records the
wrapper itself and leaves child packages disabled until you either edit `members` in
`nodus.toml` or opt in up front with `--accept-all-dependencies`.

If you want a user-level install instead of repo-scoped state, use `--global` explicitly:

```bash
nodus add nodus-rs/nodus --global --adapter codex
```

## CLI Help

`nodus --help` is the main command guide.

Start there when you want to learn the workflow, then open command-specific help as needed:

```bash
nodus --help
nodus add --help
nodus sync --help
nodus doctor --help
```

Commands most users need:

- `nodus add <package> --adapter <adapter>` to install a package into the current repo
- `nodus info <package-or-alias>` to inspect a package before or after install
- `nodus sync` to rebuild managed outputs from the versions already recorded
- `nodus update` to move dependencies to newer allowed revisions
- `nodus remove <alias>` to remove a dependency and prune what it owned
- `nodus clean` to clear shared repository, checkout, and snapshot cache data without changing project manifests or managed outputs
- `nodus doctor` to check that the repo, lockfile, shared store, and managed outputs still agree

## Learn More

- Docs: <https://nodus.elata.ai/docs/>
- Install guide: <https://nodus.elata.ai/install/>
- Package command generator: <https://nodus.elata.ai/packages/>
- Example manifest: [examples/nodus.toml](./examples/nodus.toml)

For package authoring details, workspace packaging, managed exports, or relay workflows, prefer the website docs and `nodus --help` over treating this README as the full command reference.

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for local development and release checks.

## License

Licensed under [Apache-2.0](./LICENSE).
