# Codex and Claude native integration

Status: draft
Date: 2026-05-16

## Summary

Nodus already treats Claude Code and Codex as first-class adapter targets, but
both products have moved deeper into native plugin and hook surfaces. This
spec defines the next integration milestone: move dependency-scoped runtime
behavior into native plugin bundles where the host supports it, refresh stale
hook capability matrices, and close the most visible "managed by Nodus but not
native-feeling" gaps.

The intended implementation should be split across multiple agents. See
`docs/plans/2026-05-16-codex-claude-native-integration.md` for execution
tracks, file ownership, and verification gates.

## Official surfaces checked

Checked on 2026-05-16:

- OpenAI Codex plugin docs:
  https://developers.openai.com/codex/plugins/build
- OpenAI Codex hook docs:
  https://developers.openai.com/codex/hooks
- OpenAI Codex config basics:
  https://developers.openai.com/codex/config-basic
- Claude Code plugin docs:
  https://code.claude.com/docs/en/plugins
- Claude Code plugin reference:
  https://code.claude.com/docs/en/plugins-reference
- Claude Code hook docs:
  https://code.claude.com/docs/en/hooks

These docs are the source of truth for this milestone. If implementation-time
behavior differs from these pages, stop and update this spec before broad code
changes.

## Fundamental facts

- Nodus has six adapters today: `agents`, `claude`, `codex`, `copilot`,
  `cursor`, and `opencode`.
- Claude and Codex currently prefer `PackagePluginWorkspaceMarketplace` in
  `AdapterProfile`, so dependency packages are normally emitted as local native
  plugins rather than direct runtime folders.
- Nodus already emits Claude dependency hooks and activation context inside
  generated Claude plugins.
- Nodus still emits Codex hooks and activation context into shared workspace
  `.codex/hooks.json`.
- Codex now documents plugin-bundled hooks. They are opt-in with
  `[features].plugin_hooks = true`, and can be loaded from the default
  `hooks/hooks.json` path or from a `.codex-plugin/plugin.json` `hooks` entry.
- Codex hook matchers now cover more than `Bash`: current docs mention
  `apply_patch`, `Edit`, `Write`, and MCP tool names. `SessionStart` matchers
  include `startup`, `resume`, and `clear`.
- Codex non-managed hooks, including plugin-bundled hooks, require trust review
  before they run.
- Claude Code plugins support more component types than Nodus currently
  models: skills, legacy commands, agents, hooks, MCP servers, LSP servers,
  monitors, `bin/`, and plugin default `settings.json`.
- Claude Code hooks support more events and handler types than Nodus portable
  `[[hooks]]`: command, http, mcp_tool, prompt, and agent handlers; plus event
  surfaces such as `Setup`, `PermissionRequest`, `PermissionDenied`,
  `PostToolUseFailure`, `PreCompact`, `PostCompact`, `WorktreeCreate`, and
  `WorktreeRemove`.
- Nodus user-level Codex config writes are currently opt-in through
  `NODUS_ENABLE_CODEX_USER_CONFIG` because authored entries are not yet
  tracked and pruned.

## Goals

1. Make Codex dependency hooks as native and removable as Claude dependency
   hooks.
2. Refresh the Codex hook matrix so Nodus exposes the capabilities Codex
   officially supports today.
3. Preserve user safety around Codex hook trust and user-level config writes.
4. Add a clear path for Claude packages that need native Claude-only plugin
   features beyond the portable Nodus subset.
5. Improve the Codex and Claude "doctor/inspect" experience so users can see
   which native marketplace, plugin, hook, and config files Nodus authored.
6. Bring the Nodus MCP tool surface closer to the CLI surface so Codex can
   manage Nodus without shelling out for common workflows.

## Non-goals

- Do not make Claude and Codex plugin systems look identical. Use each host's
  documented native shape.
- Do not bypass Codex hook trust review.
- Do not turn every Claude native hook event into portable Nodus `[[hooks]]`.
  The portable subset should remain conservative.
- Do not remove the existing workspace hook path for root manifests. The root
  manifest describes the workspace itself.
- Do not redesign dependency resolution, lockfile versioning, or package
  discovery outside the integration points listed here.
- Do not silently write to `~/.codex/config.toml` by default until provenance
  and pruning are implemented.

## Current state

### Codex

Nodus emits a local Codex marketplace at
`.nodus/.agents/plugins/marketplace.json`, and dependency plugins under
`.nodus/packages/<alias>/codex-plugin/`.

Plugin metadata includes:

- `.codex-plugin/plugin.json`
- `skills` when a package has skills or synthetic command skills
- `mcpServers` when a package has MCP servers

Codex commands do not have a native Nodus command artifact. They are bridged as
synthetic skills named with the reserved `__cmd_` prefix.

Codex hooks currently land in:

- `.codex/hooks.json`
- `.codex/hooks/nodus-hook-*.sh`

Project `.codex/config.toml` is used for MCP config and for enabling
`[features].hooks` when launch sync is emitted.

### Claude

Nodus emits a local Claude marketplace at
`.nodus/.claude-plugin/marketplace.json`, and dependency plugins under
`.nodus/packages/<alias>/claude-plugin/`.

Generated Claude plugins can include:

- skills
- agents
- rules
- commands
- `.mcp.json`
- `hooks/hooks.json`
- `.claude-plugin/plugin.json`

Root hooks remain in workspace `.claude/settings.json`. Dependency portable
hooks and activation context live in plugin-local hooks.

Claude plugin imports are partial. Nodus parses core fields such as skills,
agents, commands, hooks, and MCP servers, but does not yet model or preserve
every plugin component supported by Claude Code.

## Desired behavior

### 1. Codex dependency plugin hooks

For non-root packages emitted as Codex plugins, Nodus should write portable
package hooks and activation context inside the generated plugin root.

Concrete output:

- `.nodus/packages/<alias>/codex-plugin/hooks/hooks.json`
- `.nodus/packages/<alias>/codex-plugin/hooks/scripts/<stem>.sh`
- `.nodus/packages/<alias>/codex-plugin/.codex-plugin/plugin.json` includes
  `"hooks": "./hooks/hooks.json"` when hook output exists.

The hook JSON should use Codex's documented layout:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup|resume|clear",
        "hooks": [
          {
            "type": "command",
            "command": "sh \"${PLUGIN_ROOT}/hooks/scripts/nodus-hook-example.sh\""
          }
        ]
      }
    ]
  }
}
```

`PLUGIN_ROOT` is preferred for Codex-generated plugin commands. Codex also
sets `CLAUDE_PLUGIN_ROOT` for compatibility, but Nodus should not require that
compatibility alias for new Codex output.

Project `.codex/config.toml` should include:

```toml
[features]
hooks = true
plugin_hooks = true
```

only when Nodus emits Codex hook surfaces that need those features. Existing
project config keys outside Nodus ownership must be preserved when merge mode
is active.

Root package hooks continue to land in `.codex/hooks.json`, because the root
package is the workspace rather than a dependency plugin.

### 2. Codex hook capability matrix

The Codex adapter profile should represent current official support:

- Events: `session_start`, `user_prompt_submit`, `pre_tool_use`,
  `permission_request`, `post_tool_use`, `stop`.
- Session start sources: `startup`, `resume`, `clear`.
- Tool matchers:
  - `bash` -> `Bash`
  - `apply_patch` -> `apply_patch`
  - `edit` -> `Edit`
  - `write` -> `Write`

MCP tool names cannot be represented by the current `HookTool` enum because
they are dynamic names such as `mcp__filesystem__read_file`. Add one of:

- a raw matcher string field under `HookMatcher`, or
- a Codex-specific hook escape hatch for native hook config.

Prefer the smallest extension that does not pollute portable adapter semantics.

### 3. Codex user config provenance

Nodus should make Codex auto-discovery smoother, but not by reviving silent
global writes.

Preferred direction:

- Track authored user-level Codex entries with enough provenance to remove
  stale marketplaces and plugin enables when packages disappear.
- Keep the current env-gated behavior until provenance exists.
- Consider a repo-root `.agents/plugins/marketplace.json` wrapper as a lower
  risk alternative for project-local discovery if Codex reliably loads it from
  trusted repo roots.

Acceptance for this milestone is a design-and-implementation slice, not a
half-on global write:

- Either provenance and pruning are complete, or user-level writes remain
  opt-in.
- Docs clearly describe the remaining user action, if any.

### 4. Claude native passthrough

Nodus should preserve more Claude plugin-native functionality when importing
or wrapping existing Claude plugins.

Minimum additions:

- Support path-based marketplace `mcpServers` entries.
- Preserve/copy plugin component files that Claude Code loads natively:
  `.lsp.json`, `monitors/`, `bin/`, `settings.json`, `output-styles/`, and
  `themes/`.
- Handle directory-backed and inline command forms if they appear in real
  marketplace packages.

Portable `[[hooks]]` should remain command-only. Native Claude hook passthrough
is the correct escape hatch for richer Claude-only hook handlers and events.

### 5. Inspection and diagnostics

Add a user-facing way to inspect native integration state. The output should
answer:

- Which adapters are enabled?
- Which marketplace files did Nodus write?
- Which plugin keys should the host see?
- Which plugin roots exist?
- Where are hooks located?
- Is Codex `plugin_hooks` required and enabled?
- Is user-level Codex config opted in, skipped, or stale?
- Which Claude `enabledPlugins` keys did Nodus manage?

This can start as `nodus info` output or a new doctor section. The important
part is that users can explain why a plugin/hook did or did not load.

### 6. MCP parity for agent-driven management

Codex and Claude can both reach Nodus through the Nodus MCP server. The MCP
tools should expose high-level install/sync options already available in the
CLI:

- `global`
- `dev`
- `revision`
- `sync_on_launch`
- `accept_all_dependencies`
- `dry_run`

The MCP layer must delegate to the same resolver/handler logic as the CLI.
Do not fork behavior.

## Data model changes

Likely additions:

- A Codex plugin hook emission struct, parallel to Claude's plugin hook
  emission result.
- A profile-level representation for `plugin_hooks` requirement, or an
  output-plan boolean calculated from emitted Codex plugin hook files.
- Optional raw hook matcher support for dynamic MCP tool matcher names.
- Claude plugin extra-file tracking for native passthrough components.
- Provenance for Codex user config entries if the user-config track is
  implemented in this milestone.

Any new public manifest fields must be documented in `examples/nodus.toml` and
`docs/hooks.md`.

## Compatibility and migration

- Existing `.codex/hooks.json` files must keep working.
- Existing root hooks must keep their workspace location.
- Existing lockfiles should prune old dependency `.codex/hooks/nodus-hook-*`
  files after dependency hooks move into Codex plugin folders.
- Existing Claude plugin hook behavior must keep working.
- Existing `NODUS_ENABLE_CODEX_USER_CONFIG` behavior should remain compatible
  until replaced by provenance-backed cleanup.

## Acceptance criteria

1. A Codex dependency package with portable hooks emits plugin-local
   `hooks/hooks.json` and scripts under `.nodus/packages/<alias>/codex-plugin/`.
2. That package's `.codex-plugin/plugin.json` includes a `hooks` field when
   hook files exist.
3. `.codex/hooks.json` no longer receives dependency hook entries when the
   dependency can be represented as a Codex plugin. Root hooks still land
   there.
4. `.codex/config.toml` enables `features.hooks` and
   `features.plugin_hooks` when plugin hooks are emitted.
5. Codex `clear`, `apply_patch`, `Edit`, and `Write` matcher support is
   represented in adapter filtering and docs.
6. Claude marketplace path-based `mcpServers` imports are supported.
7. Claude plugin native passthrough files are preserved or explicitly warned
   as unsupported.
8. MCP `nodus_add` reaches CLI option parity for the options listed above.
9. Docs and examples describe the new Codex plugin hook behavior, Codex
   feature flags, Claude native passthrough, and MCP parity.
10. Relevant tests pass:
    - `cargo fmt --check`
    - `cargo test --workspace --all-features`
    - `cargo clippy --workspace --all-targets --all-features -- -D warnings`

## Open questions

- Should Codex plugin hooks use `PLUGIN_ROOT` only, or should generated command
  strings use both `PLUGIN_ROOT` and a fallback to `CLAUDE_PLUGIN_ROOT` for
  compatibility with older Codex builds?
- Should dynamic MCP tool hook matchers be portable manifest data, or a
  Codex-only native hook passthrough?
- Is a repo-root `.agents/plugins/marketplace.json` wrapper sufficient for
  Codex auto-discovery in trusted projects, or is user-level config still
  required for reliable installs?
- How much of Claude's expanded event list should be exposed as portable
  `HookEvent` variants versus adapter-native passthrough?
