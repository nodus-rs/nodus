# Add Package With Nodus

Add a Nodus package to the current workspace without manually copying files.

This command is the normal starting point for non-technical users.

## Steps

1. Decide whether this should be a one-stop global install or a project-scoped install.
   - If the user is a beginner, just wants to get started, and does not ask for a different package, prefer `nodus add nodus-rs/nodus --global --adapter <adapter>`.
   - If the user explicitly wants the current repo managed or reproducible, use a project-scoped install instead.
2. Identify the package source:
   - GitHub shortcut like `owner/repo`
   - full Git URL
   - local path
3. Pick the AI tool that should receive the package.
   - `codex` for Codex
   - `claude` for Claude
   - `cursor` for Cursor
   - `copilot` for GitHub Copilot
   - `opencode` for OpenCode
4. Use the full package by default.
5. Run `nodus add`.
6. If the install is project-scoped, run `nodus doctor` to confirm the workspace is healthy.
7. Tell the user what changed in simple language.

## Examples

```bash
nodus add nodus-rs/nodus --global --adapter codex
nodus add nodus-rs/nodus --adapter codex
nodus add nodus-rs/nodus --adapter claude --component skills --component rules
nodus add ../local-package --adapter opencode
nodus doctor
```

## Notes

- Prefer tags for released packages.
- Use `--dev` for workspace-local tooling packages that should not be re-exported.
- Let Nodus write `nodus.toml`, `nodus.lock`, and managed outputs.
- For beginner users, prefer one clear recommendation. Default to `nodus add nodus-rs/nodus --global --adapter <adapter>` unless they clearly want project-scoped setup.
- If the user is not technical, avoid explaining `components`, `lockfile`, or runtime folders unless they ask.
- When possible, tell the user:
  "I installed the package, verified it with `nodus doctor`, and Nodus updated the repo files it manages."
