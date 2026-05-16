use super::{Adapter, Adapters, ArtifactKind};
use crate::manifest::{HookEvent, HookSessionSource, HookTool};

pub(crate) struct AdapterProfile {
    adapter: Adapter,
    runtime_root: &'static str,
    preferred_surface: PreferredSurface,
    artifacts: &'static [ArtifactKind],
    hooks: HookSupport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreferredSurface {
    DirectManagedOutput,
    PackagePluginWorkspaceMarketplace,
}

struct HookSupport {
    events: &'static [HookEvent],
    session_start_sources: &'static [HookSessionSource],
    tool_matchers: &'static [(HookTool, &'static str)],
}

const ALL_HOOK_EVENTS_EXCEPT_PERMISSION_REQUEST: &[HookEvent] = &[
    HookEvent::SessionStart,
    HookEvent::UserPromptSubmit,
    HookEvent::PreToolUse,
    HookEvent::PostToolUse,
    HookEvent::Stop,
    HookEvent::SubagentStop,
    HookEvent::SessionEnd,
];

const CODEX_HOOK_EVENTS: &[HookEvent] = &[
    HookEvent::SessionStart,
    HookEvent::UserPromptSubmit,
    HookEvent::PreToolUse,
    HookEvent::PermissionRequest,
    HookEvent::PostToolUse,
    HookEvent::Stop,
];

const OPENCODE_HOOK_EVENTS: &[HookEvent] = &[
    HookEvent::SessionStart,
    HookEvent::PreToolUse,
    HookEvent::PostToolUse,
    HookEvent::Stop,
];

const COPILOT_HOOK_EVENTS: &[HookEvent] = &[
    HookEvent::SessionStart,
    HookEvent::UserPromptSubmit,
    HookEvent::PreToolUse,
    HookEvent::PostToolUse,
    HookEvent::Stop,
    HookEvent::SubagentStop,
    HookEvent::SessionEnd,
];

const ALL_SESSION_START_SOURCES: &[HookSessionSource] = &[
    HookSessionSource::Startup,
    HookSessionSource::Resume,
    HookSessionSource::Clear,
    HookSessionSource::Compact,
];

const STARTUP_AND_RESUME: &[HookSessionSource] =
    &[HookSessionSource::Startup, HookSessionSource::Resume];

const STARTUP_RESUME_CLEAR: &[HookSessionSource] = &[
    HookSessionSource::Startup,
    HookSessionSource::Resume,
    HookSessionSource::Clear,
];

const STARTUP_ONLY: &[HookSessionSource] = &[HookSessionSource::Startup];

const CLAUDE_TOOL_MATCHERS: &[(HookTool, &str)] = &[
    (HookTool::Bash, "Bash"),
    (HookTool::Read, "Read"),
    (HookTool::Edit, "Edit"),
    (HookTool::Write, "Write"),
    (HookTool::MultiEdit, "MultiEdit"),
    (HookTool::Glob, "Glob"),
    (HookTool::Grep, "Grep"),
    (HookTool::WebFetch, "WebFetch"),
    (HookTool::WebSearch, "WebSearch"),
    (HookTool::Task, "Task"),
];

const CODEX_TOOL_MATCHERS: &[(HookTool, &str)] = &[
    (HookTool::ApplyPatch, "apply_patch"),
    (HookTool::Bash, "Bash"),
    (HookTool::Edit, "Edit"),
    (HookTool::Write, "Write"),
];

const OPENCODE_TOOL_MATCHERS: &[(HookTool, &str)] = &[
    (HookTool::ApplyPatch, "apply_patch"),
    (HookTool::Bash, "bash"),
    (HookTool::Edit, "edit"),
    (HookTool::Glob, "glob"),
    (HookTool::Grep, "grep"),
    (HookTool::MultiEdit, "multi_edit"),
    (HookTool::Read, "read"),
    (HookTool::Task, "task"),
    (HookTool::WebFetch, "web_fetch"),
    (HookTool::WebSearch, "web_search"),
    (HookTool::Write, "write"),
];

const COPILOT_TOOL_MATCHERS: &[(HookTool, &str)] = &[
    (HookTool::Bash, "bash"),
    (HookTool::Read, "view"),
    (HookTool::Edit, "edit"),
    (HookTool::Write, "create"),
    (HookTool::Glob, "glob"),
    (HookTool::Grep, "grep"),
    (HookTool::WebFetch, "web_fetch"),
    (HookTool::Task, "task"),
];

const ALL_SKILL_ARTIFACTS: &[ArtifactKind] = &[
    ArtifactKind::Skill,
    ArtifactKind::Agent,
    ArtifactKind::Rule,
    ArtifactKind::Command,
];

const AGENTS_ARTIFACTS: &[ArtifactKind] = &[ArtifactKind::Skill, ArtifactKind::Command];
const CODEX_ARTIFACTS: &[ArtifactKind] = &[ArtifactKind::Skill, ArtifactKind::Agent];
const COPILOT_ARTIFACTS: &[ArtifactKind] = &[ArtifactKind::Skill, ArtifactKind::Agent];
const CURSOR_ARTIFACTS: &[ArtifactKind] = &[
    ArtifactKind::Skill,
    ArtifactKind::Rule,
    ArtifactKind::Command,
];
const OPENCODE_ARTIFACTS: &[ArtifactKind] = ALL_SKILL_ARTIFACTS;

const NO_HOOK_SUPPORT: HookSupport = HookSupport {
    events: &[],
    session_start_sources: &[],
    tool_matchers: &[],
};

const AGENTS_PROFILE: AdapterProfile = AdapterProfile {
    adapter: Adapter::Agents,
    runtime_root: ".agents",
    preferred_surface: PreferredSurface::DirectManagedOutput,
    artifacts: AGENTS_ARTIFACTS,
    hooks: NO_HOOK_SUPPORT,
};

const CLAUDE_PROFILE: AdapterProfile = AdapterProfile {
    adapter: Adapter::Claude,
    runtime_root: ".claude",
    preferred_surface: PreferredSurface::PackagePluginWorkspaceMarketplace,
    artifacts: ALL_SKILL_ARTIFACTS,
    hooks: HookSupport {
        events: ALL_HOOK_EVENTS_EXCEPT_PERMISSION_REQUEST,
        session_start_sources: ALL_SESSION_START_SOURCES,
        tool_matchers: CLAUDE_TOOL_MATCHERS,
    },
};

const CODEX_PROFILE: AdapterProfile = AdapterProfile {
    adapter: Adapter::Codex,
    runtime_root: ".codex",
    preferred_surface: PreferredSurface::PackagePluginWorkspaceMarketplace,
    artifacts: CODEX_ARTIFACTS,
    hooks: HookSupport {
        events: CODEX_HOOK_EVENTS,
        session_start_sources: STARTUP_RESUME_CLEAR,
        tool_matchers: CODEX_TOOL_MATCHERS,
    },
};

const COPILOT_PROFILE: AdapterProfile = AdapterProfile {
    adapter: Adapter::Copilot,
    runtime_root: ".github",
    preferred_surface: PreferredSurface::DirectManagedOutput,
    artifacts: COPILOT_ARTIFACTS,
    hooks: HookSupport {
        events: COPILOT_HOOK_EVENTS,
        session_start_sources: STARTUP_AND_RESUME,
        tool_matchers: COPILOT_TOOL_MATCHERS,
    },
};

const CURSOR_PROFILE: AdapterProfile = AdapterProfile {
    adapter: Adapter::Cursor,
    runtime_root: ".cursor",
    preferred_surface: PreferredSurface::DirectManagedOutput,
    artifacts: CURSOR_ARTIFACTS,
    hooks: NO_HOOK_SUPPORT,
};

const OPENCODE_PROFILE: AdapterProfile = AdapterProfile {
    adapter: Adapter::OpenCode,
    runtime_root: ".opencode",
    preferred_surface: PreferredSurface::DirectManagedOutput,
    artifacts: OPENCODE_ARTIFACTS,
    hooks: HookSupport {
        events: OPENCODE_HOOK_EVENTS,
        session_start_sources: STARTUP_ONLY,
        tool_matchers: OPENCODE_TOOL_MATCHERS,
    },
};

pub(crate) fn adapter_profile(adapter: Adapter) -> &'static AdapterProfile {
    let profile = match adapter {
        Adapter::Agents => &AGENTS_PROFILE,
        Adapter::Claude => &CLAUDE_PROFILE,
        Adapter::Codex => &CODEX_PROFILE,
        Adapter::Copilot => &COPILOT_PROFILE,
        Adapter::Cursor => &CURSOR_PROFILE,
        Adapter::OpenCode => &OPENCODE_PROFILE,
    };
    debug_assert_eq!(profile.adapter, adapter);
    profile
}

pub(crate) fn artifact_supported(adapter: Adapter, kind: ArtifactKind) -> bool {
    adapter_profile(adapter).artifacts.contains(&kind)
}

pub(crate) fn supported_adapters(kind: ArtifactKind) -> Adapters {
    Adapter::ALL
        .into_iter()
        .filter(|adapter| artifact_supported(*adapter, kind))
        .fold(Adapters::NONE, |supported, adapter| {
            supported.union(adapter.into())
        })
}

pub(crate) fn hook_event_supported(adapter: Adapter, event: HookEvent) -> bool {
    adapter_profile(adapter).hooks.events.contains(&event)
}

pub(crate) fn session_start_source_supported(adapter: Adapter, source: HookSessionSource) -> bool {
    adapter_profile(adapter)
        .hooks
        .session_start_sources
        .contains(&source)
}

pub(crate) fn hook_tool_matcher(adapter: Adapter, tool: HookTool) -> Option<&'static str> {
    adapter_profile(adapter)
        .hooks
        .tool_matchers
        .iter()
        .find_map(|(candidate, spelling)| (*candidate == tool).then_some(*spelling))
}

pub(crate) fn runtime_root_name(adapter: Adapter) -> &'static str {
    adapter_profile(adapter).runtime_root
}

pub(crate) fn preferred_surface(adapter: Adapter) -> PreferredSurface {
    adapter_profile(adapter).preferred_surface
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapters_for(kind: ArtifactKind) -> Vec<Adapter> {
        supported_adapters(kind).iter().collect()
    }

    #[test]
    fn artifact_support_matches_current_matrix() {
        assert_eq!(adapters_for(ArtifactKind::Skill), Adapter::ALL);
        assert_eq!(
            adapters_for(ArtifactKind::Agent),
            vec![
                Adapter::Claude,
                Adapter::Codex,
                Adapter::Copilot,
                Adapter::OpenCode
            ]
        );
        assert_eq!(
            adapters_for(ArtifactKind::Rule),
            vec![Adapter::Claude, Adapter::Cursor, Adapter::OpenCode]
        );
        assert_eq!(
            adapters_for(ArtifactKind::Command),
            vec![
                Adapter::Agents,
                Adapter::Claude,
                Adapter::Cursor,
                Adapter::OpenCode
            ]
        );
    }

    #[test]
    fn native_plugin_adapters_prefer_package_plugin_surface() {
        assert_eq!(
            preferred_surface(Adapter::Claude),
            PreferredSurface::PackagePluginWorkspaceMarketplace
        );
        assert_eq!(
            preferred_surface(Adapter::Codex),
            PreferredSurface::PackagePluginWorkspaceMarketplace
        );

        for adapter in [
            Adapter::Agents,
            Adapter::Copilot,
            Adapter::Cursor,
            Adapter::OpenCode,
        ] {
            assert_eq!(
                preferred_surface(adapter),
                PreferredSurface::DirectManagedOutput
            );
        }
    }

    #[test]
    fn hook_event_support_matches_current_matrix() {
        for event in [
            HookEvent::SessionStart,
            HookEvent::UserPromptSubmit,
            HookEvent::PreToolUse,
            HookEvent::PostToolUse,
            HookEvent::Stop,
            HookEvent::SubagentStop,
            HookEvent::SessionEnd,
        ] {
            assert!(hook_event_supported(Adapter::Claude, event));
        }
        assert!(!hook_event_supported(
            Adapter::Claude,
            HookEvent::PermissionRequest
        ));

        for event in CODEX_HOOK_EVENTS {
            assert!(hook_event_supported(Adapter::Codex, *event));
        }
        assert!(!hook_event_supported(
            Adapter::Codex,
            HookEvent::SubagentStop
        ));
        assert!(!hook_event_supported(Adapter::Codex, HookEvent::SessionEnd));

        for event in OPENCODE_HOOK_EVENTS {
            assert!(hook_event_supported(Adapter::OpenCode, *event));
        }
        assert!(!hook_event_supported(
            Adapter::OpenCode,
            HookEvent::UserPromptSubmit
        ));
        assert!(!hook_event_supported(
            Adapter::OpenCode,
            HookEvent::PermissionRequest
        ));

        for event in COPILOT_HOOK_EVENTS {
            assert!(hook_event_supported(Adapter::Copilot, *event));
        }
        assert!(!hook_event_supported(
            Adapter::Copilot,
            HookEvent::PermissionRequest
        ));

        for adapter in [Adapter::Agents, Adapter::Cursor] {
            for event in [
                HookEvent::SessionStart,
                HookEvent::UserPromptSubmit,
                HookEvent::PreToolUse,
                HookEvent::PermissionRequest,
                HookEvent::PostToolUse,
                HookEvent::Stop,
                HookEvent::SubagentStop,
                HookEvent::SessionEnd,
            ] {
                assert!(!hook_event_supported(adapter, event));
            }
        }
    }

    #[test]
    fn session_start_source_support_matches_current_matrix() {
        for source in ALL_SESSION_START_SOURCES {
            assert!(session_start_source_supported(Adapter::Claude, *source));
        }

        assert!(session_start_source_supported(
            Adapter::Codex,
            HookSessionSource::Startup
        ));
        assert!(session_start_source_supported(
            Adapter::Codex,
            HookSessionSource::Resume
        ));
        assert!(session_start_source_supported(
            Adapter::Codex,
            HookSessionSource::Clear
        ));
        assert!(!session_start_source_supported(
            Adapter::Codex,
            HookSessionSource::Compact
        ));

        assert!(session_start_source_supported(
            Adapter::Copilot,
            HookSessionSource::Startup
        ));
        assert!(session_start_source_supported(
            Adapter::Copilot,
            HookSessionSource::Resume
        ));
        assert!(!session_start_source_supported(
            Adapter::Copilot,
            HookSessionSource::Clear
        ));
        assert!(!session_start_source_supported(
            Adapter::Copilot,
            HookSessionSource::Compact
        ));

        assert!(session_start_source_supported(
            Adapter::OpenCode,
            HookSessionSource::Startup
        ));
        assert!(!session_start_source_supported(
            Adapter::OpenCode,
            HookSessionSource::Resume
        ));

        for adapter in [Adapter::Agents, Adapter::Cursor] {
            assert!(!session_start_source_supported(
                adapter,
                HookSessionSource::Startup
            ));
        }
    }

    #[test]
    fn tool_matcher_spellings_match_current_outputs() {
        assert_eq!(
            hook_tool_matcher(Adapter::Claude, HookTool::MultiEdit),
            Some("MultiEdit")
        );
        assert_eq!(
            hook_tool_matcher(Adapter::Codex, HookTool::Bash),
            Some("Bash")
        );
        assert_eq!(
            hook_tool_matcher(Adapter::Codex, HookTool::ApplyPatch),
            Some("apply_patch")
        );
        assert_eq!(
            hook_tool_matcher(Adapter::Codex, HookTool::Edit),
            Some("Edit")
        );
        assert_eq!(
            hook_tool_matcher(Adapter::Codex, HookTool::Write),
            Some("Write")
        );
        assert_eq!(
            hook_tool_matcher(Adapter::OpenCode, HookTool::ApplyPatch),
            Some("apply_patch")
        );
        assert_eq!(
            hook_tool_matcher(Adapter::Copilot, HookTool::Read),
            Some("view")
        );
        assert_eq!(hook_tool_matcher(Adapter::Cursor, HookTool::Bash), None);
    }

    #[test]
    fn runtime_root_names_match_current_paths() {
        assert_eq!(runtime_root_name(Adapter::Agents), ".agents");
        assert_eq!(runtime_root_name(Adapter::Claude), ".claude");
        assert_eq!(runtime_root_name(Adapter::Codex), ".codex");
        assert_eq!(runtime_root_name(Adapter::Copilot), ".github");
        assert_eq!(runtime_root_name(Adapter::Cursor), ".cursor");
        assert_eq!(runtime_root_name(Adapter::OpenCode), ".opencode");
    }
}
