use ghis::agent_context::{AgentContext, SelectionSourceKind, SelectionState};
use ghis::app::AppContext;
use ghis::config::{
    Config, ConfigPaths, Profile, ProfileResolution, ResolutionSource, SigningProfile, SshProfile,
};
use ghis::repo::{Remote, Repository, Transport};
use std::path::PathBuf;

fn context(source: ResolutionSource, profile_id: Option<&str>) -> AppContext {
    let root = PathBuf::from("/safe/worktree");
    let profile = Profile {
        host: "github.example.test".into(),
        login: "alice".into(),
        git_name: "Alice Example".into(),
        git_email: "alice@example.test".into(),
        description: Some("CANARY_PROFILE_DESCRIPTION".into()),
        ssh: Some(SshProfile {
            public_key: Some(PathBuf::from("CANARY_PUBLIC_KEY_PATH")),
            fingerprint: Some("CANARY_FINGERPRINT".into()),
            agent_socket: Some(PathBuf::from("CANARY_AGENT_SOCKET")),
            ..SshProfile::default()
        }),
        signing: SigningProfile {
            enabled: true,
            signing_key: Some("CANARY_SIGNING_KEY".into()),
            program: Some(PathBuf::from("CANARY_SIGNING_PROGRAM")),
        },
    };
    let mut config = Config::default();
    config.profiles.insert("work".into(), profile.clone());
    config.rules.push(ghis::config::Rule {
        id: "CANARY_RAW_CONFIG_RULE".into(),
        remote: Some("CANARY_RAW_REMOTE_RULE".into()),
        ..ghis::config::Rule::default()
    });
    AppContext {
        paths: ConfigPaths::from_bases("/CANARY_CONFIG", "/CANARY_CACHE", "/CANARY_STATE"),
        config,
        repository: Some(Repository {
            path: root.clone(),
            git_dir: PathBuf::from("/CANARY_GIT_DIR"),
            common_dir: PathBuf::from("/CANARY_COMMON_DIR"),
            root: Some(root),
            bare: false,
        }),
        remote: Some(Remote {
            name: "origin".into(),
            url: "https://CANARY_TOKEN@github.example.test/acme/widget.git".into(),
            transport: Transport::Https,
            host: Some("github.example.test".into()),
            owner: Some("acme".into()),
            repo: Some("widget".into()),
        }),
        resolution: ProfileResolution {
            profile: profile_id.map(str::to_owned),
            source,
            candidates: profile_id.into_iter().map(str::to_owned).collect(),
            warnings: vec!["CANARY_RESOLUTION_WARNING".into()],
        },
        profile: profile_id.map(|_| profile),
        identities: None,
        warnings: vec!["CANARY_APP_WARNING".into(), "CANARY_REPAIR_GUIDANCE".into()],
    }
}

#[test]
fn allowlist_projection_does_not_leak_sensitive_or_diagnostic_fields() {
    let projected = AgentContext::from_app(&context(ResolutionSource::Explicit, Some("work")));
    let outputs = [
        projected.render_json().expect("json"),
        projected.render_claude(),
        projected.render_codex(),
    ];

    for output in outputs {
        for canary in [
            "CANARY_TOKEN",
            "CANARY_PUBLIC_KEY_PATH",
            "CANARY_FINGERPRINT",
            "CANARY_AGENT_SOCKET",
            "CANARY_SIGNING_KEY",
            "CANARY_SIGNING_PROGRAM",
            "CANARY_RAW_CONFIG_RULE",
            "CANARY_RAW_REMOTE_RULE",
            "CANARY_GIT_DIR",
            "CANARY_COMMON_DIR",
            "CANARY_CONFIG",
            "CANARY_CACHE",
            "CANARY_STATE",
            "CANARY_RESOLUTION_WARNING",
            "CANARY_APP_WARNING",
            "CANARY_REPAIR_GUIDANCE",
            "CANARY_PROFILE_DESCRIPTION",
            "https://",
        ] {
            assert!(!output.contains(canary), "leaked {canary}: {output}");
        }
        assert!(output.contains("github.example.test"));
        assert!(output.contains("alice@example.test"));
        assert!(output.contains("acme"));
        assert!(output.contains("widget"));
    }
}

#[test]
fn rule_resolution_preserves_source_metadata() {
    let projected = AgentContext::from_app(&context(
        ResolutionSource::Rule {
            id: "company-rule".into(),
            priority: 80,
        },
        Some("work"),
    ));

    assert_eq!(projected.selection.state, SelectionState::Resolved);
    assert_eq!(projected.selection.source.kind, SelectionSourceKind::Rule);
    assert_eq!(
        projected.selection.source.rule_id.as_deref(),
        Some("company-rule")
    );
    assert_eq!(projected.selection.source.priority, Some(80));
    projected.require_resolved().expect("resolved selection");

    let unresolved = AgentContext::from_app(&context(ResolutionSource::Unresolved, None));
    assert_eq!(unresolved.selection.state, SelectionState::Unresolved);
    assert_eq!(
        unresolved.selection.source.kind,
        SelectionSourceKind::Unresolved
    );
    assert!(unresolved.require_resolved().is_err());
}

#[test]
fn require_resolved_checks_selection_state_not_repository_presence() {
    let mut app = context(ResolutionSource::Default, Some("work"));
    app.repository = None;
    app.remote = None;
    let projected = AgentContext::from_app(&app);
    assert_eq!(projected.selection.state, SelectionState::Resolved);
    projected
        .require_resolved()
        .expect("default may resolve outside a repo");

    let unresolved = AgentContext::from_app(&context(ResolutionSource::Ambiguous, None));
    assert_eq!(unresolved.selection.state, SelectionState::Ambiguous);
    assert!(unresolved.require_resolved().is_err());
}

#[test]
fn default_profile_context_explains_fallback_scope() {
    let projected = AgentContext::from_app(&context(ResolutionSource::Default, Some("work")));
    let rendered = projected.render_codex();

    assert!(rendered.contains(
        "context note: the selected profile comes from ghis default_profile, not a repository binding; after entering a repository, use the repository-specific ghis context."
    ));

    let bound = AgentContext::from_app(&context(ResolutionSource::RepositoryBinding, Some("work")));
    assert!(!bound.render_codex().contains("context note:"));
}
