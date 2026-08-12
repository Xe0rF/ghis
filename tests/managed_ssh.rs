#![cfg(unix)]

use assert_cmd::Command as AssertCommand;
use predicates::prelude::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write executable");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("set executable mode");
}

fn prepend_path(directory: &Path) -> String {
    std::env::join_paths(
        std::iter::once(directory.to_path_buf()).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )),
    )
    .expect("join PATH")
    .to_string_lossy()
    .into_owned()
}

fn xdg_environment(root: &Path) -> [(String, PathBuf); 4] {
    [
        ("HOME".into(), root.join("home")),
        ("XDG_CONFIG_HOME".into(), root.join("xdg-config")),
        ("XDG_CACHE_HOME".into(), root.join("xdg-cache")),
        ("XDG_STATE_HOME".into(), root.join("xdg-state")),
    ]
}

fn apply_environment(command: &mut AssertCommand, root: &Path) {
    for key in [
        "GHIS_CONFIG",
        "GHIS_PROFILE",
        "GHIS_BANNER_SHOWN",
        "GHIS_WRAPPER_ACTIVE",
        "GHIS_BYPASS",
        "GHIS_DISABLE_CHPWD",
    ] {
        command.env_remove(key);
    }
    for (key, value) in xdg_environment(root) {
        command.env(key, value);
    }
}

#[test]
fn managed_ssh_push_overrides_inherited_git_ssh_environment() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let root = temporary.path();
    let fake_bin = root.join("fake-bin");
    let repository = root.join("repository");
    let identity_directory = root.join("managed identity");
    fs::create_dir_all(&fake_bin).expect("fake bin");
    fs::create_dir_all(&repository).expect("repository");
    fs::create_dir_all(&identity_directory).expect("identity directory");

    let public_key = identity_directory.join("work key.pub");
    let agent_socket = identity_directory.join("1password agent.sock");
    let ssh_trace = root.join("ssh.trace");
    let bad_ssh_trace = root.join("bad-ssh.trace");
    let bad_credential_trace = root.join("bad-credential.trace");
    fs::write(&public_key, "ssh-ed25519 AAAATEST work\n").expect("public key");

    executable(
        &fake_bin.join("ssh-add"),
        r#"#!/bin/sh
[ "$SSH_AUTH_SOCK" = "$EXPECTED_AGENT_SOCKET" ] || exit 65
printf '%s\n' 'ssh-ed25519 AAAATEST work'
"#,
    );
    executable(
        &fake_bin.join("ssh-keygen"),
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '256 SHA256:work work (ED25519)'
"#,
    );
    executable(
        &fake_bin.join("ssh"),
        r#"#!/bin/sh
{
  printf 'variant=%s\n' "${GIT_SSH_VARIANT-}"
  for argument do
    printf 'arg=%s\n' "$argument"
  done
} > "$SSH_TRACE"
exit 73
"#,
    );
    let bad_ssh = fake_bin.join("bad-ssh");
    executable(
        &bad_ssh,
        r#"#!/bin/sh
printf 'inherited SSH command ran\n' > "$BAD_SSH_TRACE"
exit 74
"#,
    );
    executable(
        &fake_bin.join("gh"),
        r#"#!/bin/sh
if [ "${1:-} ${2:-}" = "auth token" ]; then
  printf '%s\n' 'selected-token'
  exit 0
fi
if [ "${1:-} ${2:-}" = "repo clone" ]; then
  exec git ls-remote backup
fi
exit 64
"#,
    );
    let bad_credential = fake_bin.join("bad-credential");
    executable(
        &bad_credential,
        r#"#!/bin/sh
printf 'wrong helper ran\n' > "$BAD_CREDENTIAL_TRACE"
printf '%s\n' 'username=wrong-account' 'password=wrong-token' ''
"#,
    );

    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repository)
            .status()
            .expect("git init")
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "-c",
                "user.name=Setup",
                "-c",
                "user.email=setup@example.test",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ])
            .current_dir(&repository)
            .status()
            .expect("initial commit")
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/example/project.git",
            ])
            .current_dir(&repository)
            .status()
            .expect("add primary remote")
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "remote",
                "add",
                "backup",
                "git@github.com:example/project.git",
            ])
            .current_dir(&repository)
            .status()
            .expect("add SSH backup remote")
            .success()
    );

    let config = root.join("config.toml");
    fs::write(
        &config,
        format!(
            r#"version = 1

[behavior]
ssh_unmanaged = "fail"

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
"#,
        ),
    )
    .expect("config");

    let path = prepend_path(&fake_bin);
    let mut bind = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    bind.args([
        "--config",
        config.to_str().expect("UTF-8 config path"),
        "use",
        "work",
        "--repo",
        repository.to_str().expect("UTF-8 repository path"),
    ])
    .env("PATH", &path)
    .env("GIT_CONFIG_GLOBAL", "/dev/null");
    apply_environment(&mut bind, root);
    bind.assert().success();

    let inherited_command = format!("{} --wrong-identity", bad_ssh.display());
    let mut push = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    push.current_dir(&repository)
        .args([
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "git",
            "--",
            "push",
            "backup",
            "HEAD:refs/heads/main",
        ])
        .env("PATH", &path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("EXPECTED_AGENT_SOCKET", &agent_socket)
        .env("SSH_TRACE", &ssh_trace)
        .env("BAD_SSH_TRACE", &bad_ssh_trace)
        .env("GIT_SSH", &bad_ssh)
        .env("GIT_SSH_COMMAND", &inherited_command)
        .env("GIT_SSH_VARIANT", "plink");
    apply_environment(&mut push, root);
    push.assert().failure();

    assert!(ssh_trace.is_file(), "managed ssh command did not run");
    assert!(
        !bad_ssh_trace.exists(),
        "the inherited GIT_SSH_COMMAND bypassed the selected Profile"
    );
    let trace = fs::read_to_string(&ssh_trace).expect("read ssh trace");
    assert!(trace.lines().any(|line| line == "variant=ssh"), "{trace}");
    assert!(
        trace.lines().any(|line| line == "arg=IdentitiesOnly=yes"),
        "{trace}"
    );
    for expected in [
        "arg=-F",
        "arg=/dev/null",
        "arg=BatchMode=yes",
        "arg=ControlMaster=no",
        "arg=ControlPath=none",
    ] {
        assert!(trace.lines().any(|line| line == expected), "{trace}");
    }
    assert!(
        trace
            .lines()
            .any(|line| line == format!("arg=IdentityAgent={}", agent_socket.display())),
        "{trace}"
    );
    assert!(
        trace
            .lines()
            .any(|line| line == format!("arg={}", public_key.display())),
        "{trace}"
    );
    assert!(!trace.contains("wrong-identity"), "{trace}");

    fs::remove_file(&ssh_trace).expect("clear push trace");
    let mut gh_clone = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    gh_clone
        .current_dir(&repository)
        .args([
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "gh",
            "--",
            "repo",
            "clone",
            "example/project",
        ])
        .env("PATH", &path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("SSH_TRACE", &ssh_trace)
        .env("BAD_SSH_TRACE", &bad_ssh_trace)
        .env("GIT_SSH", &bad_ssh)
        .env("GIT_SSH_COMMAND", inherited_command)
        .env("GIT_SSH_VARIANT", "plink");
    apply_environment(&mut gh_clone, root);
    gh_clone.assert().failure();

    assert!(
        ssh_trace.is_file(),
        "a Git command started by gh did not receive managed SSH"
    );
    assert!(
        !bad_ssh_trace.exists(),
        "gh inherited the caller's SSH identity override"
    );
    let trace = fs::read_to_string(&ssh_trace).expect("read gh SSH trace");
    assert!(trace.lines().any(|line| line == "variant=ssh"), "{trace}");
    assert!(
        trace.lines().any(|line| line == "arg=IdentitiesOnly=yes"),
        "{trace}"
    );

    assert!(
        Command::new("git")
            .args([
                "config",
                "--worktree",
                "--unset-all",
                "credential.https://github.com.helper",
            ])
            .current_dir(&repository)
            .status()
            .expect("remove repository helper")
            .success()
    );
    let global_config = root.join("global.gitconfig");
    assert!(
        Command::new("git")
            .args(["config", "--file"])
            .arg(&global_config)
            .arg("credential.helper")
            .arg(format!("!{}", bad_credential.display()))
            .status()
            .expect("install wrong global helper")
            .success()
    );
    let mut explicit_override = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    explicit_override
        .current_dir(&repository)
        .arg("--config")
        .arg(config.to_str().expect("UTF-8 config path"))
        .args(["git", "--", "-c"])
        .arg(format!("credential.helper=!{}", bad_credential.display()))
        .args(["credential", "fill"])
        .write_stdin("protocol=https\nhost=github.com\n\n")
        .env("PATH", &path)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("BAD_CREDENTIAL_TRACE", &bad_credential_trace);
    apply_environment(&mut explicit_override, root);
    explicit_override
        .assert()
        .failure()
        .stderr(predicates::str::contains("credential helper 覆盖"));
    assert!(
        !bad_credential_trace.exists(),
        "an explicit helper ran before ghis rejected it"
    );

    let mut credential = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    credential
        .current_dir(&repository)
        .arg("--config")
        .arg(config.to_str().expect("UTF-8 config path"))
        .args(["git", "--", "credential", "fill"])
        .write_stdin("protocol=https\nhost=github.com\n\n")
        .env("PATH", &path)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("BAD_CREDENTIAL_TRACE", &bad_credential_trace);
    apply_environment(&mut credential, root);
    credential
        .assert()
        .success()
        .stdout(predicates::str::contains("username=worker"))
        .stdout(predicates::str::contains("password=selected-token"))
        .stdout(predicates::str::contains("wrong-token").not());
    assert!(
        !bad_credential_trace.exists(),
        "a caller or global helper replaced the selected Profile"
    );

    fs::write(
        &config,
        format!(
            r#"version = 1

[behavior]
ssh_unmanaged = "fail"

[profiles.work]
host = "github.com"
login = "worker"
git_name = "Work Identity"
git_email = "work@example.test"

[profiles.work.ssh]
mode = "one-password"
public_key = {public_key:?}
fingerprint = "SHA256:work"
"#,
        ),
    )
    .expect("config without an available Agent socket");

    let mut remote_update = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    remote_update
        .current_dir(&repository)
        .args([
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "git",
            "--",
            "remote",
            "update",
            "backup",
        ])
        .env("PATH", &path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("BAD_SSH_TRACE", &bad_ssh_trace)
        .env("GIT_SSH", &bad_ssh)
        .env(
            "GIT_SSH_COMMAND",
            format!("{} --wrong-identity", bad_ssh.display()),
        )
        .env("GIT_SSH_VARIANT", "plink")
        .env_remove("SSH_AUTH_SOCK");
    apply_environment(&mut remote_update, root);
    remote_update.assert().failure();
    assert!(
        !bad_ssh_trace.exists(),
        "an unclassified remote command inherited the caller's SSH identity"
    );
}
