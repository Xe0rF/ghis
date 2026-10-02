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
            transport: ghis::config::SigningTransport::LocalAgent,
            signing_key: Some("CANARY_SIGNING_KEY".into()),
            fingerprint: None,
            program: Some(PathBuf::from("CANARY_SIGNING_PROGRAM")),
        },
        credential_mode: Some(ghis::config::CredentialMode::Passthrough),
        credential_command: Some(vec!["CANARY_CREDENTIAL_COMMAND".into()]),
        credential_username: Some("CANARY_CREDENTIAL_USERNAME".into()),
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

/// Build the same fixture with a profile host that is not already normalised.
/// `Profile.host` is only normalised when the CLI writes it, so a hand-edited
/// or imported configuration can still carry odd spellings.
fn context_with_profile_host(source: ResolutionSource, host: &str) -> AppContext {
    let mut app = context(source, Some("work"));
    app.profile.as_mut().expect("profile").host = host.into();
    app
}

#[test]
fn json_identity_uses_forge_neutral_field_names() {
    let projected = AgentContext::from_app(&context(ResolutionSource::Explicit, Some("work")));
    let json = projected.render_json().expect("json");

    assert!(
        json.contains(r#""host": "github.example.test""#),
        "missing neutral host key: {json}"
    );
    assert!(json.contains(r#""login": "alice""#), "{json}");
    for stale in ["github_host", "github_login"] {
        assert!(
            !json.contains(stale),
            "the GitHub-only key `{stale}` is still in the payload: {json}"
        );
    }
}

#[test]
fn schema_version_matches_the_constant_and_the_text_header() {
    // Pin the value, not just its self-consistency.  v2 renamed the identity
    // fields; v3 renamed the `GithubLogin` selection source, which reaches both
    // the JSON and the text output.
    assert_eq!(ghis::agent_context::AGENT_CONTEXT_SCHEMA_VERSION, 3);

    let projected = AgentContext::from_app(&context(ResolutionSource::Explicit, Some("work")));

    assert_eq!(
        projected.schema_version,
        ghis::agent_context::AGENT_CONTEXT_SCHEMA_VERSION
    );
    let json = projected.render_json().expect("json");
    assert!(
        json.contains(&format!(
            r#""schema_version": {json_version}"#,
            json_version = projected.schema_version
        )),
        "{json}"
    );

    // The header used to be a hardcoded "schema v1" literal that silently
    // desynced from the constant.  Pin them together.
    let header = projected.render_codex();
    assert!(
        header.starts_with(&format!(
            "ghis session context (schema v{version})",
            version = projected.schema_version
        )),
        "text header disagrees with schema_version: {header}"
    );
}

#[test]
fn reported_identity_host_is_normalised_like_cross_host_guards() {
    // ghis compares hosts through `github::normalize_host` everywhere it makes
    // an authorisation decision.  The agent context used to copy `Profile.host`
    // verbatim, so an agent could be told a host string that no guard would
    // ever match.
    let projected = AgentContext::from_app(&context_with_profile_host(
        ResolutionSource::Explicit,
        "Git.Example.Test.:443",
    ));
    let json = projected.render_json().expect("json");

    assert!(
        json.contains(r#""host": "git.example.test""#),
        "identity host was not normalised: {json}"
    );
    assert!(!json.contains("Git.Example.Test"), "{json}");
}

#[test]
fn contract_text_reflects_the_active_forge_rather_than_only_github() {
    let gitlab = AgentContext::from_app(&context_with_profile_host(
        ResolutionSource::Explicit,
        "Git.Example.Test.",
    ));
    let contract = gitlab.contract.join("\n");
    let lowered = contract.to_lowercase();

    // The agent has to be told which host it is on; otherwise a GitLab
    // repository reads "use gh" and the agent reaches for the wrong CLI.
    assert!(
        lowered.contains("the repository host is `git.example.test`"),
        "the contract must name the active host: {contract}"
    );
    assert!(
        !lowered.contains("use ordinary git and gh"),
        "no rule may name one CLI as universal: {contract}"
    );
    assert!(
        !contract.contains("gitlab") && !contract.contains("glab"),
        "the contract must not hard-code one forge: {contract}"
    );
    for rule in [
        "use ordinary git from path",
        "do not switch the selected account globally",
        "do not output or persist tokens",
    ] {
        assert!(lowered.contains(rule), "missing rule {rule}: {contract}");
    }

    // The contract lines reach the agent verbatim, so every format must carry
    // them.
    for output in [
        gitlab.render_json().expect("json"),
        gitlab.render_codex(),
        gitlab.render_claude(),
    ] {
        assert!(
            output
                .to_lowercase()
                .contains("the repository host is `git.example.test`"),
            "{output}"
        );
    }
}

/// The selection source used to be reported as `github_login` even though the
/// matcher is forge-agnostic: it compares the remote host and first path
/// segment against every Profile, so it fires on any forge.
#[test]
fn unique_remote_login_source_is_reported_without_a_forge_name() {
    let projected =
        AgentContext::from_app(&context(ResolutionSource::UniqueRemoteLogin, Some("work")));
    assert_eq!(
        projected.selection.source.kind,
        SelectionSourceKind::UniqueRemoteLogin
    );
    let json = projected.render_json().expect("json");
    assert!(json.contains(r#""kind": "unique_remote_login""#), "{json}");
    assert!(!json.contains("github_login"), "{json}");
    assert!(
        projected
            .render_codex()
            .contains("source=unique_remote_login"),
        "{}",
        projected.render_codex()
    );
}
