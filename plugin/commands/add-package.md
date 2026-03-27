# Add Package With Nodus

Add a Nodus package to the current workspace without manually copying files.

This command is the normal starting point for non-technical users.

## Steps

1. Identify the package source:
   - GitHub shortcut like `owner/repo`
   - full Git URL
   - local path
2. Pick the AI tool that should receive the package.
   - `codex` for Codex
   - `claude` for Claude
   - `cursor` for Cursor
   - `copilot` for GitHub Copilot
   - `opencode` for OpenCode
3. Use the full package by default.
4. Run `nodus add`.
5. Run `nodus doctor` to confirm the install is healthy.
6. Tell the user what changed in simple language.

## Examples

```bash
nodus add WendellXY/nodus --adapter codex
nodus add WendellXY/nodus --adapter claude --component skills --component rules
nodus add ../local-package --adapter opencode
nodus doctor
```

## Notes

- Prefer tags for released packages.
- Use `--dev` for workspace-local tooling packages that should not be re-exported.
- Let Nodus write `nodus.toml`, `nodus.lock`, and managed outputs.
- If the user is not technical, avoid explaining `components`, `lockfile`, or runtime folders unless they ask.
- When possible, tell the user:
  "I installed the package, verified it with `nodus doctor`, and Nodus updated the repo files it manages."
