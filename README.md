# Agen

Agen is a local-first Rust CLI for managing project-scoped agent packages by convention instead of explicit export configuration.

The current implementation discovers package content from repository folders, supports Git-tag dependencies backed entirely by a shared remote repository cache and shared cached checkouts, locks exact commits in `agentpack.lock`, snapshots package contents into a content-addressed store, and emits managed runtime outputs for Claude, Codex, and OpenCode.

## Status

The current MVP supports:

- Zero-config package discovery from:
  - `skills/`
  - `agents/`
  - `rules/`
  - `commands/`
- Minimal root `agentpack.toml`
- Git dependencies pinned by `tag` in the manifest and exact `rev` in the lockfile
- `agen add <url>` with automatic latest-tag selection
- `agen add <url> --tag <tag>` for explicit pinning
- Shared Git repository cache with shared cached checkouts by revision
- Shared content-addressed snapshots in the cache root
- Deterministic `agentpack.lock`
- Managed output emission for:
  - `.claude/skills/<id>_<source-id>/`
  - `.codex/skills/<id>_<source-id>/`
  - `.codex/rules/<id>.rules`
  - `.opencode/instructions/<id>.md`
  - `opencode.json`
- Ownership tracking in `agentpack.lock`
- Collision protection for unmanaged files
- Capability gating for high-sensitivity packages

Still deferred:

- Remote registries
- Publish flows
- Signature or provenance verification
- Global install scopes
- Claude plugin mode
- Runtime emission for discovered `commands/`

## Install

Install the CLI with Cargo:

```bash
cargo install --path .
```

Then run it directly:

```bash
agen <command>
```

By default, Agen stores shared Git repository mirrors in the system cache directory for this app. On macOS, that is:

```text
~/Library/Caches/agen/
```

You can override that location for any command with `--cache-path <path>`.

## Quick Start

Initialize a local package skeleton:

```bash
agen init
```

That creates:

- `agentpack.toml`
- `skills/example/SKILL.md`

Add a Git dependency by tag:

```bash
agen add wenext-limited/playbook-ios
```

Sync discovered content into managed runtime outputs:

```bash
agen sync
```

If the root project declares any `high` sensitivity capabilities:

```bash
agen sync --allow-high-sensitivity
```

Validate that the repo, shared cached dependencies, lockfile, and owned outputs are all consistent:

```bash
agen doctor
```

Remove one configured dependency and prune its managed outputs:

```bash
agen uninstall playbook_ios
```

For reproducible CI:

```bash
agen sync --locked
```

Use a custom shared cache root when needed:

```bash
agen --cache-path /tmp/agen-cache sync
```

## Manifest

The root project does not need `api_version`, `name`, or `version` just to consume dependencies.

A minimal consumer manifest looks like:

```toml
[dependencies]
playbook_ios = { url = "https://github.com/wenext-limited/playbook-ios", tag = "v0.1.0" }
```

You can also use local path dependencies:

```toml
[dependencies]
local_playbook = { path = "vendor/playbook", tag = "v0.1.0" }
```

Optional capabilities are still supported:

```toml
[[capabilities]]
id = "shell.exec"
sensitivity = "high"
justification = "Run repository checks."
```

### Supported Fields

- `api_version` (optional)
- `name` (optional)
- `version` (optional)
- `capabilities`
- `[dependencies]`
- `dependencies.<alias>.url`
- `dependencies.<alias>.path`
- `dependencies.<alias>.tag`

Unknown manifest fields are ignored with warnings.

## Discovery Rules

Agen validates and discovers package content by top-level folders:

- `skills/<id>/SKILL.md` => skill
- `agents/<id>.md` => agent
- `rules/<id>.*` => rule
- `commands/<id>.*` => command

Package validity rules:

- A dependency repo must contain at least one of `skills/`, `agents/`, `rules/`, or `commands/`
- Other files and directories are allowed and ignored
- `skills/` entries must be directories
- Each skill must contain `SKILL.md` with YAML frontmatter containing:
  - `name`
  - `description`
- `agents/` entries must be `.md` files
- `rules/` and `commands/` entries must be files

Discovered `commands/` content is currently validated and locked, but not emitted to any runtime yet.

## Commands

### `agen add`

```bash
agen add <url>
```

By default, Agen resolves the latest Git tag, writes that tag into `agentpack.toml`, and immediately runs a normal `agen sync`.

You can still pin a specific tag explicitly:

```bash
agen add <url> --tag <tag>
```

You can also override the shared repository cache root for this command:

```bash
agen --cache-path /tmp/agen-cache add <url>
```

Behavior:

- accepts a full Git URL or a GitHub shortcut like `wenext-limited/playbook-ios`
- infers the dependency alias from the repo name
- fetches a shared bare mirror into the cache root
- materializes a shared cached checkout for the resolved revision under the cache root
- resolves the latest tag when `--tag` is omitted
- checks out the resolved tag
- validates the discovered package layout
- creates or updates `agentpack.toml`

Example:

```bash
agen add wenext-limited/playbook-ios
```

### `agen init`

Creates an empty `agentpack.toml` plus `skills/example/SKILL.md`.

### `agen uninstall`

Removes one dependency from `agentpack.toml` and runs the normal sync flow to update
`agentpack.lock` and prune managed runtime files. The package argument accepts either the
dependency alias or a repository reference like `owner/repo`.

### `agen sync`

Resolves the root project plus configured dependencies, snapshots their discovered content, writes `agentpack.lock`, and emits managed runtime outputs.

Options:

- `--cache-path <path>`: override the shared Git repository cache root
- `--locked`: fail if `agentpack.lock` would change
- `--allow-high-sensitivity`: allow packages that declare `high` sensitivity capabilities

### `agen doctor`

Checks that:

- the root manifest parses
- shared cached dependency checkouts exist in the cache root
- shared repository mirrors exist in the cache root with the expected origin URL
- discovered layouts are valid
- Git dependencies are at the expected locked revision
- `agentpack.lock` is up to date
- managed file ownership entries are internally consistent
- no unmanaged-file collisions would block sync

## Managed Files

Agen only manages files it wrote itself.

Managed files are tracked in `agentpack.lock`. During sync, Agen:

- writes or updates managed files
- removes stale managed files that are no longer desired
- refuses to overwrite existing unmanaged files

This is especially important for OpenCode. Agen manages `.opencode/instructions/` and `opencode.json`, but it does not overwrite a top-level `AGENTS.md`.

## Lockfile and Store

`agentpack.lock` records:

- dependency alias
- source kind (`path` or `git`)
- source URL or path
- requested tag
- exact Git revision
- content digest
- discovered skills / agents / rules / commands
- declared capabilities
- managed file paths

Resolved packages are snapshotted under:

```text
<cache-root>/store/sha256/<digest>/
```

Sync emits from those snapshots rather than directly from mutable working trees.

## Shared Cache

Cached dependency data uses three on-disk locations:

- Shared remote mirrors live under `<cache-root>/repositories/<repo-name>-<url-hash>.git`
- Shared cached checkouts live under `<cache-root>/checkouts/<repo-name>-<url-hash>/<rev>/`
- Shared content-addressed snapshots live under `<cache-root>/store/sha256/<digest>/`

This keeps fetched repositories, materialized checkouts, and package snapshots shared across projects. Project-specific state stays limited to each repo's lockfile and emitted runtime outputs.

## Runtime Output Mapping

Current adapter behavior:

- Claude: discovered skills are copied to `.claude/skills/<skill-id>_<source-id>/`
- Codex: discovered skills are copied to `.codex/skills/<skill-id>_<source-id>/`
- Codex: discovered rules are copied to `.codex/rules/<rule-id>.rules`
- OpenCode: discovered agents are copied to `.opencode/instructions/<agent-id>.md`
- OpenCode: managed instruction paths are written to `opencode.json`

For skill folders, `<source-id>` is a short deterministic suffix:

- Git dependencies use the first 6 characters of the locked commit SHA
- Root and local-path packages use the first 6 characters of the package content digest
- Commands: discovered and locked, but not emitted

## Development

Run the verification suite:

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```
