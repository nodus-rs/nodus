# Nodus

Nodus is a local-first Rust CLI for managing project-scoped agent packages.

It lets a repository publish agent assets by convention, resolves those packages from Git tags or local paths, locks exact revisions in `nodus.lock`, snapshots resolved content into a shared local store, and emits managed runtime files for Claude, Codex, and OpenCode.

## Why Nodus

Agent customization tends to drift because every runtime expects a different on-disk layout. Nodus gives teams one package shape and one sync flow:

- Discover package content from conventional folders:
  - `skills/`
  - `agents/`
  - `rules/`
  - `commands/`
- Pin direct dependencies by Git tag in `nodus.toml`
- Lock exact Git revisions and managed outputs in `nodus.lock`
- Reuse a shared store of repository mirrors, checkouts, and content-addressed snapshots across projects
- Emit only the runtime outputs your repo actually needs
- Protect unmanaged files from accidental overwrite
- Gate high-sensitivity packages behind explicit opt-in

## Current Scope

Nodus currently supports:

- Local path dependencies
- Git dependencies resolved from tags
- Deterministic sync with lock state stored in `nodus.lock`
- Managed output emission for Claude, Codex, and OpenCode
- Repo-level adapter selection that can be inferred, chosen explicitly, or persisted
- Validation of shared store state, lockfile state, and managed files with `nodus doctor`

Not implemented yet:

- Remote registries
- Package publishing workflows
- Signature or provenance verification
- Global install scopes
- Claude plugin mode

## Install

Install the released crate from crates.io:

```bash
cargo install nodus
```

Build or install from the current checkout:

```bash
cargo install --path .
```

After installation, run:

```bash
nodus <command>
```

By default, Nodus stores shared mirrors, checkouts, and snapshots in the platform's local application data directory:

```text
macOS:   ~/Library/Application Support/nodus/
Linux:   ~/.local/state/nodus/              (or $XDG_STATE_HOME/nodus/)
Windows: %LOCALAPPDATA%\nodus\
```

You can override that location for any command with `--store-path <path>`.

## Quick Start

Initialize a repo that will consume agent packages:

```bash
nodus init
```

That creates:

- `nodus.toml`
- `skills/example/SKILL.md`

Add a dependency from Git:

```bash
nodus add obra/superpowers --adapter codex
```

That command:

- resolves the latest tag unless you pass `--tag`
- records the dependency in `nodus.toml`
- persists adapter selection when needed
- runs a normal sync immediately

Sync dependencies into managed runtime outputs:

```bash
nodus sync
```

Validate that the repo, lockfile, managed outputs, and shared store are consistent:

```bash
nodus doctor
```

For reproducible CI:

```bash
nodus sync --locked
```

If the root project declares any `high` sensitivity capabilities, opt in explicitly:

```bash
nodus sync --allow-high-sensitivity
```

Remove a configured dependency and prune its managed outputs:

```bash
nodus remove superpowers
```

Use a custom shared store root when needed:

```bash
nodus --store-path /tmp/nodus-store sync
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the local development workflow and release checks.

## License

Licensed under [Apache-2.0](LICENSE).

## Manifest

The root project does not need `api_version`, `name`, or `version` just to consume dependencies.

A minimal consumer manifest looks like:

```toml
[adapters]
enabled = ["codex"]

[dependencies]
superpowers = { github = "obra/superpowers", tag = "v0.1.0" }
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

## Package Discovery

Nodus validates and discovers package content by top-level folders:

- `skills/<id>/SKILL.md` => skill
- `agents/<id>.md` => agent
- `rules/<id>.*` => rule
- `commands/<id>.*` => command

When you run Nodus in a repo root, those folders are treated as package source for consumers of that repo. Nodus does not mirror the root project's own `skills/`, `agents/`, `rules/`, or `commands/` into managed runtime folders like `.codex/` or `.claude/`; managed outputs are emitted only for resolved dependencies.

Package validity rules:

- A dependency repo must contain at least one of `skills/`, `agents/`, `rules/`, or `commands/`, or declare at least one dependency in `nodus.toml`
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

You can also override the shared repository store root for this command:

```bash
nodus --store-path /tmp/nodus-store add <url>
```

Behavior:

- accepts a full Git URL or a GitHub shortcut like `obra/superpowers`
- infers the dependency alias from the repo name
- fetches a shared bare mirror into the shared store root
- materializes a shared checkout for the resolved revision under the shared store root
- resolves the latest tag when `--tag` is omitted
- checks out the resolved tag
- validates the discovered package layout or dependency wrapper manifest
- creates or updates `nodus.toml`
- records only the direct dependency you added in the caller manifest
- lets the normal sync flow recursively resolve dependencies declared by the remote repo's `nodus.toml`
- persists adapter selection when it is inferred or explicitly provided

Example:

```bash
nodus add obra/superpowers
```

### `nodus init`

Creates a minimal `nodus.toml` plus `skills/example/SKILL.md`.

### `nodus remove`

Removes one dependency from `nodus.toml` and runs the normal sync flow to update
`nodus.lock` and prune managed runtime files. The package argument accepts either the
dependency alias or a repository reference like `owner/repo`.

### `nodus sync`

Resolves the root project plus configured dependencies, recursively follows nested dependencies declared in dependency manifests, snapshots their discovered content, writes `nodus.lock`, and emits managed runtime outputs for resolved dependencies.

Options:

- `--store-path <path>`: override the shared repository store root
- `--locked`: fail if `nodus.lock` would change
- `--allow-high-sensitivity`: allow packages that declare `high` sensitivity capabilities
- `--adapter <claude|codex|opencode>`: override and persist adapter selection for this repo

### `nodus doctor`

Checks that:

- the root manifest parses
- shared dependency checkouts exist in the shared store root
- shared repository mirrors exist in the shared store root with the expected origin URL
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
<store-root>/store/sha256/<digest>/
```

Sync emits from those snapshots rather than directly from mutable working trees.

## Shared Store

Shared dependency state uses three on-disk locations:

- Shared remote mirrors live under `<store-root>/repositories/<repo-name>-<url-hash>.git`
- Shared checkouts live under `<store-root>/checkouts/<repo-name>-<url-hash>/<rev>/`
- Shared content-addressed snapshots live under `<store-root>/store/sha256/<digest>/`

This keeps fetched repositories, materialized checkouts, and package snapshots shared across projects. Project-specific state stays limited to each repo's lockfile and emitted runtime outputs.

## Runtime Output Mapping

Current adapter behavior:

- Nodus emits only the selected adapters for the repo
- If multiple adapter roots are already present, Nodus installs all detected adapters
- Claude: discovered skills are copied to `.claude/skills/<skill-id>_<source-id>/`
- Claude: discovered agents are copied to `.claude/agents/<agent-id>_<source-id>.md`
- Claude: discovered commands are copied to `.claude/commands/<command-id>_<source-id>.md`
- Claude: discovered rules are copied to `.claude/rules/<rule-id>_<source-id>.md`
- Codex: discovered skills are copied to `.codex/skills/<skill-id>_<source-id>/`
- Codex: discovered rules are copied to `.codex/rules/<rule-id>_<source-id>.rules`
- OpenCode: discovered skills are copied to `.opencode/skills/<skill-id>_<source-id>/`
- OpenCode: discovered agents are copied to `.opencode/agents/<agent-id>_<source-id>.md`
- OpenCode: discovered commands are copied to `.opencode/commands/<command-id>_<source-id>.md`
- OpenCode: discovered rules are copied to `.opencode/rules/<rule-id>_<source-id>.md`

For managed directories and files, `<source-id>` is a short deterministic suffix:

- Git dependencies use the first 6 characters of the locked commit SHA
- Root and local-path packages use the first 6 characters of the package content digest

In `nodus.lock`, managed runtime outputs are tracked by stable logical roots such as `.claude/skills/<skill-id>`, `.claude/agents/<agent-id>.md`, `.codex/rules/<rule-id>.rules`, and `.opencode/commands/<command-id>.md`. During sync and doctor, Nodus expands each logical path back to the concrete suffixed directory or file using the locked package source.

For each selected runtime root, Nodus also writes a managed `.gitignore` file that ignores both itself and the generated runtime outputs inside that root.

## Development

Run the verification suite:

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```
