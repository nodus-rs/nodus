# Virtual Plugin Marketplaces

Some adapters have a native marketplace protocol. Claude can load a local
marketplace manifest that points at generated package plugin roots under
`.nodus/packages/<alias>/`.

Other adapters expose a plugin loader but no marketplace. For those adapters,
Nodus uses a virtual marketplace:

1. Resolve and lock the package through normal Nodus dependency state.
2. Copy the selected package files into a managed install root under
   `.nodus/packages/<alias>/<adapter>-plugin/`.
3. Emit the adapter's runtime loader files that import or reference those
   copied package entrypoints.
4. Record ownership for both the install root and loader files so updates,
   disables, and removals prune stale output.

Virtual marketplaces do not fetch remote plugins, install npm packages, or add
another package manager. The package source remains the `nodus.toml`
dependency graph.

## Codex v1

Codex has a plugin marketplace file format, but current Codex runtime
activation is not project-local: effective plugin enablement is read from the
user config/cache layer. Nodus therefore treats Codex as a virtual marketplace
backend so one project cannot leak plugin registrations into another.

When the Codex adapter is selected, full packages with Codex-supported runtime
content are copied to:

```text
.nodus/packages/<alias>/codex-plugin/
```

Codex still reads skills, agents, synthetic command skills, hooks, MCP config,
and feature flags from direct project files under `.codex/`. Nodus does not
emit `.agents/plugins/marketplace.json` for project syncs and does not add
`[marketplaces]` or `[plugins]` entries to `.codex/config.toml`.

## OpenCode v1

When the OpenCode adapter is selected, every full package with OpenCode-supported
runtime content is copied to:

```text
.nodus/packages/<alias>/opencode-plugin/
```

That install root mirrors the package lifecycle Nodus uses for virtual package
payloads. OpenCode still reads skills, agents, commands, rules, MCP
configuration, and portable hooks from the direct `.opencode/` project files.

The lockfile records those deterministic direct OpenCode runtime artifacts with
a compact `owned_runtime_adapters = ["opencode"]` package claim. Nodus expands
that claim in memory from the package's locked skills, agents, commands, and
rules when checking collisions, pruning stale outputs, and recomputing
`install_digest`, so the lock does not repeat every `.opencode/skills/...`
path.

Package authors can additionally declare explicit JavaScript plugin entrypoints
in `nodus.toml`:

```toml
opencode_plugin_hooks = ["hooks/nodus-plugin.ts"]
```

For each entrypoint, Nodus emits a loader wrapper:

```text
.opencode/plugins/nodus-<alias>-<name>-<hash>.js
```

The wrapper imports the copied entrypoint, preserves named exports, and provides
a default export for OpenCode by selecting a default export, common named plugin
export, or first exported plugin-like value.

## Adapter Contract

A future virtual marketplace backend only needs to define:

- adapter name and runtime root
- how package manifests declare or discover plugin entrypoints
- install root pattern under `.nodus/packages/<alias>/`
- loader or config emission strategy for the adapter runtime
- ownership rules for install roots and loader files
- tests for install, update, adapter filtering, component narrowing, disable,
  and removal pruning

Gemini can use this contract later, but this slice intentionally does not add
Gemini support.
