# Agen

Agen is a local-first Rust CLI for managing project-scoped agent packages.

In this MVP, Agen treats a repository as an `agentpack` package described by `agentpack.toml`, resolves local path dependencies, writes a deterministic `agentpack.lock`, snapshots package contents into a local content-addressed store, and emits managed runtime outputs for Claude, Codex, and OpenCode.

## Status

This repository currently implements the MVP slice:

- TOML manifest parsing and validation via `agentpack.toml`
- Deterministic lockfile generation via `agentpack.lock`
- Local path dependency resolution with cycle and version-conflict checks
- Project-local package snapshots under `.agen/store/sha256/`
- Managed output emission for:
  - `.claude/skills/<id>/`
  - `.codex/skills/<id>/`
  - `.codex/rules/<id>.rules`
  - `.opencode/instructions/<id>.md`
  - `opencode.json`
- Ownership tracking via `.agen/state.json`
- Collision protection for unmanaged files
- Capability gating for high-sensitivity packages

Not implemented yet:

- Remote registries
- Publish flows
- Signing or provenance verification
- User/global install scopes
- Claude plugin mode
- OpenCode executable plugin exports
- General dependency solving beyond local path resolution

## Install

Build the CLI with Cargo:

```bash
cargo build
```

Run it with:

```bash
cargo run -- <command>
```

## Quick Start

Create a starter package in the current repository:

```bash
cargo run -- init
```

That scaffolds:

- `agentpack.toml`
- `skills/example/SKILL.md`

Then sync the package into managed runtime outputs:

```bash
cargo run -- sync
```

If your package declares any `high` sensitivity capabilities, sync requires explicit opt-in:

```bash
cargo run -- sync --allow-high-sensitivity
```

Validate that the repo is consistent with the current manifest and lockfile:

```bash
cargo run -- doctor
```

For CI or reproducible local checks, use:

```bash
cargo run -- sync --locked
```

## Manifest

The only supported manifest format in the MVP is `agentpack.toml`.

Minimal example:

```toml
api_version = "agentpack/v0"
name = "example-pack"
version = "0.1.0"

[[exports.skills]]
id = "review"
path = "skills/review"
```

More complete example:

```toml
api_version = "agentpack/v0"
name = "acme-dev-standards"
version = "0.1.0"

[[exports.skills]]
id = "review"
path = "skills/review"

[[exports.agents]]
id = "security-reviewer"
path = "agents/security-reviewer.md"

[[exports.rules]]
id = "default"

[[exports.rules.sources]]
type = "codex.ruleset"
path = "rules/default.rules"

[[capabilities]]
id = "shell.exec"
sensitivity = "high"
justification = "Run repository checks."

[dependencies.agentpacks.shared]
path = "vendor/shared"
requirement = "^1.0.0"
```

### Supported Fields

- `api_version`
- `name`
- `version`
- `exports.skills`
- `exports.agents`
- `exports.rules`
- `capabilities`
- `dependencies.agentpacks.<name>.path`
- `dependencies.agentpacks.<name>.requirement`

Unsupported manifest fields are currently ignored with warnings.

### Skill Validation

Each exported skill must point to a directory containing `SKILL.md` with YAML frontmatter that includes:

- `name`
- `description`

## Commands

### `agen init`

Scaffolds a starter `agentpack.toml` plus an example skill in `skills/example/`.

`init` refuses to overwrite an existing manifest or example skill.

### `agen sync`

Resolves the local package graph, snapshots package contents, writes `agentpack.lock`, and emits managed runtime outputs into the current repository.

Options:

- `--locked`: fail if `agentpack.lock` would change
- `--allow-high-sensitivity`: allow packages that declare `high` sensitivity capabilities

### `agen doctor`

Checks that:

- `agentpack.toml` is valid
- dependencies resolve successfully
- `agentpack.lock` exists and is up to date
- managed file ownership state is internally consistent
- no unmanaged-file collisions would block sync

## Managed Files

Agen only manages files it wrote itself.

Files owned by Agen are recorded in `.agen/state.json`. During sync, Agen:

- writes or updates managed files
- removes stale managed files that are no longer part of the desired state
- refuses to overwrite existing unmanaged files

This is especially important for OpenCode. Agen manages instruction files under `.opencode/instructions/` and `opencode.json`, but it does not overwrite a top-level `AGENTS.md`.

## Lockfile and Store

`agentpack.lock` captures the resolved package graph, including:

- package name and version
- source path
- content digest
- exported IDs
- dependencies
- declared capabilities

Resolved packages are snapshotted into the local store:

```text
.agen/store/sha256/<digest>/
```

Adapters read from these snapshots rather than directly from mutable working trees.

## Runtime Output Mapping

Current adapter behavior:

- Claude: exported skills are copied to `.claude/skills/<skill-id>/`
- Codex: exported skills are copied to `.codex/skills/<skill-id>/`
- Codex: `codex.ruleset` sources are emitted to `.codex/rules/<rule-id>.rules`
- OpenCode: exported agents are emitted to `.opencode/instructions/<agent-id>.md`
- OpenCode: managed instruction paths are written to `opencode.json`

## Development

Run the verification suite:

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

The current implementation is intentionally single-crate, but the internal modules are already split by responsibility to make a later workspace extraction straightforward.
