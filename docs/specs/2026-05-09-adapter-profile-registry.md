# Adapter Profile Registry

Status: Draft
Date: 2026-05-09

## Summary

Centralize adapter capability metadata in a Rust-native profile registry while
keeping provider-specific file formatting in the existing adapter modules.

The registry should answer questions such as "does Claude support rules?",
"which hook events can Codex emit?", and "where does OpenCode place commands?"
from one source of truth. It should not turn adapter rendering into external
configuration.

## Fundamental Facts

- Nodus supports six adapters today: `agents`, `claude`, `codex`, `copilot`,
  `cursor`, and `opencode`.
- Adapter support is currently represented in several places:
  - `ArtifactKind::supported_adapters`.
  - `runtime_root`.
  - `managed_artifact_path`.
  - Hook event, session source, and tool matcher support helpers.
  - Conditional branches in `build_output_plan`.
  - Documentation tables such as `docs/hooks.md`.
- Adapter rendering is provider-specific. A Claude command file and an OpenCode
  command file may share a source artifact but not the same emitted semantics.
- Nodus should remain compatible with provider-native standards instead of
  forcing providers into one invented target format.

## Goals

- Make adapter capability checks come from one typed registry.
- Keep adapter-specific rendering functions in `agents.rs`, `claude.rs`,
  `codex.rs`, `copilot.rs`, `cursor.rs`, and `opencode.rs`.
- Reduce drift between code support matrices and documentation.
- Make future adapter additions easier to review.
- Give `nodus info` and `doctor` better access to explain why a package
  artifact was emitted, skipped, or transformed.

## Non-Goals

- Do not add Gemini, Windsurf, or other adapters in this spec.
- Do not move formatter code into TOML, JSON, or YAML configuration.
- Do not make unsupported provider behavior appear portable.
- Do not change emitted paths or file contents in the first implementation.
- Do not remove existing adapter modules.

## Proposed Registry

Add `src/adapters/profile.rs`.

Candidate shape:

```rust
pub(crate) struct AdapterProfile {
    pub adapter: Adapter,
    pub root: &'static str,
    pub artifacts: ArtifactSupport,
    pub hooks: HookSupport,
    pub mcp: McpSupport,
}

pub(crate) struct ArtifactSupport {
    pub skill: Option<ArtifactPlacement>,
    pub agent: Option<ArtifactPlacement>,
    pub rule: Option<ArtifactPlacement>,
    pub command: Option<ArtifactPlacement>,
}

pub(crate) struct ArtifactPlacement {
    pub directory: &'static str,
    pub extension: ArtifactExtension,
    pub placement: PlacementKind,
}

pub(crate) enum PlacementKind {
    RuntimeRoot,
    SharedAgentsRoot,
    SyntheticSkill,
}

pub(crate) struct HookSupport {
    pub events: &'static [HookEvent],
    pub session_start_sources: &'static [HookSessionSource],
    pub tool_matchers: &'static [(HookTool, &'static str)],
}

pub(crate) struct McpSupport {
    pub project_mcp_json: bool,
    pub codex_config_toml: bool,
    pub opencode_json: bool,
}
```

The exact names can change. The important rule is that capability metadata is
centralized and statically typed.

## Registry Scope

The profile should own:

- Adapter root directory, such as `.claude` or `.opencode`.
- Whether each artifact kind is supported.
- Default managed placement metadata.
- Hook event support.
- Hook session source support.
- Hook tool matcher spelling.
- MCP output surface support.

The profile should not own:

- Markdown, TOML, or JSON formatter implementations.
- Provider-specific hook wrapper script contents.
- Provider-specific MCP serialization details.
- File merge logic.
- Managed name conflict resolution.

## Initial Profile Data

The initial registry should encode current behavior only.

### Artifact Support

- `skills`: agents, Claude, Codex, Copilot, Cursor, OpenCode.
- `agents`: Claude, Codex, Copilot, OpenCode.
- `rules`: Claude, Cursor, OpenCode.
- `commands`: agents, Claude, Cursor, OpenCode.
- Codex commands remain a special synthetic skill path, because current Nodus
  emits commands as Codex skills.

### Hook Support

Use the current hook support matrix:

- Claude: all events except `permission_request`.
- Codex: `session_start`, `user_prompt_submit`, `pre_tool_use`,
  `permission_request`, `post_tool_use`, `stop`.
- OpenCode: `session_start`, `pre_tool_use`, `post_tool_use`, `stop`.
- Copilot: `session_start`, `user_prompt_submit`, `pre_tool_use`,
  `post_tool_use`, `stop`, `subagent_stop`, `session_end`.
- Agents and Cursor: no portable hooks.

Tool matcher spelling should come from the registry rather than a `match`
spread across helper functions.

### MCP Support

Current MCP output surfaces are:

- Project `.mcp.json`.
- Codex `.codex/config.toml`.
- OpenCode `opencode.json`.

The registry can describe availability, while existing serializer functions
continue to generate provider-specific config.

## Public Internal API

Expose small query functions rather than leaking profile internals everywhere:

```rust
pub(crate) fn adapter_profile(adapter: Adapter) -> &'static AdapterProfile;
pub(crate) fn artifact_supported(adapter: Adapter, kind: ArtifactKind) -> bool;
pub(crate) fn hook_event_supported(adapter: Adapter, event: HookEvent) -> bool;
pub(crate) fn hook_tool_matcher(adapter: Adapter, tool: HookTool) -> Option<&'static str>;
pub(crate) fn runtime_root_name(adapter: Adapter) -> &'static str;
```

Existing helpers can delegate to these functions during migration. This keeps
the first refactor behavior-preserving and lowers the blast radius.

## Migration Plan

### Phase 1: Read-Only Registry

Add profiles and tests that assert the registry matches current behavior.

Keep existing functions, but implement these through the registry:

- `hook_event_supported_by_adapter`.
- `session_start_source_supported_by_adapter`.
- `hook_tool_matcher_for_adapter`.
- `runtime_root`.

`ArtifactKind::supported_adapters` may either delegate to the registry or remain
temporarily duplicated with tests proving parity.

### Phase 2: Artifact Capability Queries

Move artifact support checks out of hard-coded support matrices.

`build_output_plan` should ask:

```rust
if artifact_supported(adapter, ArtifactKind::Skill) { ... }
```

Formatter calls remain explicit. This preserves the current direct relationship
between provider and formatter module.

### Phase 3: Path Placement Metadata

Move simple placement data into the profile where it is truly declarative:

- Runtime root names.
- Default artifact directories.
- Default extensions.
- Shared `.agents` skill placement.
- Codex command-as-skill marker.

Keep complex path construction in Rust functions until the profile is proven
stable.

### Phase 4: Documentation Generation

Once the registry is authoritative, add a small test or doc generation helper
that checks `docs/hooks.md` against hook profile data.

This can be a test-only renderer first. It does not need to rewrite docs
automatically.

## Interaction With Managed Output Provenance

The provenance spec benefits from the registry:

- `adapter` values can be serialized from `AdapterProfile`.
- `kind` values can be validated against profile support.
- Skipped artifacts can include profile-backed reasons.

Do not block provenance implementation on this registry. Provenance can ship
first and migrate to registry queries later.

## Interaction With Package Shape Adapters

Shape adapters should produce package content and provider-native compatibility
surfaces. Adapter profiles decide whether a selected adapter can emit those
surfaces.

Example:

```text
root SKILL.md -> package skill
adapter profile says selected adapter supports skills
adapter module renders provider-specific skill files
```

This keeps shape detection separate from target emission.

## Verification

Add tests for:

- Registry artifact support matches current `ArtifactKind::supported_adapters`.
- Registry hook event support matches current hook behavior.
- Registry session source support matches current hook behavior.
- Registry tool matcher spelling matches current emitted adapter spellings.
- `runtime_root` returns the same paths as before.
- `build_output_plan` output is byte-for-byte unchanged for representative
  packages across all existing adapters.
- Documentation hook matrix is covered by either a parity test or a generated
  fixture.

## Open Questions

- Should `ArtifactKind::supported_adapters` remain as a convenience wrapper or
  be removed after migration? Keeping it as a wrapper may preserve readability.
- Should MCP support be modeled per adapter or as project-level outputs gated by
  selected adapters? Current behavior has both project `.mcp.json` and
  adapter-specific outputs, so the profile may need both concepts.
- Should profile data include user-level/global install paths? That should wait
  until project-scope behavior is fully migrated.
