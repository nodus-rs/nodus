# Sync And Verify A Workspace

Refresh the files Nodus manages for the workspace, then verify that the setup is healthy.

Use this after installing, updating, or removing a package.

## Commands

```bash
nodus sync
nodus doctor
```

## CI Variants

```bash
nodus sync --locked
nodus sync --frozen
```

## When To Use Which

- Use `nodus sync` after manifest changes or package updates.
- Use `nodus sync --locked` in CI when `nodus.lock` is expected to already be current.
- Use `nodus sync --frozen` when you must install the exact Git revisions already written in `nodus.lock`.
- Always finish with `nodus doctor` when debugging drift or validating a fresh setup.
- For non-technical users, explain this as:
  "`nodus sync` makes the repo match the packages it is supposed to have, and `nodus doctor` checks that everything looks correct."
