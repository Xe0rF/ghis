use ghis::agent::claude::{self, SETTINGS_MARKER};
use ghis::agent::{AgentKind, ContextProvider};
use serde_json::json;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

#[derive(Debug)]
struct Provider;
impl ContextProvider for Provider {
    type Error = std::io::Error;
    fn require_resolved(&self) -> Result<(), Self::Error> {
        Ok(())
    }
    fn render_for(&self, _: AgentKind) -> Result<String, Self::Error> {
        Ok("repo=example\nidentity=work".into())
    }
}

#[test]
fn hook_is_structured_and_does_not_echo_prompt() {
    let input = br#"{"hook_event_name":"UserPromptSubmit","prompt":"do not leak me","cwd":"/tmp"}"#;
    let output = claude::handle_hook(&Provider, &input[..]).unwrap();
    assert_eq!(
        output.hook_specific_output.hook_event_name,
        "UserPromptSubmit"
    );
    assert!(
        output
            .hook_specific_output
            .additional_context
            .contains("repo=example")
    );
    assert!(
        !serde_json::to_string(&output)
            .unwrap()
            .contains("do not leak me")
    );
}

#[test]
fn launcher_injects_context_without_hook_and_preserves_existing_argv() {
    let spec = claude::run_spec(
        &Provider,
        "claude",
        [
            "--model",
            "sonnet",
            "-C",
            "/repo",
            "--append-system-prompt",
            "user context",
        ],
        Path::new("/repo"),
    )
    .unwrap();
    let args = spec.arguments();
    assert_eq!(args[0], "--append-system-prompt");
    assert_eq!(args[1], "repo=example\nidentity=work");
    assert_eq!(args[2], "--model");
    assert_eq!(args[3], "sonnet");
    assert_eq!(args[4], "-C");
    assert_eq!(args[5], "/repo");
    assert_eq!(args[6], "--append-system-prompt");
    assert_eq!(args[7], "user context");
}

#[test]
fn settings_merge_preserves_unknown_and_uninstall_is_idempotent() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("settings.json");
    fs::write(&path, serde_json::to_vec_pretty(&json!({
        "unknown": {"keep": true},
        "hooks": {"SessionStart": [{"matcher":"x","hooks":[{"type":"command","command":"other"}]}]}
    })).unwrap()).unwrap();
    assert!(claude::setup_settings(&path, std::path::Path::new("/bin/ghis"), None).unwrap());
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(value["unknown"]["keep"], true);
    for event in ["SessionStart", "UserPromptSubmit", "SubagentStart"] {
        assert!(
            value["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["hooks"][0]["statusMessage"] == SETTINGS_MARKER)
        );
    }
    assert!(claude::uninstall_settings(&path).unwrap());
    assert!(!claude::uninstall_settings(&path).unwrap());
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(value["unknown"]["keep"].as_bool().unwrap());
    assert!(
        value["hooks"]["SessionStart"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["hooks"][0]["command"] == "other")
    );
}
