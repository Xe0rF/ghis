#![cfg(unix)]

use assert_cmd::Command as AssertCommand;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write executable");
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("permissions");
}

#[test]
fn doctor_checks_credential_agent_key_and_signing_program_without_leaking_token() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let fake_bin = temp.path().join("bin");
    let config_home = temp.path().join("config");
    let cache_home = temp.path().join("cache");
    let state_home = temp.path().join("state");
    fs::create_dir_all(&fake_bin).expect("fake bin");
    fs::create_dir_all(config_home.join("ghis")).expect("config directory");

    let public_key = temp.path().join("work key.pub");
    fs::write(&public_key, "ssh-ed25519 AAAATEST work\n").expect("public key");
    let agent_socket = temp.path().join("1password agent.sock");
    fs::write(&agent_socket, "socket placeholder").expect("socket placeholder");
    let signing_program = fake_bin.join("op-ssh-sign");
    executable(&signing_program, "#!/bin/sh\nexit 0\n");
    executable(
        &fake_bin.join("gh"),
        r#"#!/bin/sh
case "$1 $2" in
  "auth status")
    printf '%s\n' '{"hosts":{"github.com":[{"host":"github.com","login":"worker","state":"success","active":true}]}}'
    ;;
  "auth token")
    printf '%s\n' 'doctor-secret-token'
    ;;
  *) exit 64 ;;
esac
"#,
    );
    executable(
        &fake_bin.join("ssh-add"),
        &format!(
            "#!/bin/sh\n[ \"$SSH_AUTH_SOCK\" = '{}' ] || exit 65\nprintf '%s\\n' 'ssh-ed25519 AAAATEST work'\n",
            agent_socket.display()
        ),
    );
    executable(
        &fake_bin.join("ssh-keygen"),
        "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '256 SHA256:work work (ED25519)'\n",
    );

    let config_file = config_home.join("ghis/config.toml");
    fs::write(
        &config_file,
        format!(
            r#"version = 1

[profiles.work]
host = "github.com"
login = "worker"
git_name = "Work Identity"
git_email = "work@example.test"

[profiles.work.ssh]
mode = "one-password"
public_key = {public_key:?}
fingerprint = "SHA256:work"
agent_socket = {agent_socket:?}

[profiles.work.signing]
enabled = true
signing_key = {public_key:?}
program = {signing_program:?}
"#,
            public_key = public_key,
            agent_socket = agent_socket,
            signing_program = signing_program,
        ),
    )
    .expect("config");

    let inherited_path = std::env::var_os("PATH").expect("PATH");
    let path = std::env::join_paths(
        std::iter::once(fake_bin.clone()).chain(std::env::split_paths(&inherited_path)),
    )
    .expect("joined PATH");
    let output = AssertCommand::cargo_bin("ghis")
        .expect("ghis binary")
        .args([
            "--config",
            config_file.to_str().expect("UTF-8 config path"),
            "--profile",
            "work",
            "doctor",
            "--json",
        ])
        .current_dir(temp.path())
        .env("PATH", path)
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("GH_TOKEN", "wrong-inherited-token")
        .output()
        .expect("run doctor");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("doctor JSON");
    assert_eq!(report["credential_available"], true);
    assert_eq!(report["ssh_agent"]["source"], "1password");
    assert_eq!(report["ssh_agent"]["available"], true);
    assert_eq!(
        report["ssh_agent"]["selected_key_fingerprint"],
        "SHA256:work"
    );
    assert_eq!(report["signing_program"]["available"], true);
    assert_eq!(report["warnings"], serde_json::json!([]));

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!combined.contains("doctor-secret-token"));
    assert!(!combined.contains("wrong-inherited-token"));
}
