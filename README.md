# Nodus

Nodus is a local-first Rust CLI for managing project-scoped agent packages by convention instead of explicit export configuration.

The current implementation discovers package content from repository folders, supports Git-tag dependencies backed entirely by a shared remote repository cache and shared cached checkouts, locks exact commits in `nodus.lock`, snapshots package contents into a content-addressed store, and emits managed runtime outputs for Claude, Codex, and OpenCode.

## Status

The current MVP supports:

- Zero-config package discovery from:
  - `skills/`
  - `agents/`
  - `rules/`
  - `commands/`
- Minimal root `nodus.toml`
- Persisted adapter selection in `nodus.toml`
- Git dependencies pinned by `tag` in the manifest and exact `rev` in the lockfile
- `nodus add <url>` with automatic latest-tag selection
- `nodus add <url> --tag <tag>` for explicit pinning
- `nodus add <url> --adapter <name>` / `nodus sync --adapter <name>` for explicit adapter installs
- Shared Git repository cache with shared cached checkouts by revision
- Shared content-addressed snapshots in the cache root
- Deterministic `nodus.lock`
- Managed output emission for the selected adapters:
  - `.claude/skills/<id>_<source-id>/`
  - `.claude/agents/<id>.md`
  - `.claude/commands/<id>.md`
  - `.claude/rules/<id>.md`
  - `.claude/.gitignore`
  - `.codex/skills/<id>_<source-id>/`
  - `.codex/rules/<id>.rules`
  - `.codex/.gitignore`
  - `.opencode/skills/<id>/`
  - `.opencode/agents/<id>.md`
  - `.opencode/commands/<id>.md`
  - `.opencode/rules/<id>.md`
  - `.opencode/.gitignore`
- Ownership tracking in `nodus.lock`
- Collision protection for unmanaged files
- Capability gating for high-sensitivity packages

Still deferred:

- Remote registries
- Publish flows
- Signature or provenance verification
- Global install scopes
- Claude plugin mode

## Install

Install the CLI with Cargo:

```bash
cargo install --path .
```

Then run it directly:

```bash
nodus <command>
```

By default, Nodus stores shared Git repository mirrors in the system cache directory for this app. On macOS, that is:

```text
~/Library/Caches/nodus/
```

You can override that location for any command with `--cache-path <path>`.

## Quick Start

Initialize a local package skeleton:

```bash
nodus init
```

That creates:

- `nodus.toml`
- `skills/example/SKILL.md`

Add a Git dependency by tag:

```bash
nodus add wenext-limited/playbook-ios
```

If the repo does not already contain adapter roots such as `.codex/`, `.claude/`, `.opencode/`, or `AGENTS.md`, pass `--adapter` the first time so Nodus can persist the choice:

```bash
nodus add wenext-limited/playbook-ios --adapter codex
```

Sync discovered content into managed runtime outputs:

```bash
nodus sync
```

If the root project declares any `high` sensitivity capabilities:

```bash
nodus sync --allow-high-sensitivity
```

Validate that the repo, shared cached dependencies, lockfile, and owned outputs are all consistent:

```bash
nodus doctor
```

Remove one configured dependency and prune its managed outputs:

```bash
nodus remove playbook_ios
```

For reproducible CI:

```bash
nodus sync --locked
```

Use a custom shared cache root when needed:

```bash
nodus --cache-path /tmp/nodus-cache sync
```

## Manifest

The root project does not need `api_version`, `name`, or `version` just to consume dependencies.

A minimal consumer manifest looks like:

```toml
[adapters]
enabled = ["codex"]

[dependencies]
playbook_ios = { github = "wenext-limited/playbook-ios", tag = "v0.1.0" }
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
- `[adapters]`
- `adapters.enabled`
- `[dependencies]`
- `dependencies.<alias>.github`
- `dependencies.<alias>.url`
- `dependencies.<alias>.path`
- `dependencies.<alias>.tag`

Unknown manifest fields are ignored with warnings.

### Adapter Selection

Nodus emits outputs only for the selected adapters. It resolves that selection in this order:

1. Explicit `--adapter <claude|codex|opencode>` flags on `nodus add` or `nodus sync`
2. Persisted `[adapters] enabled = [...]` in `nodus.toml`
3. Detected repo roots:
   - `.claude/` => Claude
   - `.codex/` => Codex
   - `.opencode/` or `AGENTS.md` => OpenCode
4. Interactive prompt on a TTY
5. Error with guidance in non-interactive environments

When Nodus resolves adapters from flags, detection, or a prompt, it writes `[adapters] enabled = [...]` into `nodus.toml` so later `sync`, `doctor`, and CI runs stay deterministic.

## Discovery Rules

Nodus validates and discovers package content by top-level folders:

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

## Commands

### `nodus add`

```bash
nodus add <url>
```

By default, Nodus resolves the latest Git tag, writes that tag into `nodus.toml`, and immediately runs a normal `nodus sync`.

You can still pin a specific tag explicitly:

```bash
nodus add <url> --tag <tag>
```

You can explicitly choose one or more adapters:

```bash
nodus add <url> --adapter codex
nodus add <url> --adapter claude --adapter opencode
```

You can also override the shared repository cache root for this command:

```bash
nodus --cache-path /tmp/nodus-cache add <url>
```

Behavior:

- accepts a full Git URL or a GitHub shortcut like `wenext-limited/playbook-ios`
- infers the dependency alias from the repo name
- fetches a shared bare mirror into the cache root
- materializes a shared cached checkout for the resolved revision under the cache root
- resolves the latest tag when `--tag` is omitted
- checks out the resolved tag
- validates the discovered package layout
- creates or updates `nodus.toml`
- persists adapter selection when it is inferred or explicitly provided

Example:

```bash
nodus add wenext-limited/playbook-ios
```

### `nodus init`

Creates an empty `nodus.toml` plus `skills/example/SKILL.md`.

### `nodus remove`

Removes one dependency from `nodus.toml` and runs the normal sync flow to update
`nodus.lock` and prune managed runtime files. The package argument accepts either the
dependency alias or a repository reference like `owner/repo`.

### `nodus sync`

Resolves the root project plus configured dependencies, snapshots their discovered content, writes `nodus.lock`, and emits managed runtime outputs.

Options:

- `--cache-path <path>`: override the shared Git repository cache root
- `--locked`: fail if `nodus.lock` would change
- `--allow-high-sensitivity`: allow packages that declare `high` sensitivity capabilities
- `--adapter <claude|codex|opencode>`: override and persist adapter selection for this repo

### `nodus doctor`

Checks that:

- the root manifest parses
- shared cached dependency checkouts exist in the cache root
- shared repository mirrors exist in the cache root with the expected origin URL
- discovered layouts are valid
- Git dependencies are at the expected locked revision
- `nodus.lock` is up to date
- managed file ownership entries are internally consistent
- no unmanaged-file collisions would block sync

## Managed Files

Nodus only manages files it wrote itself.

Managed files are tracked in `nodus.lock`. During sync, Nodus:

- writes or updates managed files
- removes stale managed files that are no longer desired
- refuses to overwrite existing unmanaged files

## Lockfile and Store

`nodus.lock` records:

- dependency alias
- source kind (`path` or `git`)
- source URL or path
- requested tag
- exact Git revision
- content digest
- discovered skills / agents / rules / commands
- declared capabilities
- managed runtime ownership entries

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

- Nodus emits only the selected adapters for the repo
- If multiple adapter roots are already present, Nodus installs all detected adapters
- Claude: discovered skills are copied to `.claude/skills/<skill-id>_<source-id>/`
- Claude: discovered agents are copied to `.claude/agents/<agent-id>.md`
- Claude: discovered commands are copied to `.claude/commands/<command-id>.md`
- Claude: discovered rules are copied to `.claude/rules/<rule-id>.md`
- Codex: discovered skills are copied to `.codex/skills/<skill-id>_<source-id>/`
- Codex: discovered rules are copied to `.codex/rules/<rule-id>.rules`
- OpenCode: discovered skills are copied to `.opencode/skills/<skill-id>/`
- OpenCode: discovered agents are copied to `.opencode/agents/<agent-id>.md`
- OpenCode: discovered commands are copied to `.opencode/commands/<command-id>.md`
- OpenCode: discovered rules are copied to `.opencode/rules/<rule-id>.md`

For skill folders, `<source-id>` is a short deterministic suffix:

- Git dependencies use the first 6 characters of the locked commit SHA
- Root and local-path packages use the first 6 characters of the package content digest

In `nodus.lock`, the hashed Claude and Codex skill outputs are tracked by stable logical roots such as `.claude/skills/<skill-id>` and `.codex/skills/<skill-id>`. During sync and doctor, Nodus expands each logical root back to the concrete hashed directory using the locked package source. OpenCode skills are tracked directly at `.opencode/skills/<skill-id>`.

For each selected runtime root, Nodus also writes a managed `.gitignore` file that ignores the generated runtime outputs inside that root.

## Development

Run the verification suite:

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```
