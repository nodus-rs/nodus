# Claude plugin hooks for portable `[[hooks]]`

Status: in progress (2026-05-15)

## Problem

When a Nodus dependency declares portable `[[hooks]]` in `nodus.toml`,
Nodus today emits two things on `nodus sync`:

1. A shell wrapper at `.claude/hooks/nodus-hook-<alias>-<event>-<digest>.sh`.
2. A managed `hooks.<Event>` entry inside the workspace
   `.claude/settings.json` pointing at that wrapper.

That works, but every dependency hook landing in `.claude/settings.json`
means:

- The workspace settings file accumulates per-package noise that has to
  be diff'd and pruned on every package change.
- Hook scripts live under `.claude/hooks/`, which sits next to direct
  workspace files (skills, commands, agents). Removing the package
  leaves orphaned scripts behind unless the lockfile diff catches them.
- The user's `.claude/settings.json` carries dependency-specific
  matchers, IDs, and digests instead of a stable "enable this plugin"
  record.

Claude Code plugins already define their own native hook surface: Claude
loads the standard `hooks/hooks.json` inside an enabled plugin, and commands
inside that file use `${CLAUDE_PLUGIN_ROOT}` to reference plugin-local
scripts. The plugin manifest's `hooks` field is reserved for additional
non-standard hook files. Nodus already emits each non-root Nodus dependency as
a Claude plugin under `.nodus/packages/<alias>/claude-plugin/`, but does not
yet route the portable `[[hooks]]` through it.

## Goal

For dependency packages that Nodus emits as Claude plugins, generate
their portable `[[hooks]]` _inside_ the plugin folder, not the workspace
settings file. Concretely:

- Hook wrappers live at
  `.nodus/packages/<alias>/claude-plugin/hooks/scripts/<id>.sh`.
- A `.nodus/packages/<alias>/claude-plugin/hooks/hooks.json` file
  contains the per-event entries that today land in
  `.claude/settings.json`.
- The plugin's `.claude-plugin/plugin.json` does not repeat the standard
  `hooks/hooks.json` path; Claude loads it automatically.
- The workspace `.claude/settings.json` only carries the root manifest's
  own `[[hooks]]` (e.g. `nodus.sync_on_startup`) plus marketplace and
  `enabledPlugins` wiring. Removing a dependency removes its plugin
  folder, which removes its hook surface.

Activation context (`[activation].always_context` /
`prefer_skills`) follows the same model for dependency packages: each
plugin owns its own activation SessionStart hook inside
`hooks/hooks.json`, with its `additionalContext` payload generated under
`hooks/scripts/`. Root-package activation, if any, stays in workspace
settings since the root is the workspace itself.

## Non-goals

- Root package hooks. The root manifest is the workspace; its hooks
  continue to be emitted into `.claude/settings.json` as today.
- Codex, OpenCode, Copilot, Cursor, Agents adapters. This change only
  reshapes Claude output.
- The `claude_plugin_hooks` compat escape hatch. That remains
  unchanged; it carries pre-built `hooks/hooks.json` from packages that
  already author plugin-shaped hooks directly.

## Acceptance criteria

1. A dependency package declaring portable `[[hooks]]` (targeting
   Claude either implicitly or via `adapters = ["claude"]`) emits its
   hook scripts under `.nodus/packages/<alias>/claude-plugin/hooks/`
   and its `hooks.json` under that same directory.
2. The plugin's `plugin.json` does not carry a `hooks` entry for the
   standard `hooks/hooks.json` file.
3. `.claude/settings.json` no longer contains entries for that
   dependency's hooks. It still carries the marketplace,
   `enabledPlugins`, and the root manifest's own hook entries.
4. A dependency that has portable hooks but no skills/agents/commands/
   rules/MCP still produces a Claude plugin shell so its hooks reach
   Claude.
5. Removing or disabling the dependency removes the plugin folder and,
   therefore, the hooks.
6. Hook wrappers use `${CLAUDE_PLUGIN_ROOT}` for self-reference and
   `${CLAUDE_PROJECT_DIR}` (falling back to `git rev-parse
   --show-toplevel`) for `cwd = "git_root"`, matching today's
   behavior. `NODUS_HOOK_ID`, `NODUS_HOOK_EVENT`, and
   `NODUS_HOOK_TIMEOUT_SEC` env vars stay exported.
7. Dependency activation context is emitted as a SessionStart entry
   inside the plugin's `hooks.json`, not the workspace settings file.
8. Existing managed-output provenance still tracks the plugin root, so
   prune-on-resync covers the new hook files automatically.

## Format

`hooks/hooks.json` mirrors Claude's native hook layout:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup|resume",
        "hooks": [
          {
            "type": "command",
            "command": "sh \"${CLAUDE_PLUGIN_ROOT}/hooks/scripts/<id>.sh\""
          }
        ]
      }
    ]
  }
}
```

The script filename is derived from a deterministic digest of the
package alias and the hook id, mirroring the current
`managed_script_stem` logic. Scripts continue to be POSIX-shell
wrappers that set Nodus env vars and exec the user command.
