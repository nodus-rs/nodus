When working with Nodus packages or workspaces:

- Assume the user may not know Nodus terminology. Prefer plain language and define technical terms the first time you use them.
- Identify whether the repo is a consumer workspace or a package authoring repo before suggesting commands.
- Prefer `nodus add`, `nodus info`, `nodus sync`, `nodus doctor`, `nodus update`, `nodus remove`, and `nodus relay` over manual copying of runtime files.
- Treat `nodus.toml` and `nodus.lock` as the source of truth for dependency and install state.
- Prefer exact tags for released dependencies. Use branches or revisions only when the workflow truly requires them.
- Narrow `components` only when the workspace needs a subset of a package. Default to the full package for non-technical users.
- For package repos, reason from discovery roots, artifact ids, and manifest invariants rather than adapter-specific output folders.
- After changes, verify the result with `nodus doctor` or `nodus info`.
- After installing or updating a package, tell the user in simple language what changed and how to confirm the package is available.
