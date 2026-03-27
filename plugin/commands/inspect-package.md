# Inspect A Nodus Package

Inspect a local package, an installed dependency, or a remote package before changing workspace state.

Use this when the user wants to know what a package contains or when you need to inspect before installing.

## Commands

```bash
nodus info .
nodus info <dependency-alias>
nodus info owner/repo
```

## What To Check

- where the package comes from
- which version or tag it points to
- which skills, agents, rules, and commands it contains
- whether it depends on other packages
- whether it declares sensitive capabilities
- whether Nodus found warnings

## Guidance

- Use `nodus info .` when authoring a package repo.
- Use `nodus info <alias>` when debugging an installed dependency in a consuming workspace.
- For non-technical users, summarize the result in plain language:
  "This package contains two skills and one agent, and it does not request sensitive permissions."
