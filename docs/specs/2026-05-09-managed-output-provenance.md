# Managed Output Provenance

Status: Draft
Date: 2026-05-09

## Summary

Add optional per-output provenance records to `nodus.lock` so Nodus can explain
which package, source artifact, adapter, and emitted bytes produced each managed
runtime file.

The first milestone must not change the core ownership rule: `managed_files`
continues to be the pruning and ownership index. New `managed_outputs` records
are diagnostic evidence used by `doctor`, `info`, and future drift reporting.

## Fundamental Facts

- Nodus owns only files it generated or explicitly exported.
- `nodus.lock` currently records packages plus a flat `managed_files` list.
- `LockedPackage` records package-level digest and artifact ids, but not the
  exact emitted file path, source artifact, adapter, or emitted file digest.
- `OutputPlan` currently carries generated files, managed path strings, and
  warnings. It does not preserve why a file exists after adapter rendering.
- Some generated files are one-to-one outputs, such as a Claude skill file.
- Some generated files are merged outputs, such as `.mcp.json`,
  `.codex/config.toml`, OpenCode MCP config, and hook settings.
- Provider-native compatibility matters. Provenance must describe what Nodus
  emitted without pretending all providers share one semantic model.

## Goals

- Explain every generated file with enough data for humans and tools to answer:
  - Which package contributed this file?
  - Which adapter consumed it?
  - Which source artifact or manifest field caused it?
  - Has the emitted file drifted since Nodus wrote it?
- Improve `nodus doctor` findings for missing, stale, and edited managed files.
- Preserve backward compatibility with lockfiles that only contain
  `managed_files`.
- Keep the lockfile deterministic and reviewable.

## Non-Goals

- Do not remove `managed_files` in this milestone.
- Do not use provenance as the sole deletion authority.
- Do not invent a provider-neutral artifact vocabulary for provider-specific
  files.
- Do not add content security scanning in this spec.
- Do not change package resolution, adapter selection, or output formatting.

## Proposed Lockfile Shape

Add an optional top-level `managed_outputs` array.

```toml
[[managed_outputs]]
path = ".claude/skills/review/SKILL.md"
adapter = "claude"
kind = "skill"
digest = "blake3:1f4c..."

[[managed_outputs.origins]]
package = "review_pack"
artifact = "review"
source = "skills/review/SKILL.md"

[[managed_outputs]]
path = ".mcp.json"
adapter = "project"
kind = "mcp_config"
digest = "blake3:a21b..."

[[managed_outputs.origins]]
package = "firebase_tools"
artifact = "firebase"
source = "mcp_servers.firebase"
```

### Fields

- `path`: Required project-relative file path. Must be a file path, not a
  managed directory root.
- `adapter`: Required adapter id, or `project` for adapter-independent project
  files such as `.mcp.json`.
- `kind`: Required output kind. Initial values:
  - `skill`
  - `agent`
  - `rule`
  - `command`
  - `hook`
  - `mcp_config`
  - `managed_export`
  - `runtime_gitignore`
  - `workspace_marketplace`
- `digest`: Required digest of the emitted file bytes. Use the existing BLAKE3
  dependency and encode as `blake3:<hex>`.
- `origins`: Required non-empty list for package-driven outputs. May be empty
  only for purely local runtime bookkeeping files if no package caused them.

Origin fields:

- `package`: Required package alias, usually `root` or a dependency alias.
- `artifact`: Optional provider-neutral id where one exists, such as a skill id,
  agent id, command id, rule id, hook id, MCP server id, or managed export target.
- `source`: Optional package-relative source path or manifest address, such as
  `skills/review/SKILL.md` or `mcp_servers.firebase`.

## Compatibility

This is an additive lockfile field. For the first milestone:

- Keep the existing lockfile version unless implementation discovers a parser
  ambiguity that requires a version bump.
- `managed_outputs` defaults to an empty list when absent.
- `managed_files` remains required for current lockfiles and remains the
  authoritative set for owned path expansion.
- `doctor` should treat a current lockfile without `managed_outputs` as
  repairable stale metadata, not as corruption.

A later milestone may make `managed_outputs` authoritative and remove
directory-style ownership roots from `managed_files`. That later change should
bump the lockfile version.

## Output Planning Model

Extend the output planning layer with provenance metadata before writing the
lockfile.

Candidate internal types:

```rust
pub(crate) struct PlannedManagedOutput {
    pub path: PathBuf,
    pub adapter: ManagedOutputAdapter,
    pub kind: ManagedOutputKind,
    pub origins: Vec<ManagedOutputOrigin>,
}

pub(crate) struct ManagedOutputOrigin {
    pub package_alias: String,
    pub artifact: Option<String>,
    pub source: Option<PathBufOrManifestAddress>,
}
```

The file bytes remain in `ManagedFile`. The digest should be computed after
final merged bytes are known, not before adapter-specific rendering.

Merged outputs should carry multiple origins when multiple packages contribute
to one file. A single merged file still has one emitted digest.

## Doctor Behavior

When `managed_outputs` is available, `doctor` should classify these cases:

- Missing locked output path:
  - Finding: safe auto-fix.
  - Message should name the path and, when available, the package and adapter.
- Disk bytes differ from the locked output digest:
  - Finding: safe auto-fix if the path is still owned and the planned output can
    be regenerated.
  - Finding: risky or manual if the path is no longer owned by current desired
    state.
- Expected output digest differs from the locked output digest:
  - Finding: safe auto-fix for stale lockfile or stale generated output.
- Output path exists but is not in `managed_files`:
  - Existing unmanaged collision rules still apply.

The current generic message, `managed outputs drifted from the declared project
state`, may remain as a summary, but detailed findings should prefer
provenance-backed messages.

## Implementation Plan

1. Add lockfile data types for `managed_outputs`, `ManagedOutputOrigin`,
   `ManagedOutputKind`, and adapter id serialization.
2. Add optional `managed_outputs` to `Lockfile` with default empty
   deserialization.
3. Extend `OutputPlan` to carry planned output metadata alongside `files`.
4. Populate metadata for skills, agents, rules, commands, managed exports, MCP
   config files, hook files, runtime `.gitignore` files, and workspace
   marketplace files.
5. Compute output byte digests when converting a resolution to a lockfile.
6. Teach `doctor` to use `managed_outputs` when present and fall back to
   `managed_files` when absent.
7. Update `nodus info --json` only after the lockfile path is stable. This is
   optional for the first implementation PR.

## Verification

Add tests for:

- Old lockfile without `managed_outputs` parses and can be repaired.
- Generated lockfile contains deterministic `managed_outputs` sorted by path,
  adapter, kind, then origin.
- Editing a managed file produces a provenance-specific `doctor` finding.
- Merged MCP config records all contributing package origins.
- A provider-native hook output records adapter-specific kind without losing
  native source identity.
- `managed_files` pruning behavior is unchanged.

## Open Questions

- Should `source` be a single string instead of a path-or-address enum in the
  serialized format? A string is simpler and likely sufficient.
- Should `digest` use the emitted bytes only, or include normalized path and
  provenance metadata? Emitted bytes are better for drift checks; metadata can
  remain separately reviewable.
- Should project-root `publish_root` outputs use package alias `root` or the
  root package effective name? `root` is more stable.
