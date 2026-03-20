<p align="center">
  <img src="./assets/nodus-mark.svg" alt="Nodus mark" width="144">
</p>

<h1 align="center">Nodus</h1>

<p align="center"><strong>Add agent packages to your repo with one command.</strong></p>

<p align="center">
  Resolve from Git refs or local paths, lock exact revisions, snapshot package contents,
  and emit managed runtime files for Claude, Codex, and OpenCode.
</p>

<p align="center">
  English • <a href="./README.cn.md">简体中文</a>
</p>

<p align="center">
  <a href="#install">Install</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#commands">Commands</a> •
  <a href="#manifest">Manifest</a> •
  <a href="./CONTRIBUTING.md">Contributing</a>
</p>

## What Is Nodus?

Nodus is for the repo that wants to consume agent packages without stitching runtime folders together by hand.

Point it at a GitHub repo or local path and Nodus will resolve the package, pin the dependency, lock the exact revision in `nodus.lock`, snapshot the package into a shared local store, and write only the managed files your selected adapters need.

```bash
nodus add WendellXY/nodus --adapter codex
nodus add WendellXY/nodus --dev --adapter codex
nodus add WendellXY/nodus --adapter claude --component skills
nodus info WendellXY/nodus
nodus outdated
nodus update
nodus relay nodus --repo-path ../nodus
nodus doctor
nodus completion zsh > ~/.zsh/completions/_nodus
```

The install flow is designed to stay predictable:

- `nodus add` records the dependency and runs sync immediately
- `nodus info` prints resolved metadata for a dependency alias, local package path, or Git reference
- `nodus.lock` captures the exact Git revision and managed outputs
- managed files are pruned when they go stale
- unmanaged files are never overwritten
- high-sensitivity packages require explicit opt-in

Package authors can still publish content from `skills/`, `agents/`, `rules/`, and `commands/`, but as a consumer you mostly interact with `nodus add`, `nodus info`, `nodus outdated`, `nodus update`, `nodus relay`, `nodus sync`, and `nodus doctor`.

## Install

Install the released crate from crates.io:

```bash
cargo install nodus
```

Install the latest prebuilt binary on macOS or Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/WendellXY/nodus/main/install.sh | bash
```

Install a specific release or choose a custom install directory:

```bash
curl -fsSL https://raw.githubusercontent.com/WendellXY/nodus/main/install.sh | bash -s -- --version v0.1.0
curl -fsSL https://raw.githubusercontent.com/WendellXY/nodus/main/install.sh | bash -s -- --install-dir /usr/local/bin
```

Verify the downloaded archive when the release includes checksum assets:

```bash
curl -fsSL https://raw.githubusercontent.com/WendellXY/nodus/main/install.sh | bash -s -- --verify
```

Uninstall from the default or a custom install directory:

```bash
curl -fsSL https://raw.githubusercontent.com/WendellXY/nodus/main/install.sh | bash -s -- --uninstall
curl -fsSL https://raw.githubusercontent.com/WendellXY/nodus/main/install.sh | bash -s -- --uninstall --install-dir /usr/local/bin
```

You can also download a prebuilt binary archive from the GitHub release assets for your platform, then run the root-level `install.sh` locally.

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

If the repo does not have a manifest yet:

```bash
nodus init
```

Then add a package:

```bash
nodus add WendellXY/nodus --adapter codex
```

To install only selected artifact kinds from that package:

```bash
nodus add WendellXY/nodus --adapter claude --component skills --component rules
```

That one command:

- resolves the latest tag unless you pass `--tag`, `--branch`, or `--revision`
- resolves the highest compatible semver tag when you pass `--version`
- writes the dependency to `nodus.toml`
- persists adapter selection when needed
- locks exact state in `nodus.lock`
- emits managed files under the selected runtime root

Validate the result:

```bash
nodus doctor
```

For repeatable CI:

```bash
nodus sync --locked
```

To install the exact Git revisions already recorded in `nodus.lock` without following newer branch heads:

```bash
nodus sync --frozen
```

When a package declares `high` sensitivity capabilities:

```bash
nodus sync --allow-high-sensitivity
```

Use a custom shared store root when needed:

```bash
nodus --store-path /tmp/nodus-store sync
```

Remove a configured dependency and prune its managed outputs:

```bash
nodus remove nodus
```

If you maintain a dependency repo locally and want to relay managed edits back into that checkout:

```bash
nodus relay nodus --repo-path ../nodus
```

After setup, your repo has a pinned dependency in `nodus.toml`, exact resolved state in `nodus.lock`, and managed runtime files under the adapter root you selected.

Generate shell completions when you want tab completion for the CLI:

```bash
nodus completion bash
nodus completion zsh
nodus completion fish
```

## Why Teams Use Nodus

- Add a package from Git or a local path without manually copying files into `.agents/`, `.codex/`, `.claude/`, `.cursor/`, or `.opencode/`
- Install once and emit only the runtime outputs your repo actually uses
- Reuse shared mirrors, checkouts, and content-addressed snapshots across projects
- Keep generated files under explicit ownership so stale outputs can be pruned safely
- Verify install state with `nodus doctor` and enforce it in CI with `nodus sync --locked`

## Available Today

Nodus currently supports:

- Local path dependencies
- Git dependencies resolved from tags or branches
- Deterministic sync with lock state stored in `nodus.lock`
- Managed output emission for Agents, Claude, Codex, Cursor, and OpenCode
- Repo-level adapter selection that can be inferred, chosen explicitly, or persisted
- Validation of shared store state, lockfile state, and managed files with `nodus doctor`

Planned later:

- Remote registries
- Package publishing workflows
- Signature or provenance verification
- Global install scopes
- Claude plugin mode

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
nodus = { github = "WendellXY/nodus", tag = "v0.3.2" }
```

If this repo also publishes AI-plugin assets from additional folders, you can
declare additive content roots and optionally mirror the root project's own
discovered assets into local runtime folders:

```toml
content_roots = ["nodus-development"]
publish_root = true
```

Each `content_roots` entry is resolved relative to the repo root and is scanned
as another package source root containing optional `skills/`, `agents/`,
`rules/`, and `commands/` subfolders.

You can optionally filter which artifact kinds a dependency contributes:

```toml
[dependencies]
nodus = { github = "WendellXY/nodus", tag = "v0.3.2", components = ["skills"] }
```

You can also use local path dependencies:

```toml
[dependencies]
local_playbook = { path = "vendor/playbook" }
```

Use `[dev-dependencies]` for packages that should resolve in the current repo but stay private when this package is consumed by another Nodus workspace:

```toml
[dev-dependencies]
tooling = { path = "vendor/tooling" }
```

You can also manage a Git dependency by a Cargo-style semver requirement instead of pinning an
exact tag:

```toml
[dependencies]
axiom = { github = "CharlesWiltgen/Axiom", version = "^2.34.0" }
```

Nodus resolves the highest compatible Git tag, records the exact resolved tag and revision in
`nodus.lock`, and leaves the manifest requirement unchanged on future compatible updates.

You can also pin a dependency to an exact Git commit:

```toml
[dependencies]
nodus = { github = "WendellXY/nodus", revision = "0123456789abcdef0123456789abcdef01234567" }
```

You can also declare direct managed file or directory mappings for a root-manifest dependency:

```toml
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"

[[dependencies.shared.managed]]
source = "templates"
target = "docs/templates"
```

`managed.source` is resolved relative to the dependency root. `managed.target` is resolved relative to the consuming repo root. Both paths must be relative, and `managed` is supported only for direct dependencies declared in the root `nodus.toml`.

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
- `content_roots`
- `publish_root`
- `capabilities`
- `[adapters]`
- `adapters.enabled`
- `[dependencies]`
- `[dev-dependencies]`
- `dependencies.<alias>.github`
- `dependencies.<alias>.url`
- `dependencies.<alias>.path`
- `dependencies.<alias>.tag`
- `dependencies.<alias>.branch`
- `dependencies.<alias>.revision`
- `dependencies.<alias>.version`
- `dependencies.<alias>.components`
- `[[dependencies.<alias>.managed]]`
- `dependencies.<alias>.managed.source`
- `dependencies.<alias>.managed.target`

Unknown manifest fields are ignored with warnings.

For a fully commented example manifest, see [examples/nodus.toml](./examples/nodus.toml).

### Adapter Selection

Nodus emits outputs only for the selected adapters. It resolves that selection in this order:

1. Explicit `--adapter <agents|claude|codex|cursor|opencode>` flags on `nodus add` or `nodus sync`
2. Persisted `[adapters] enabled = [...]` in `nodus.toml`
3. Detected repo roots:
   - `.agents/` => Agents
   - `.claude/` => Claude
   - `.codex/` => Codex
   - `.cursor/` => Cursor
   - `.opencode/` or `AGENTS.md` => OpenCode
4. Interactive prompt on a TTY
5. Error with guidance in non-interactive environments

When Nodus resolves adapters from flags, detection, or a prompt, it writes `[adapters] enabled = [...]` into `nodus.toml` so later `sync`, `doctor`, and CI runs stay deterministic.

## Package Discovery

Nodus validates and discovers package content from the repo root and any
configured `content_roots` by looking for these folders inside each discovery
root:

- `skills/<id>/SKILL.md` => skill
- `agents/<id>.md` => agent
- `rules/<id>.*` => rule
- `commands/<id>.*` => command

When you run Nodus in a repo root, those folders are treated as package source
for consumers of that repo. By default, Nodus does not mirror the root
project's own discovered `skills/`, `agents/`, `rules/`, or `commands/` into
managed runtime folders like `.codex/` or `.claude/`; set `publish_root = true`
to opt into publishing the root project's discovered assets alongside resolved
dependencies. Artifact ids must remain unique per kind across all discovery
roots.

Package validity rules:

- A dependency repo must contain at least one discovered `skills/`, `agents/`, `rules/`, or `commands/` entry across the repo root plus any configured `content_roots`, or declare at least one dependency in `nodus.toml`
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

Or track a specific branch or exact commit:

```bash
nodus add <url> --branch <branch>
nodus add <url> --revision <commit>
```

Or declare a semver requirement and let Nodus resolve the highest compatible tag:

```bash
nodus add <url> --version '^1.2.0'
```

You can explicitly choose one or more adapters:

```bash
nodus add <url> --adapter codex
nodus add <url> --adapter claude --adapter opencode
```

You can also restrict the dependency to specific component kinds:

```bash
nodus add <url> --component skills
nodus add <url> --component skills --component agents
```

You can also override the shared repository store root for this command:

```bash
nodus --store-path /tmp/nodus-store add <url>
```

Behavior:

- accepts a full Git URL or a GitHub shortcut like `WendellXY/nodus`
- infers the dependency alias from the repo name
- fetches a shared bare mirror into the shared store root
- materializes a shared checkout for the resolved revision under the shared store root
- resolves the latest tag when no Git selector is provided
- writes either `tag`, `branch`, `revision`, or `version` into `nodus.toml`
- validates the discovered package layout or dependency wrapper manifest
- creates or updates `nodus.toml`
- records only the direct dependency you added in the caller manifest
- lets the normal sync flow recursively resolve dependencies declared by the remote repo's `nodus.toml`
- persists adapter selection when it is inferred or explicitly provided
- persists dependency component selection when `--component` is provided

Example:

```bash
nodus add WendellXY/nodus
```

### `nodus init`

Creates a minimal `nodus.toml` plus `skills/example/SKILL.md`.

### `nodus info`

```bash
nodus info <package>
```

Displays resolved package metadata without modifying the current project.

Examples:

```bash
nodus info WendellXY/nodus
nodus info ./vendor/nodus
nodus info nodus
nodus info WendellXY/nodus --tag v0.3.2
nodus info WendellXY/nodus --branch main
```

Behavior:

- accepts a dependency alias from the current repo, a local package directory, a full Git URL, or a GitHub shortcut like `owner/repo`
- resolves a direct dependency alias using the source pinned in the current repo's `nodus.toml`
- inspects local package directories directly when no Git ref override is provided
- resolves the latest Git tag when inspecting a Git reference without `--tag` or `--branch`
- falls back to the default branch when a Git repository has no tags
- prints the resolved source, package root, selected components, discovered artifact ids, dependencies, adapters, declared capabilities, and any dependency semver requirement recorded in the current repo

### `nodus remove`

Removes one dependency from `nodus.toml` and runs the normal sync flow to update
`nodus.lock` and prune managed runtime files. The package argument accepts either the
dependency alias or a repository reference like `owner/repo`.

### `nodus outdated`

Checks configured dependencies from `nodus.toml`, including `[dev-dependencies]`, for newer upstream tags or branch head changes.

Behavior:

- tagged Git dependencies are compared against the newest available tag in the shared mirror
- semver-managed Git dependencies report the locked tag, the highest compatible tag, and the latest overall semver tag
- branch Git dependencies are compared against the currently locked revision in `nodus.lock`
- path dependencies are reported as local paths and are never marked outdated

### `nodus update`

Updates configured dependencies from `nodus.toml`, including `[dev-dependencies]`, and then runs the normal sync flow.

Behavior:

- tagged Git dependencies are rewritten to the newest available tag
- semver-managed Git dependencies keep their manifest `version` requirement and update only the locked tag and revision to the highest compatible release
- branch Git dependencies keep their branch pin and refresh to the latest branch head
- path dependencies are left as local paths and included in the normal sync pass
- `--allow-high-sensitivity` mirrors `nodus sync` for projects that already opt into high-sensitivity capabilities

### `nodus relay`

```bash
nodus relay <dependency>... [--repo-path <path>] [--via <adapter>] [--watch]
```

Relays edits from managed runtime outputs like `.codex/`, `.claude/`, `.cursor/`, `.agents/`, and `.opencode/` back into a maintainer-owned local checkout of the direct Git dependency.

Behavior:

- works only for direct Git dependencies from `nodus.toml`
- requires a current `nodus.lock` and uses the locked snapshot as the relay baseline
- persists maintainer linkage in `.nodus/local.toml`
- accepts multiple dependencies in one invocation and relays each one using its persisted link
- `--via <adapter>` persists a preferred adapter hint in `.nodus/local.toml` when relay metadata should remember which adapter to treat as canonical; aliases: `--relay-via`, `--prefer`
- `--repo-path <path>` still applies to exactly one dependency, because each relay link points at one maintainer checkout
- writes `.nodus/.gitignore` so the local relay config stays untracked
- validates that the linked checkout is a Git repo whose `origin` matches the dependency URL
- writes only changed source files into the linked checkout; it does not commit or push
- with `--watch`, keeps polling the managed outputs and relays new edits automatically until you stop the command; multi-dependency watch uses each dependency's persisted relay link
- fails when managed variants disagree or when both the linked source and managed output changed since the locked baseline

Example:

```bash
nodus relay nodus --repo-path ../nodus
nodus relay nodus internal-tools docs-kit
nodus relay nodus --via claude
nodus relay nodus internal-tools --watch
nodus relay nodus --watch
```

### `nodus sync`

Resolves the root project plus configured dependencies, recursively follows nested dependencies declared in dependency manifests, snapshots their discovered content, writes `nodus.lock`, and emits managed runtime outputs for resolved dependencies plus the root project when `publish_root = true`.

Options:

- `--store-path <path>`: override the shared repository store root
- `--locked`: fail if `nodus.lock` would change
- `--frozen`: install exact Git revisions from `nodus.lock` and fail if the lockfile is missing or stale
- `--allow-high-sensitivity`: allow packages that declare `high` sensitivity capabilities
- `--adapter <agents|claude|codex|cursor|opencode>`: override and persist adapter selection for this repo

When a dependency has a relay link in `.nodus/local.toml`, `sync` fails instead of overwriting pending managed edits that have not been relayed yet.

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
- refuses to overwrite pending relay edits for dependencies linked through `.nodus/local.toml`

## Lockfile and Store

`nodus.lock` records:

- dependency alias
- source kind (`path` or `git`)
- source URL or path
- requested tag
- exact Git revision
- content digest
- selected dependency components, when narrowed from the package default
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
- Nodus filters each dependency's own exported components before adapter-specific emission
- If multiple adapter roots are already present, Nodus installs all detected adapters
- Agents: discovered skills are copied to `.agents/skills/<skill-id>_<source-id>/`
- Agents: discovered commands are copied to `.agents/commands/<command-id>_<source-id>.md`
- Claude: discovered skills are copied to `.claude/skills/<skill-id>_<source-id>/`
- Claude: discovered agents are copied to `.claude/agents/<agent-id>_<source-id>.md`
- Claude: discovered commands are copied to `.claude/commands/<command-id>_<source-id>.md`
- Claude: discovered rules are copied to `.claude/rules/<rule-id>_<source-id>.md`
- Codex: discovered skills are copied to `.codex/skills/<skill-id>_<source-id>/`
- Cursor: discovered skills are copied to `.cursor/skills/<skill-id>_<source-id>/`
- Cursor: discovered commands are copied to `.cursor/commands/<command-id>_<source-id>.md`
- Cursor: discovered rules are copied to `.cursor/rules/<rule-id>_<source-id>.mdc`
- OpenCode: discovered skills are copied to `.opencode/skills/<skill-id>_<source-id>/`
- OpenCode: discovered agents are copied to `.opencode/agents/<agent-id>_<source-id>.md`
- OpenCode: discovered commands are copied to `.opencode/commands/<command-id>_<source-id>.md`
- OpenCode: discovered rules are copied to `.opencode/rules/<rule-id>_<source-id>.md`

For managed directories and files, `<source-id>` is a short deterministic suffix:

- Git dependencies use the first 6 characters of the locked commit SHA
- Root and local-path packages use the first 6 characters of the package content digest

In `nodus.lock`, managed runtime outputs are tracked by stable logical roots such as `.agents/skills/<skill-id>`, `.agents/commands/<command-id>.md`, `.claude/skills/<skill-id>`, `.codex/skills/<skill-id>`, `.cursor/rules/<rule-id>.mdc`, and `.opencode/commands/<command-id>.md`. During sync and doctor, Nodus expands each logical path back to the concrete suffixed directory or file using the locked package source.

For each selected runtime root, Nodus also writes a managed `.gitignore` file that ignores both itself and the generated runtime outputs inside that root.

## Development

Run the verification suite:

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```
