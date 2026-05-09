# Package Shape Adapters

Status: Draft
Date: 2026-05-09

## Summary

Introduce a small package-shape import layer that recognizes provider-native
package layouts and converts them into the existing `LoadedManifest` model.

This is compatibility work, not a new package standard. Nodus should accept
more native package shapes while continuing to use one resolver, one lockfile,
and one adapter output pipeline.

## Fundamental Facts

- Nodus' native package shape is `nodus.toml` plus content roots such as
  `skills/`, `agents/`, `rules/`, and `commands/`.
- Nodus already discovers `skills/<id>/SKILL.md` packages.
- Nodus already imports some provider-native metadata:
  - Claude marketplace wrappers from `.claude-plugin/marketplace.json`.
  - Codex marketplace wrappers from `.agents/plugins/marketplace.json`.
  - Claude plugin metadata from `.claude-plugin/plugin.json`.
  - Codex plugin metadata from `.codex-plugin/plugin.json`.
  - Claude default hook bundles at `hooks/hooks.json`.
  - OpenCode plugin hooks declared explicitly in `nodus.toml`.
- This compatibility logic currently lives inside manifest discovery and load
  helpers rather than behind an explicit shape boundary.
- Provider-native layouts may contain semantics that cannot be translated
  losslessly.

## Goals

- Make package shape detection explicit, inspectable, and testable.
- Accept more provider-native package layouts without requiring package authors
  to write `nodus.toml` first.
- Preserve the existing resolver and output pipeline by producing
  `LoadedManifest` data.
- Keep lossy provider-native behavior behind explicit compatibility surfaces
  such as Claude plugin extras, `claude_plugin_hooks`,
  `opencode_plugin_hooks`, and `managed_exports`.
- Improve `nodus info` warnings so users can see which package shape Nodus
  detected and what was imported or skipped.

## Non-Goals

- Do not adopt APM's package taxonomy.
- Do not add a second install path for provider-native packages.
- Do not translate provider-native semantics into fake portable semantics when
  an escape hatch is more accurate.
- Do not change dependency resolution or adapter output formatting.
- Do not infer MCP servers or hooks from arbitrary code.

## Package Shape Model

Add an internal shape module, for example `src/manifest/shapes.rs`.

Candidate types:

```rust
pub(crate) enum PackageShape {
    NodusManifest,
    NodusStandardContent,
    RootSkill,
    ClaudePlugin,
    ClaudeMarketplaceWrapper,
    CodexPlugin,
    CodexMarketplaceWrapper,
    ClaudeHookBundle,
    OpenCodePluginBundle,
}

pub(crate) struct ShapeEvidence {
    pub shape: PackageShape,
    pub marker: PathBuf,
}

pub(crate) struct ShapeImport {
    pub manifest_overlay: Manifest,
    pub claude_plugin: Option<ClaudePluginExtras>,
    pub extra_package_files: Vec<PathBuf>,
    pub warnings: Vec<String>,
    pub allows_empty_dependency_wrapper: bool,
    pub allows_unpinned_git_dependencies: bool,
}
```

The exact data types can differ, but the boundary should be clear:

```text
directory -> detect package shape -> import shape into LoadedManifest -> validate -> resolve
```

No shape adapter should write project files directly.

## Detection Rules

Detection must be deterministic and conservative.

1. If `nodus.toml` exists, load it first.
2. Discover native Nodus content roots.
3. Detect provider-native markers.
4. Apply shape imports in a stable order.
5. Validate the final `LoadedManifest`.

Suggested marker order:

1. `nodus.toml`
2. Native content directories: `skills/`, `agents/`, `rules/`, `commands/`
3. `.claude-plugin/marketplace.json`
4. `.agents/plugins/marketplace.json`
5. `.claude-plugin/plugin.json`
6. `.codex-plugin/plugin.json`
7. `hooks/hooks.json`
8. Root `SKILL.md`
9. OpenCode plugin bundle markers, if supported by the implementation

Explicit Nodus package data should win over fallback shape inference. Existing
behavior where standard layout takes precedence over marketplace fallback should
remain.

## Initial Shape Adapters

### Standard Skill Bundle

Current behavior already recognizes:

```text
skills/<id>/SKILL.md
```

The first implementation should preserve this behavior and report it as
`NodusStandardContent` evidence.

### Root Skill

Accept a package whose root contains:

```text
SKILL.md
```

Rules:

- Only infer root `SKILL.md` when there is no `nodus.toml` and no native Nodus
  content directory.
- Use `SKILL.md` frontmatter `name` as the display name when present.
- Use the package directory name, normalized through the existing artifact id
  rules, as the skill id.
- Treat the root directory as the skill root so assets next to `SKILL.md` stay
  package-owned.
- Reject root `SKILL.md` packages whose frontmatter fails the same validation
  used for normal skills.

This lets Nodus consume simple provider-native skill packages while avoiding
surprising behavior in existing Nodus packages.

### Claude Plugin

Keep importing `.claude-plugin/plugin.json` through Claude plugin extras.

Rules:

- Preserve native Claude plugin hook semantics through Claude plugin hook
  compatibility.
- Import supported skills, agents, commands, and MCP declarations through
  existing Claude plugin metadata logic.
- Warn on unsupported inline content or path indirection instead of guessing.

### Claude Hook Bundle

Keep accepting `hooks/hooks.json` as a Claude-specific compatibility surface.

Rules:

- Treat it as provider-native, not portable `[[hooks]]`.
- Include it in package files and lockfile digest.
- Emit it only for the Claude adapter.

### Codex Plugin

Keep importing `.codex-plugin/plugin.json` for metadata Nodus already
understands.

Rules:

- Import supported MCP declarations.
- Preserve Codex-specific plugin metadata as shape evidence where possible.
- Do not imply all Codex plugin semantics are portable.

### OpenCode Plugin Bundle

OpenCode plugin import should remain explicit until the supported marker is
well-defined.

The implementation may start with one of these conservative choices:

- Only support `opencode_plugin_hooks` in `nodus.toml`, preserving current
  behavior.
- Or detect a narrow conventional marker such as `.opencode/plugins/*.js` and
  import it as `opencode_plugin_hooks` only when there is exactly one clear
  plugin root.

If ambiguity exists, Nodus should require an explicit `nodus.toml`.

## Manifest Load Flow

Refactor `load_from_dir` toward this conceptual flow:

```text
load raw nodus.toml if present
collect shape evidence
apply explicit manifest values
apply compatible shape imports
rediscover package contents using imported plugin extras
load provider-native version metadata if manifest version is absent
validate loaded manifest
```

Shape adapters should reuse existing helpers where possible. The first refactor
should move logic without changing behavior, then add root `SKILL.md`.

## User-Facing Reporting

`nodus info` should eventually show shape evidence:

```text
package-shape:
  - nodus-standard-content: skills/
  - claude-plugin: .claude-plugin/plugin.json
```

JSON output should expose the same data as a stable list of objects:

```json
[
  { "shape": "root_skill", "marker": "SKILL.md" }
]
```

Warnings should explain skipped provider-native content. Example:

```text
warning: .opencode/plugins contains multiple plugin files; add nodus.toml
with opencode_plugin_hooks to select explicit entry points
```

## Implementation Plan

1. Add shape evidence types and detection helpers.
2. Move existing marketplace wrapper and plugin metadata trigger decisions
   behind shape detection without changing behavior.
3. Preserve all existing manifest tests.
4. Add root `SKILL.md` import as the first new shape adapter.
5. Add shape evidence to `LoadedManifest` warnings or a dedicated internal
   field.
6. Surface shape evidence in `nodus info --json` after the internal model is
   stable.
7. Consider narrow OpenCode plugin bundle detection only after the root skill
   and refactor tests are stable.

## Verification

Add tests for:

- Existing standard `skills/<id>/SKILL.md` packages behave unchanged.
- Existing Claude marketplace wrapper behavior is unchanged.
- Existing Codex marketplace wrapper behavior is unchanged.
- Existing Claude plugin metadata import behavior is unchanged.
- Existing default `hooks/hooks.json` Claude hook compatibility is unchanged.
- Root `SKILL.md` without `nodus.toml` installs as one skill.
- Root `SKILL.md` does not override an explicit `nodus.toml` package.
- Root `SKILL.md` does not create an extra skill when `skills/` exists.
- Ambiguous OpenCode plugin markers produce a warning or hard error, not a
  guessed install.

## Open Questions

- Should root `SKILL.md` be assigned component `skills` only, or should assets
  next to it be represented as package managed exports? Treating the whole root
  as the skill root is simpler and closer to provider-native skill packages.
- Should shape evidence become part of `nodus.lock` provenance? The managed
  output provenance spec can record source files, but package-shape evidence may
  belong in `nodus info` rather than the lockfile.
- Should Nodus support single-file virtual packages later? That is separate
  from package-shape detection and should not be folded into this first pass.
