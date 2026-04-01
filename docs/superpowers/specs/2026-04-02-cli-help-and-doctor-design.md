# Guided CLI Help And Repair-First Doctor

## Summary

Nodus should optimize its CLI for users who do not reliably remember command-line workflows. Help output should teach the safe default path in plain language, and `nodus doctor` should become the first recovery command instead of a passive validator.

This design covers two linked deliverables:

1. A guided-first rewrite of `--help` output across the CLI.
2. A repair-first redesign of `nodus doctor`.

## Goals

- Make `nodus --help` understandable to a first-time user in under 30 seconds.
- Make every core command help page show one safe copy-paste command.
- Make every core command help page tell the user what to run next.
- Make `nodus doctor` automatically fix safe issues by default.
- Keep destructive cleanup explicit in normal mode.
- Preserve a read-only mode suitable for CI.

## Non-Goals

- Replacing the CLI with a fully interactive wizard.
- Hiding all advanced flags from experienced users.
- Letting `doctor` silently change requested dependency state.
- Refactoring unrelated command behavior outside help text and doctor recovery.

## User Model

The default user for Nodus help is:

- not fluent in command-line tools
- unlikely to remember the correct sequence of commands
- unsure what terms like `adapter`, `lockfile`, or `managed outputs` mean
- willing to follow copy-paste examples when they are clearly labeled as safe defaults

This means help output should optimize for guided action first and full reference second.

## Design Principles

- Lead with the safest common path.
- Prefer plain English before internal terms.
- Show examples before dense flag lists.
- Tell the user what the command changes.
- Tell the user what to run next.
- Put advanced detail below the beginner path, not mixed into it.
- Keep one consistent help-page structure across the CLI.

## CLI Help Information Architecture

### Root Help

`nodus --help` should present information in this order:

1. One plain-English sentence describing what Nodus does.
2. A `Most common tasks` section with 4-6 copy-paste commands.
3. A `Typical workflows` section showing command sequences such as:
   - first install: `add -> doctor`
   - rebuild current setup: `sync -> doctor`
   - upgrade packages: `update -> doctor`
   - remove package: `remove -> doctor`
4. A concise command list where each command answers `when do I use this?`
5. A note pointing users to `nodus <command> --help` for task-specific details.

### Command Help Template

Each core command help page should follow the same structure:

1. What this command is for.
2. Safest common use.
3. What it changes.
4. Copy-paste examples, ordered from beginner to advanced.
5. What to run next.
6. Grouped options.
7. Common mistakes or warnings, when relevant.

### Option Grouping

Flags should be explained by intent, not just listed as a flat alphabetized set. For example, `nodus add --help` should group options like:

- source selection: `--tag`, `--branch`, `--revision`, `--version`
- install target: `--adapter`, `--global`
- package selection: `--component`, `--accept-all-dependencies`
- execution mode: `--dry-run`, `--sync-on-launch`

If clap output cannot visually group these in the default option list, the grouped explanation should appear in the long help text above the raw flag list.

## Help Content Standards

- Prefer `This writes managed files into your repo` over `This mutates runtime roots`.
- Explain terms the first time they appear.
- Use direct, concrete examples with realistic package names.
- Avoid long paragraphs when short labeled sections work better.
- Put the beginner-safe example first on every command page.
- Do not assume users know they should run `doctor` after state-changing commands.

## Core Command Help Requirements

### `nodus add --help`

Must explain:

- what adding a package does
- that project-scoped install is the default
- the safest default command
- what files may be created or updated
- what to run next: `nodus doctor`

### `nodus sync --help`

Must explain:

- when to use sync instead of update
- that sync rebuilds from existing declared and locked state
- when `--locked` and `--frozen` matter
- what to run next: `nodus doctor`

### `nodus update --help`

Must explain:

- that update resolves newer allowed versions
- that it may rewrite `nodus.lock` and managed outputs
- how it differs from sync
- what to run next: `nodus doctor`

### `nodus remove --help`

Must explain:

- that remove updates configuration and prunes outputs it owned
- the safest default command
- what to run next: `nodus doctor`

### `nodus doctor --help`

Must explain:

- that `doctor` is the first recovery command when Nodus feels broken
- that default behavior checks and auto-fixes safe issues
- that `--check` is read-only
- that `--force` skips prompts for risky cleanup

## Repair-First `doctor`

### Mental Model

The user-facing mental model should be:

`nodus doctor` checks what is wrong, fixes safe problems automatically, and asks before doing risky cleanup.

`doctor` repairs realized state. It should bring the repository back into agreement with `nodus.toml`, `nodus.lock`, shared store state, and generated managed outputs. It should not silently change dependency intent.

### Modes

### Default Mode

`nodus doctor`

- inspects the current state
- auto-fixes safe issues
- prompts before risky fixes
- reruns verification after repairs
- ends with a clear final result: fixed, partially fixed, or blocked

### Check Mode

`nodus doctor --check`

- performs the same inspection
- makes no changes
- exits non-zero when issues remain
- is stable for CI and cautious users

### Force Mode

`nodus doctor --force`

- performs inspection
- applies the complete repair plan without interactive prompts
- may delete conflicting Nodus-managed files or whole managed subtrees when that is the only supported repair

### Repair Classification

Each doctor finding should be classified into exactly one bucket:

- informational
- safe auto-fix
- risky fix requiring permission
- not repairable by doctor

### Safe Auto-Fix Examples

- regenerate missing managed outputs
- rewrite stale generated files that Nodus owns
- restore missing startup hooks or marketplace metadata
- rebuild adapter outputs when declared state and generated state disagree

### Risky Fix Examples

- removing conflicting files in a Nodus-managed directory
- deleting and reinstalling an adapter-specific managed subtree
- removing files that appear generated but no longer match current ownership metadata

### Not Repairable By Doctor Examples

- dependency selection changes that require `nodus add`, `nodus remove`, or manifest edits
- version upgrades that require `nodus update`
- unresolved state that cannot be reconstructed from current manifest and lockfile

### Repair Planner Architecture

`doctor` should be implemented as a repair planner with these phases:

1. Inspect: gather mismatches across manifest, lockfile, store, and managed outputs.
2. Classify: assign each finding to one repair bucket.
3. Plan: build explicit repair actions.
4. Execute: apply actions according to mode and user approval.
5. Verify: rerun validation and report the final state.

This architecture should be shared by both interactive UX and structured machine-readable output.

### Repair Actions

Representative repair actions:

- regenerate one managed file
- regenerate one managed adapter root
- restore startup integration
- prune one conflicting managed file
- wipe and reinstall one managed adapter subtree

Each risky action should produce a plain-English preview before execution in default mode.

Example prompt:

`Codex managed files conflict with what Nodus needs to write. Remove .codex/marketplace/ and reinstall the managed Codex config?`

### Output Behavior

Doctor output should use a consistent structure:

1. What was checked.
2. What problems were found.
3. What was fixed automatically.
4. What needs permission.
5. What remains blocked and which command to run next.

Final output should avoid vague suggestions such as `try syncing again` when a precise next step is known.

### CLI Surface Changes

Recommended command surface:

- `nodus doctor`
- `nodus doctor --check`
- `nodus doctor --force`

If the current CLI already uses JSON output or other existing flags, those should remain compatible. New doctor behavior should fit the existing surface where practical rather than introducing extra command names.

## Rollout Plan

### Phase 1: Guided Help For Core Commands

Rewrite help text for:

- `nodus`
- `add`
- `doctor`
- `sync`
- `update`

These commands define the main user workflow and should be upgraded first.

### Phase 2: Guided Help For Secondary Commands

Apply the same structure to:

- `remove`
- `info`
- `list`
- `outdated`
- `clean`

### Phase 3: Doctor Planner And Safe Repairs

- refactor doctor internals around inspect/classify/plan/execute/verify
- implement safe auto-repairs
- add risky cleanup prompts
- add `--check` and `--force` semantics if not already present

### Phase 4: Messaging Alignment

- align runtime command output with the same plain-language tone as `--help`
- ensure errors and prompts use exact next-step guidance

## Testing And Verification

Add snapshot or golden tests for:

- root help output
- core command help output
- doctor output in normal, check, and force modes

Add behavioral tests for:

- safe doctor auto-fixes
- risky doctor prompts
- forced cleanup paths
- read-only check mode
- post-repair verification state

## Success Criteria

- A first-time user can read `nodus --help` and identify the default workflow quickly.
- Every core command help page has one clear safe example near the top.
- Every state-changing command help page tells the user to run `nodus doctor` next.
- `nodus doctor` resolves common drift without requiring deep knowledge of Nodus internals.
- Destructive cleanup never happens silently in default mode.
- `nodus doctor --check` works reliably for CI.
- Help and doctor UX are protected by tests so regressions are visible.

## Open Implementation Notes

- Keep advanced flags available for experienced users, but move them below the beginner path in help text.
- Preserve compatibility with existing structured output where possible.
- Prefer changes that improve both human help text and machine-generated command guidance at the same time.
