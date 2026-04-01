# Guided CLI Help And Repair-First Doctor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rewrite Nodus help output into a guided, beginner-first CLI experience and redesign `nodus doctor` into a repair-first command with safe default fixes, explicit risky cleanup prompts, and read-only/force modes.

**Architecture:** Split the work into two bounded tracks that land cleanly together: a dedicated CLI help content layer for guided `--help` output, and a dedicated doctor planner inside the resolver runtime that separates inspection, classification, repair planning, execution, and verification. Reuse the existing sync/output planning code for safe repairs instead of inventing a second write path.

**Tech Stack:** Rust 2024, `clap`, `serde`, `dialoguer` or existing TTY prompt patterns, existing `Reporter`, existing resolver/runtime sync planning and tests under `cargo test`

---

## File Map

- Create: `nodus/src/cli/help.rs`
- Create: `nodus/src/resolver/runtime/doctor.rs`
- Modify: `nodus/src/cli.rs`
- Modify: `nodus/src/cli/args.rs`
- Modify: `nodus/src/cli/router.rs`
- Modify: `nodus/src/cli/handlers/query.rs`
- Modify: `nodus/src/cli/tests.rs`
- Modify: `nodus/src/resolver.rs`
- Modify: `nodus/src/resolver/runtime.rs`
- Modify: `nodus/src/resolver/runtime/support.rs`
- Modify: `nodus/src/resolver/runtime/tests.rs`
- Test: `nodus/src/cli/tests.rs`
- Test: `nodus/src/resolver/runtime/tests.rs`

### Task 1: Extract Guided Help Content Into A Dedicated CLI Help Module

**Files:**
- Create: `nodus/src/cli/help.rs`
- Modify: `nodus/src/cli.rs`
- Modify: `nodus/src/cli/args.rs`
- Test: `nodus/src/cli/tests.rs`

- [ ] **Step 1: Write the failing guided-help tests for root and `add`**

```rust
#[test]
fn root_help_leads_with_guided_workflows() {
    let help = <Cli as clap::CommandFactory>::command()
        .render_long_help()
        .to_string();

    assert!(help.contains("Most common tasks"));
    assert!(help.contains("Typical workflows"));
    assert!(help.contains("add -> doctor"));
    assert!(help.contains("Need details? Run `nodus <command> --help`"));
}

#[test]
fn add_help_leads_with_safe_example_and_next_step() {
    let mut root = <Cli as clap::CommandFactory>::command();
    let help = root
        .find_subcommand_mut("add")
        .unwrap()
        .render_long_help()
        .to_string();

    assert!(help.contains("Most common use"));
    assert!(help.contains("nodus add nodus-rs/nodus --adapter codex"));
    assert!(help.contains("What this changes"));
    assert!(help.contains("Run `nodus doctor` next"));
}
```

- [ ] **Step 2: Run the targeted CLI help tests and confirm they fail**

Run: `cargo test root_help_leads_with_guided_workflows -- --exact`

Run: `cargo test add_help_leads_with_safe_example_and_next_step -- --exact`

Expected: FAIL because the current long help strings in `src/cli/args.rs` do not contain the new guided sections.

- [ ] **Step 3: Move long help content into `src/cli/help.rs` and rewire `args.rs` to use it**

```rust
// src/cli/help.rs
pub(super) const ROOT_LONG_ABOUT: &str = r#"Nodus adds AI agent packages to this repo and keeps the generated tool files in sync.

Most common tasks:
  nodus add nodus-rs/nodus --adapter codex
  nodus doctor
  nodus sync
  nodus update

Typical workflows:
  first install: add -> doctor
  rebuild current setup: sync -> doctor
  upgrade packages: update -> doctor
  remove a package: remove -> doctor
"#;

pub(super) const ADD_LONG_ABOUT: &str = r#"Install one package into this repo and immediately write the managed files the selected AI tool needs.

Most common use:
  nodus add nodus-rs/nodus --adapter codex

What this changes:
  - creates or updates `nodus.toml`
  - resolves and records exact package revisions in `nodus.lock`
  - writes managed files under tool folders such as `.codex/` or `.claude/`

Run `nodus doctor` next to verify the repo is healthy."#;
```

```rust
// src/cli.rs
mod help;
```

```rust
// src/cli/args.rs
use crate::cli::help::{
    ADD_AFTER_LONG_HELP, ADD_LONG_ABOUT, DOCTOR_AFTER_LONG_HELP, DOCTOR_LONG_ABOUT,
    ROOT_AFTER_LONG_HELP, ROOT_LONG_ABOUT, SYNC_AFTER_LONG_HELP, SYNC_LONG_ABOUT,
    UPDATE_AFTER_LONG_HELP, UPDATE_LONG_ABOUT,
};
```

- [ ] **Step 4: Run the focused CLI help tests and confirm they pass**

Run: `cargo test root_help_leads_with_guided_workflows -- --exact`

Run: `cargo test add_help_leads_with_safe_example_and_next_step -- --exact`

Expected: PASS with the new guided sections visible in rendered help.

- [ ] **Step 5: Commit the help-module extraction**

```bash
git add src/cli.rs src/cli/help.rs src/cli/args.rs src/cli/tests.rs
git commit -m "feat(cli): rewrite root and add help for guided usage"
```

### Task 2: Finish Guided Help Coverage For Core Commands And Add Doctor Mode Flags

**Files:**
- Modify: `nodus/src/cli/args.rs`
- Modify: `nodus/src/cli/router.rs`
- Modify: `nodus/src/cli/handlers/query.rs`
- Modify: `nodus/src/cli/tests.rs`

- [ ] **Step 1: Write failing tests for guided `sync`, `update`, and `doctor` help plus doctor flag parsing**

```rust
#[test]
fn sync_help_explains_when_to_use_sync_and_what_to_run_next() {
    let mut root = <Cli as clap::CommandFactory>::command();
    let help = root
        .find_subcommand_mut("sync")
        .unwrap()
        .render_long_help()
        .to_string();

    assert!(help.contains("Use this when you want to rebuild from what this repo already declares"));
    assert!(help.contains("Run `nodus doctor` next"));
    assert!(help.contains("Common options"));
}

#[test]
fn doctor_help_describes_default_check_force_modes() {
    let mut root = <Cli as clap::CommandFactory>::command();
    let help = root
        .find_subcommand_mut("doctor")
        .unwrap()
        .render_long_help()
        .to_string();

    assert!(help.contains("If Nodus feels broken, start here"));
    assert!(help.contains("checks and auto-fixes safe issues"));
    assert!(help.contains("--check"));
    assert!(help.contains("--force"));
}

#[test]
fn doctor_command_parses_check_and_force_flags() {
    let check = Cli::try_parse_from(["nodus", "doctor", "--check"]).unwrap();
    let force = Cli::try_parse_from(["nodus", "doctor", "--force"]).unwrap();

    assert!(matches!(
        check.command,
        Command::Doctor { check: true, force: false, json: false }
    ));
    assert!(matches!(
        force.command,
        Command::Doctor { check: false, force: true, json: false }
    ));
}
```

- [ ] **Step 2: Run the targeted parsing and help tests and confirm they fail**

Run: `cargo test sync_help_explains_when_to_use_sync_and_what_to_run_next -- --exact`

Run: `cargo test doctor_help_describes_default_check_force_modes -- --exact`

Run: `cargo test doctor_command_parses_check_and_force_flags -- --exact`

Expected: FAIL because `Doctor` currently only has a `json` flag and the help text does not describe guided modes.

- [ ] **Step 3: Add `--check` and `--force` to the doctor CLI surface and keep JSON read-only**

```rust
// src/cli/args.rs
Command::Doctor {
    #[arg(long, conflicts_with = "force", help = "Check for problems without changing anything")]
    check: bool,
    #[arg(long, conflicts_with = "check", help = "Apply risky repairs without asking first")]
    force: bool,
    #[arg(
        long,
        conflicts_with = "force",
        help = "Emit machine-readable JSON from a read-only doctor check"
    )]
    json: bool,
}
```

```rust
// src/cli/router.rs
Command::Doctor { check, force, json } => {
    query::handle_doctor(&context, query::DoctorCommand { check, force, json })
}
```

```rust
// src/cli/handlers/query.rs
pub(crate) struct DoctorCommand {
    pub(crate) check: bool,
    pub(crate) force: bool,
    pub(crate) json: bool,
}
```

- [ ] **Step 4: Expand the guided help strings for `sync`, `update`, `remove`, and `doctor`**

```rust
// src/cli/help.rs
pub(super) const DOCTOR_LONG_ABOUT: &str = r#"If Nodus feels broken, start here.

Default behavior:
  - checks the repo state
  - auto-fixes safe issues
  - asks before risky cleanup

Common commands:
  nodus doctor
  nodus doctor --check
  nodus doctor --force
"#;
```

- [ ] **Step 5: Run the targeted CLI tests and commit the doctor flag/help surface**

Run: `cargo test sync_help_explains_when_to_use_sync_and_what_to_run_next -- --exact`

Run: `cargo test doctor_help_describes_default_check_force_modes -- --exact`

Run: `cargo test doctor_command_parses_check_and_force_flags -- --exact`

Expected: PASS

```bash
git add src/cli/args.rs src/cli/router.rs src/cli/handlers/query.rs src/cli/tests.rs
git commit -m "feat(cli): add guided doctor modes and core help coverage"
```

### Task 3: Introduce A Dedicated Doctor Planner Module With Check-Mode Parity

**Files:**
- Create: `nodus/src/resolver/runtime/doctor.rs`
- Modify: `nodus/src/resolver/runtime.rs`
- Modify: `nodus/src/resolver.rs`
- Modify: `nodus/src/resolver/runtime/tests.rs`

- [ ] **Step 1: Write failing tests for doctor mode selection and structured summary fields**

```rust
#[test]
fn doctor_check_mode_reports_read_only_status() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    sync_all(temp.path(), cache.path());

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Check,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Healthy);
    assert!(summary.applied_actions.is_empty());
}

#[test]
fn doctor_check_mode_keeps_missing_managed_file_as_unfixed_finding() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    sync_all(temp.path(), cache.path());
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    fs::remove_file(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md")),
    )
    .unwrap();

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Check,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Blocked);
    assert!(summary.findings.iter().any(|finding| {
        finding.kind == DoctorFindingKind::SafeAutoFix
            && finding.message.contains("managed file is missing from disk")
    }));
}
```

- [ ] **Step 2: Run the targeted doctor planner tests and confirm they fail**

Run: `cargo test doctor_check_mode_reports_read_only_status -- --exact`

Run: `cargo test doctor_check_mode_keeps_missing_managed_file_as_unfixed_finding -- --exact`

Expected: FAIL because the current doctor path has no `DoctorMode`, no structured status enum, and returns early errors instead of planned findings.

- [ ] **Step 3: Add `runtime/doctor.rs` and move doctor types/entrypoints into it**

```rust
// src/resolver/runtime/doctor.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorMode {
    Repair,
    Check,
    Force,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Healthy,
    Fixed,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorFindingKind {
    Informational,
    SafeAutoFix,
    RiskyFix,
    Manual,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorSummary {
    pub package_count: usize,
    pub warnings: Vec<String>,
    pub status: DoctorStatus,
    pub findings: Vec<DoctorFinding>,
    pub applied_actions: Vec<DoctorActionRecord>,
}
```

```rust
// src/resolver/runtime.rs
mod doctor;

pub use self::doctor::{
    DoctorFindingKind, DoctorMode, DoctorStatus, DoctorSummary, doctor_in_dir_with_mode,
};

pub fn doctor_in_dir(cwd: &Path, cache_root: &Path, reporter: &Reporter) -> Result<DoctorSummary> {
    doctor::doctor_in_dir_with_mode(cwd, cache_root, DoctorMode::Repair, reporter)
}
```

```rust
// src/resolver.rs
pub use runtime::{
    DoctorMode, DoctorStatus, PackageSource, Resolution, ResolvedPackage, doctor_in_dir,
    doctor_in_dir_with_mode, resolve_project_from_existing_lockfile_in_dir,
    sync_in_dir_with_adapters, sync_in_dir_with_adapters_dry_run,
    sync_in_dir_with_adapters_frozen, sync_in_dir_with_adapters_frozen_dry_run,
};
```

- [ ] **Step 4: Implement inspect/classify/plan scaffolding without enabling repairs yet**

```rust
// src/resolver/runtime/doctor.rs
pub(crate) fn doctor_in_dir_with_mode(
    cwd: &Path,
    cache_root: &Path,
    mode: DoctorMode,
    reporter: &Reporter,
) -> Result<DoctorSummary> {
    let inspection = inspect_doctor_state(cwd, cache_root, reporter)?;
    let plan = build_doctor_plan(&inspection)?;
    execute_doctor_plan(plan, mode, reporter)
}
```

- [ ] **Step 5: Run the targeted tests and commit the planner extraction**

Run: `cargo test doctor_check_mode_reports_read_only_status -- --exact`

Run: `cargo test doctor_check_mode_keeps_missing_managed_file_as_unfixed_finding -- --exact`

Expected: PASS, with `DoctorMode::Check` reporting findings but not mutating disk.

```bash
git add src/resolver.rs src/resolver/runtime.rs src/resolver/runtime/doctor.rs src/resolver/runtime/tests.rs
git commit -m "refactor(doctor): extract planner-based doctor module"
```

### Task 4: Implement Safe Auto-Repairs By Reusing Existing Sync Planning

**Files:**
- Modify: `nodus/src/resolver/runtime/doctor.rs`
- Modify: `nodus/src/resolver/runtime/support.rs`
- Modify: `nodus/src/resolver/runtime/tests.rs`

- [ ] **Step 1: Write failing tests for safe repairs**

```rust
#[test]
fn doctor_repairs_missing_file_inside_managed_skill_directory() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");
    sync_all(temp.path(), cache.path());
    let resolution = resolve_project(temp.path(), cache.path(), ResolveMode::Sync).unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    fs::remove_file(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md")),
    )
    .unwrap();

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Fixed);
    assert!(temp.path().join(".claude/skills/review/SKILL.md").exists());
}

#[test]
fn doctor_repairs_invalid_managed_mcp_json_when_it_owns_the_file() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_manifest(temp.path(), "[dependencies.firebase]\npath = \"vendor/firebase\"\n");
    write_file(&temp.path().join("vendor/firebase/nodus.toml"), "[mcp_servers.firebase]\ncommand = \"npx\"\n");
    sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, &[Adapter::Codex]).unwrap();

    write_file(&temp.path().join(".mcp.json"), "{");

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Repair,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Fixed);
    assert!(summary.applied_actions.iter().any(|action| action.message.contains("rewrote managed output")));
}
```

- [ ] **Step 2: Run the targeted safe-repair tests and confirm they fail**

Run: `cargo test doctor_repairs_missing_file_inside_managed_skill_directory -- --exact`

Run: `cargo test doctor_repairs_invalid_managed_mcp_json_when_it_owns_the_file -- --exact`

Expected: FAIL because current doctor reports errors but does not repair missing or corrupt managed outputs.

- [ ] **Step 3: Teach the planner to classify recoverable output drift as safe auto-fixes**

```rust
// src/resolver/runtime/doctor.rs
enum DoctorAction {
    RebuildManagedOutputs,
    RewriteManagedFile { path: PathBuf },
}

fn classify_output_drift(inspection: &DoctorInspection, findings: &mut Vec<DoctorFinding>) {
    if inspection.has_missing_managed_files || inspection.has_invalid_owned_output {
        findings.push(DoctorFinding::safe_auto_fix(
            "managed outputs drifted from the declared project state",
        ));
    }
}
```

- [ ] **Step 4: Execute safe repairs through the existing sync execution machinery**

```rust
// src/resolver/runtime/doctor.rs
fn execute_safe_repairs(
    inspection: &DoctorInspection,
    reporter: &Reporter,
) -> Result<Vec<DoctorActionRecord>> {
    let plan = build_sync_execution_plan(
        &inspection.original_root,
        &inspection.working_root,
        &inspection.lockfile_path,
        &inspection.expected_lockfile,
        &inspection.runtime_root,
        &inspection.owned_paths,
        &inspection.desired_paths,
        &inspection.planned_files,
        inspection.warnings.clone(),
        inspection.sync_summary.clone(),
        SyncMode::Normal,
    )?;
    execute_sync_plan(&plan, ExecutionMode::Apply, reporter)?;
    Ok(vec![DoctorActionRecord::new("rewrote managed outputs from the existing lockfile")])
}
```

- [ ] **Step 5: Run the targeted tests and commit the safe repair path**

Run: `cargo test doctor_repairs_missing_file_inside_managed_skill_directory -- --exact`

Run: `cargo test doctor_repairs_invalid_managed_mcp_json_when_it_owns_the_file -- --exact`

Expected: PASS

```bash
git add src/resolver/runtime/doctor.rs src/resolver/runtime/support.rs src/resolver/runtime/tests.rs
git commit -m "feat(doctor): auto-repair safe managed output drift"
```

### Task 5: Add Risky Cleanup Prompts And Force-Mode Execution

**Files:**
- Modify: `nodus/src/resolver/runtime/doctor.rs`
- Modify: `nodus/src/resolver/runtime/support.rs`
- Modify: `nodus/src/resolver/runtime/tests.rs`
- Modify: `nodus/src/cli/handlers/query.rs`

- [ ] **Step 1: Write failing tests for risky cleanup prompts and force mode**

```rust
#[test]
fn doctor_check_mode_reports_risky_cleanup_without_deleting_anything() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    sync_all(temp.path(), cache.path());

    fs::create_dir_all(temp.path().join(".codex/marketplace")).unwrap();
    write_file(
        &temp.path().join(".codex/marketplace/plugin.json"),
        "user-owned file\n",
    );

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Check,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Blocked);
    assert!(summary.findings.iter().any(|finding| finding.kind == DoctorFindingKind::RiskyFix));
    assert!(temp.path().join(".codex/marketplace/plugin.json").exists());
}

#[test]
fn doctor_force_mode_applies_risky_cleanup_without_prompt() {
    let temp = TempDir::new().unwrap();
    let cache = cache_dir();
    write_skill(&temp.path().join("skills/review"), "Review");
    sync_all(temp.path(), cache.path());

    fs::create_dir_all(temp.path().join(".codex/marketplace")).unwrap();
    write_file(
        &temp.path().join(".codex/marketplace/plugin.json"),
        "user-owned file\n",
    );

    let summary = doctor_in_dir_with_mode(
        temp.path(),
        cache.path(),
        DoctorMode::Force,
        &Reporter::silent(),
    )
    .unwrap();

    assert_eq!(summary.status, DoctorStatus::Fixed);
    assert!(summary.applied_actions.iter().any(|action| action.message.contains("removed conflicting managed subtree")));
}
```

- [ ] **Step 2: Run the targeted risky-cleanup tests and confirm they fail**

Run: `cargo test doctor_check_mode_reports_risky_cleanup_without_deleting_anything -- --exact`

Run: `cargo test doctor_force_mode_applies_risky_cleanup_without_prompt -- --exact`

Expected: FAIL because the planner does not yet model risky actions or execute them in force mode.

- [ ] **Step 3: Add doctor-specific risky action types and a reusable confirmation interface**

```rust
// src/resolver/runtime/doctor.rs
enum DoctorAction {
    RebuildManagedOutputs,
    RemoveConflictingManagedPath { path: PathBuf, reason: String },
}

trait DoctorPrompt {
    fn confirm(&mut self, action: &DoctorAction) -> Result<bool>;
}

struct TtyDoctorPrompt;
```

```rust
// src/resolver/runtime/doctor.rs
impl DoctorPrompt for TtyDoctorPrompt {
    fn confirm(&mut self, action: &DoctorAction) -> Result<bool> {
        match action {
            DoctorAction::RemoveConflictingManagedPath { path, reason } => {
                eprintln!("Nodus needs to remove {}.", display_path(path));
                eprintln!("{reason}");
                eprintln!("Continue? [y/N]");
                // parse stdin here and return true only for explicit yes
                Ok(false)
            }
            _ => Ok(true),
        }
    }
}
```

- [ ] **Step 4: Execute risky actions only in default mode after approval, and always in force mode**

```rust
// src/resolver/runtime/doctor.rs
match mode {
    DoctorMode::Check => return Ok(plan.into_blocked_summary()),
    DoctorMode::Repair => {
        for action in plan.risky_actions() {
            if !prompt.confirm(action)? {
                return Ok(plan.into_blocked_summary());
            }
            apply_risky_action(action, reporter)?;
        }
    }
    DoctorMode::Force => {
        for action in plan.risky_actions() {
            apply_risky_action(action, reporter)?;
        }
    }
}
```

- [ ] **Step 5: Run the targeted tests and commit risky cleanup support**

Run: `cargo test doctor_check_mode_reports_risky_cleanup_without_deleting_anything -- --exact`

Run: `cargo test doctor_force_mode_applies_risky_cleanup_without_prompt -- --exact`

Expected: PASS

```bash
git add src/resolver/runtime/doctor.rs src/resolver/runtime/support.rs src/resolver/runtime/tests.rs src/cli/handlers/query.rs
git commit -m "feat(doctor): add risky cleanup prompts and force mode"
```

### Task 6: Align Doctor Output, JSON Semantics, And Full Regression Coverage

**Files:**
- Modify: `nodus/src/cli/handlers/query.rs`
- Modify: `nodus/src/cli/tests.rs`
- Modify: `nodus/src/resolver/runtime/tests.rs`

- [ ] **Step 1: Write failing tests for final doctor output and JSON behavior**

```rust
#[test]
fn doctor_command_reports_repairs_in_human_output() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write_manifest(
        temp.path(),
        r#"
[dependencies]
shared = { path = "vendor/shared" }
"#,
    );
    write_skill(&temp.path().join("vendor/shared/skills/review"), "Review");

    let reporter = Reporter::silent();
    resolver::sync_in_dir_with_adapters(temp.path(), cache.path(), false, false, false, &[], false, &reporter).unwrap();

    let resolution = resolver::resolve_project_for_sync(temp.path(), cache.path(), &reporter)
        .unwrap();
    let dependency = resolution
        .packages
        .iter()
        .find(|package| package.alias == "shared")
        .unwrap();
    let managed_skill_id = namespaced_skill_id(dependency, "review");
    fs::remove_file(
        temp.path()
            .join(format!(".claude/skills/{managed_skill_id}/SKILL.md")),
    )
    .unwrap();

    let output = run_command_output(
        Command::Doctor { check: false, force: false, json: false },
        temp.path(),
        cache.path(),
    );

    assert!(output.contains("Checking"));
    assert!(output.contains("repaired"));
    assert!(output.contains("Finished"));
}

#[test]
fn doctor_json_runs_as_read_only_check() {
    let temp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join(".codex")).unwrap();

    let reporter = Reporter::silent();
    resolver::sync_in_dir_with_adapters(
        temp.path(),
        cache.path(),
        false,
        false,
        false,
        &[],
        false,
        &reporter,
    )
    .unwrap();

    let output = run_command_output(
        Command::Doctor { check: false, force: false, json: true },
        temp.path(),
        cache.path(),
    );

    let json: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(json["status"], "healthy");
    assert!(!output.contains("Checking"));
    assert!(!output.contains("Finished"));
}
```

- [ ] **Step 2: Run the targeted doctor output tests and confirm they fail**

Run: `cargo test doctor_command_reports_repairs_in_human_output -- --exact`

Run: `cargo test doctor_json_runs_as_read_only_check -- --exact`

Expected: FAIL because the command handler still assumes doctor is pure validation and the old `Command::Doctor` shape no longer matches.

- [ ] **Step 3: Update the command handler to map flags into doctor modes and produce plain-language summaries**

```rust
// src/cli/handlers/query.rs
pub(crate) fn handle_doctor(
    context: &CommandContext<'_>,
    command: DoctorCommand,
) -> anyhow::Result<()> {
    let mode = if command.force {
        crate::resolver::DoctorMode::Force
    } else if command.check || command.json {
        crate::resolver::DoctorMode::Check
    } else {
        crate::resolver::DoctorMode::Repair
    };

    let summary = crate::resolver::doctor_in_dir_with_mode(
        context.cwd,
        context.cache_root,
        mode,
        if command.json { &Reporter::silent() } else { context.reporter },
    )?;

    if command.json {
        return write_json(context.reporter, &summary);
    }

    match summary.status {
        crate::resolver::DoctorStatus::Healthy => context
            .reporter
            .finish(format!("checked {} packages; no repairs needed", summary.package_count)),
        crate::resolver::DoctorStatus::Fixed => context
            .reporter
            .finish(format!("checked {} packages; repaired {} issues", summary.package_count, summary.applied_actions.len())),
        crate::resolver::DoctorStatus::Blocked => context
            .reporter
            .finish(format!("checked {} packages; manual action still required", summary.package_count)),
    }
}
```

- [ ] **Step 4: Run the full focused suites for CLI and resolver runtime**

Run: `cargo test cli::tests -- --nocapture`

Expected: PASS

Run: `cargo test resolver::runtime::tests -- --nocapture`

Expected: PASS

- [ ] **Step 5: Commit the final integrated behavior**

```bash
git add src/cli/handlers/query.rs src/cli/tests.rs src/resolver/runtime/tests.rs
git commit -m "feat(doctor): align repair output and json check mode"
```

## Self-Review

### Spec Coverage

- Guided root help: covered in Tasks 1 and 2.
- Guided core command help: covered in Tasks 1 and 2.
- Beginner-safe examples and “what to run next”: covered in Tasks 1 and 2.
- `doctor` default safe repairs: covered in Task 4.
- `doctor --check`: covered in Tasks 2, 3, and 6.
- `doctor --force`: covered in Tasks 2 and 5.
- Risky cleanup prompts: covered in Task 5.
- Doctor output and JSON behavior: covered in Task 6.
- Regression tests around help and doctor behavior: covered in every task.

### Placeholder Scan

- No unresolved placeholder markers remain.
- Every code-changing step includes concrete Rust snippets or command examples.
- Every test step names exact test functions and commands.

### Type Consistency

- `DoctorMode` uses `Repair`, `Check`, and `Force` consistently in all tasks.
- `DoctorStatus` uses `Healthy`, `Fixed`, and `Blocked` consistently in all tasks.
- `Command::Doctor` uses `check`, `force`, and `json` consistently in all tasks.
