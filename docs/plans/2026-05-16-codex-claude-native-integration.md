# Codex and Claude native integration plan

> For agentic workers: this plan is intentionally split into parallel tracks.
> Each worker owns the files listed in its track and must not revert or
> overwrite work from other workers. Commit each completed step before starting
> the next step, using Conventional Commits.

Spec: `docs/specs/2026-05-16-codex-claude-native-integration.md`

## Goal

Implement deeper native Codex and Claude integration for Nodus:

- Codex dependency hooks and activation move into generated Codex plugins.
- Codex hook capability data matches current official docs.
- Claude plugin wrappers preserve more native Claude plugin surfaces.
- The Nodus MCP server exposes the same high-level install options as the CLI.
- Docs and diagnostics make native plugin state understandable.

## Coordination rules

- Start each track from a clean working tree.
- Do not edit files outside the track's ownership unless the coordinator
  explicitly reassigns ownership.
- Prefer targeted tests while iterating, then run the final verification gate.
- If a track discovers that official docs no longer match this plan, stop and
  update the spec first.
- Keep changes behavior-focused. Avoid unrelated refactors.

## Dependency graph

1. Track A can start immediately.
2. Track B can start immediately, but must coordinate if Track A changes the
   Codex hook profile shape.
3. Track C can start immediately.
4. Track D can start immediately.
5. Track E should start after A, B, and C expose final state.
6. Track F should run after implementation tracks are merged.

## Track A: Codex plugin-local hooks

**Worker ownership**

- `src/adapters/codex.rs`
- Codex hook-related sections of `src/adapters/output.rs`
- Codex hook tests in `src/resolver/runtime/tests.rs`
- Codex hook docs in `docs/hooks.md`

**Objective**

Move non-root dependency Codex hooks and activation context from workspace
`.codex/hooks.json` into each generated Codex plugin.

**Steps**

- [x] Add Codex plugin hook emission helpers parallel to Claude's
      `plugin_native_hook_files`.
- [x] Emit plugin-local hook scripts under
      `.nodus/packages/<alias>/codex-plugin/hooks/scripts/`.
- [x] Emit plugin-local `hooks/hooks.json` with the documented Codex hook
      shape.
- [x] Add `"hooks": "./hooks/hooks.json"` to generated
      `.codex-plugin/plugin.json` when hook output exists.
- [x] Ensure `.codex/config.toml` writes `features.hooks = true` and
      `features.plugin_hooks = true` when Codex plugin hooks are emitted.
- [x] Keep root Codex hooks in `.codex/hooks.json`.
- [x] Update pruning/ownership expectations so old dependency workspace hook
      files are removed after resync.
- [x] Add tests for:
      - dependency Codex hooks emitted inside plugin root
      - root Codex hooks still emitted in workspace hooks
      - activation context emitted inside dependency plugin hooks
      - `plugin_hooks` feature enabled only when required

**Acceptance**

- Dependency hooks do not appear in `.codex/hooks.json` when the dependency is
  emitted as a Codex plugin.
- Generated Codex plugin hook commands use plugin-root-relative paths.
- `cargo test -p nodus resolver::runtime` or the closest targeted test scope
  passes while iterating.

**Suggested commit**

`feat(codex): emit dependency hooks inside native plugins`

## Track B: Codex hook profile refresh

**Worker ownership**

- `src/adapters/profile.rs`
- Hook matcher data model files if needed:
  - `src/manifest/types.rs`
  - `src/manifest/load.rs`
  - `src/manifest/tests.rs`
- Profile and hook docs:
  - `docs/hooks.md`
  - `examples/nodus.toml`

**Objective**

Update Nodus's Codex hook capability matrix to match current official docs.

**Steps**

- [x] Add Codex `SessionStart` support for `clear`.
- [x] Add Codex tool matcher support:
      - `apply_patch` -> `apply_patch`
      - `edit` -> `Edit`
      - `write` -> `Write`
- [x] Decide how dynamic MCP tool names are represented:
      - add a raw matcher field, or
      - document that native Codex hook passthrough is required for MCP names.
- [x] Update docs tables and examples.
- [x] Add/adjust tests for supported events, session sources, and tool
      matchers.

**Acceptance**

- Hooks targeting `apply_patch`, `edit`, and `write` are no longer filtered out
  for Codex.
- Docs state that Codex does not intercept every tool path and that hook
  coverage follows official Codex limitations.

**Suggested commit**

`feat(codex): refresh hook matcher support`

## Track C: Claude native plugin passthrough

**Worker ownership**

- `src/manifest/types.rs`
- `src/manifest/discover.rs`
- Claude plugin emission/copying in:
  - `src/adapters/claude.rs`
  - Claude sections of `src/adapters/output.rs`
- Claude import tests in `src/manifest/tests.rs` and
  `src/resolver/runtime/tests.rs`

**Objective**

Preserve more of Claude Code's native plugin surface when Nodus imports or
wraps Claude plugins.

**Steps**

- [x] Support marketplace `mcpServers` path values by reading the referenced
      file relative to the plugin root.
- [x] Track native passthrough components in `ClaudePluginExtras`:
      - `.lsp.json`
      - `monitors/`
      - `bin/`
      - `settings.json`
      - `output-styles/`
      - `themes/`
- [x] Copy those components into generated Claude plugin roots when present.
- [x] Preserve or warn explicitly for unsupported command shapes:
      - directory-backed commands
      - inline command content
- [x] Add tests for path-based MCP import and each passthrough file/dir.

**Acceptance**

- A Claude marketplace plugin using `mcpServers: "./.mcp.json"` imports
  successfully.
- Supported passthrough files survive a Nodus sync into the generated plugin.
- Unsupported shapes produce deterministic warnings rather than silent loss.

**Suggested commit**

`feat(claude): preserve native plugin components`

## Track D: MCP tool parity

**Worker ownership**

- `src/mcp/tools.rs`
- `src/mcp/handlers.rs`
- MCP handler tests
- `docs/superpowers/specs/2026-04-03-mcp-server-design.md` only if it needs
  an update

**Objective**

Let Codex and Claude use MCP to drive the same install flows users can drive
from the CLI.

**Steps**

- [x] Extend `nodus_add` schema with:
      - `global`
      - `dev`
      - `revision`
      - `sync_on_launch`
      - `accept_all_dependencies`
      - `dry_run`
- [x] Thread those values through the MCP handler into existing resolver/CLI
      logic.
- [x] Add validation for mutually exclusive selectors:
      - `tag`
      - `branch`
      - `version`
      - `revision`
- [x] Add MCP tests covering schema and dispatch behavior.
- [x] Update MCP docs if they list tool schemas.

**Acceptance**

- MCP `nodus_add` can express the CLI options listed above.
- The MCP path does not fork dependency resolution behavior from CLI handlers.

**Suggested commit**

`feat(mcp): align add tool with cli options`

## Track E: Native integration diagnostics

**Worker ownership**

- `src/info.rs`
- `src/report.rs`
- `src/review/runtime.rs` if doctor/review output is used
- CLI output tests that cover info/doctor reports
- User-facing docs touched by diagnostics

**Objective**

Make it easy to inspect what Nodus generated for Codex and Claude.

**Steps**

- [x] Decide whether diagnostics live under `nodus info`, `nodus doctor`, or
      both.
- [x] Report enabled adapters and selected surfaces.
- [x] Report marketplace paths:
      - `.agents/plugins/marketplace.json`
      - `.nodus/.claude-plugin/marketplace.json`
- [x] Report generated plugin keys and plugin roots.
- [x] Report hook locations for root and dependency hooks.
- [x] Report Codex feature state:
      - `features.hooks`
      - `features.plugin_hooks`
      - user config sync state
- [x] Report Claude settings state:
      - `extraKnownMarketplaces`
      - `enabledPlugins`
- [x] Add tests with stable text or JSON output.

**Acceptance**

- A user can tell from Nodus output why a Codex or Claude plugin/hook should or
  should not be visible to the host.
- Diagnostics do not require invoking Codex or Claude.

**Suggested commit**

`feat(info): show native plugin integration state`

## Track F: Documentation and examples sweep

**Worker ownership**

- `README.md`
- `README.cn.md` if translated docs are kept in sync for this area
- `docs/hooks.md`
- `examples/nodus.toml`
- `examples/package-author.nodus.toml`
- Newly added spec/plan status updates

**Objective**

Make the new behavior understandable for package consumers and authors.

**Steps**

- [x] Document Codex plugin-local hooks and `plugin_hooks`.
- [x] Document Codex matcher support and limitations.
- [x] Document Claude native passthrough support.
- [x] Document MCP `nodus_add` parity.
- [x] Add package-author examples for Codex and Claude native plugin cases.
- [x] Update this plan's checkboxes and the spec status.

**Acceptance**

- Docs match implemented behavior and official host terminology.
- No stale claims remain about Codex only supporting `Bash` matchers.

**Suggested commit**

`docs(adapters): document native Codex and Claude integration`

## Final integration gate

After all tracks are merged:

- [x] `cargo fmt --check`
- [x] `cargo test --workspace --all-features`
- [x] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [x] `cargo test --doc --workspace`
- [x] Manual generated-output inspection with a fixture package that has:
      - Codex hooks
      - Claude hooks
      - activation context
      - MCP server
      - command-as-skill
- [x] Confirm Codex local marketplace registration is documented.
- [x] Confirm lockfile-owned paths cover all generated plugin hook files.

## Suggested agent launch set

When implementation begins, spawn these workers in parallel:

- `worker-codex-hooks`: Track A.
- `worker-codex-profile`: Track B.
- `worker-claude-native`: Track C.
- `worker-mcp-parity`: Track D.

Hold Track E until Tracks A-C have produced their final output shapes. Hold
Track F until implementation behavior has stabilized.

## Risks

- Codex plugin hooks are still marked under development. The implementation
  must be easy to revise if OpenAI changes the feature flag or trust model.
- Codex hook trust review means generated hooks may exist but not run until
  trusted. Diagnostics and docs must say this clearly.
- Claude's native hook surface is much larger than Nodus portable hooks. Avoid
  overfitting the portable model to Claude-only concepts.
- User-level Codex config cleanup can affect files outside the project root.
  Keep an opt-out until provenance is real.
