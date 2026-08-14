use ghis::agent::codex;
use ghis::agent::{AgentKind, ContextProvider};
use std::ffi::{OsStr, OsString};
use std::path::Path;

#[derive(Debug)]
struct Provider;
impl ContextProvider for Provider {
    type Error = std::io::Error;
    fn require_resolved(&self) -> Result<(), Self::Error> {
        Ok(())
    }
    fn render_for(&self, kind: AgentKind) -> Result<String, Self::Error> {
        assert_eq!(kind, AgentKind::Codex);
        Ok("selected identity: work".into())
    }
}

#[test]
fn context_is_developer_instruction_and_user_argv_is_not_concatenated() {
    let spec = codex::run_spec(
        &Provider,
        "codex",
        ["--model", "o3", "-C", "/repo", "exec", "review this"],
        Path::new("/repo"),
    )
    .unwrap();
    let args = spec.arguments();
    assert_eq!(args[0], OsStr::new("-c"));
    assert_eq!(
        args[1],
        OsStr::new("developer_instructions=selected identity: work")
    );
    assert_eq!(args[2], OsStr::new("--model"));
    assert_eq!(args[3], OsStr::new("o3"));
    assert_eq!(args[4], OsStr::new("-C"));
    assert_eq!(args[5], OsStr::new("/repo"));
    assert_eq!(args[6], OsStr::new("exec"));
    assert_eq!(args[7], OsStr::new("review this"));
    assert!(!args.iter().any(|arg| arg.to_string_lossy().contains("mcp")));
    assert!(
        !args
            .iter()
            .any(|arg| arg.to_string_lossy().contains("skill"))
    );
}

#[test]
fn shell_path_is_explicitly_forwarded_to_codex_tool_environments() {
    let spec = codex::run_spec_with_shell_path(
        &Provider,
        "codex",
        ["exec", "status"],
        Path::new("/repo"),
        OsStr::new("/tmp/ghis agent-shims:/usr/bin:/bin"),
    )
    .unwrap();

    assert_eq!(
        spec.arguments(),
        &[
            OsString::from("-c"),
            OsString::from("developer_instructions=selected identity: work"),
            OsString::from("-c"),
            OsString::from(
                "shell_environment_policy.set.PATH=\"/tmp/ghis agent-shims:/usr/bin:/bin\"",
            ),
            OsString::from("exec"),
            OsString::from("status"),
        ]
    );
}
