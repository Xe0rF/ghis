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

fn path_with(directory: &Path) -> String {
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
        ("XDG_CONFIG_HOME".into(), root.join("config")),
        ("XDG_CACHE_HOME".into(), root.join("cache")),
        ("XDG_STATE_HOME".into(), root.join("state")),
    ]
}

fn apply_environment(command: &mut AssertCommand, root: &Path) {
    for (key, value) in xdg_environment(root) {
        command.env(key, value);
    }
}

fn init_repository(root: &Path) -> PathBuf {
    let repository = root.join("repository");
    fs::create_dir_all(&repository).expect("repository");
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
    repository
}

fn write_config(root: &Path, ssh: &str, public_key: Option<&Path>) -> PathBuf {
    let directory = root.join("config/ghis");
    fs::create_dir_all(&directory).expect("config directory");
    let public_key = public_key
        .map(|path| format!("public_key = {path:?}\n"))
        .unwrap_or_default();
    let config = directory.join("config.toml");
    fs::write(
        &config,
        format!(
            r#"version = 1

[behavior]
default_profile = "work"
ssh_unmanaged = "fail"

[profiles.work]
host = "github.com"
login = "worker"
git_name = "Work Identity"
git_email = "work@example.test"

[profiles.work.ssh]
mode = "{ssh}"
{public_key}"#,
        ),
    )
    .expect("config");
    config
}

#[test]
fn dangerous_https_auth_sources_are_rejected_without_leaking_values() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let root = temporary.path();
    let repository = init_repository(root);
    let config = write_config(root, "external", None);
    let mut bind = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    bind.args([
        "--config",
        config.to_str().expect("UTF-8 config path"),
        "use",
        "work",
        "--repo",
        repository.to_str().expect("UTF-8 repository path"),
    ])
    .env("GIT_CONFIG_GLOBAL", "/dev/null");
    apply_environment(&mut bind, root);
    bind.assert().success();
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
            .expect("add remote")
            .success()
    );

    let mut helper_override = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    helper_override
        .current_dir(&repository)
        .args([
            "git",
            "--",
            "-c",
            "credential.helper=!wrong-helper",
            "fetch",
            "origin",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    apply_environment(&mut helper_override, root);
    helper_override
        .assert()
        .failure()
        .stderr(predicate::str::contains("credential helper 覆盖"));

    assert!(
        Command::new("git")
            .args(["config", "alias.deploy", "!sh -c 'git push origin'",])
            .current_dir(&repository)
            .status()
            .expect("configure shell alias")
            .success()
    );
    let mut shell_alias = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    shell_alias
        .current_dir(&repository)
        .args(["git", "--", "deploy"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    apply_environment(&mut shell_alias, root);
    shell_alias
        .assert()
        .failure()
        .stderr(predicate::str::contains("Git shell alias 无法安全解析"));

    assert!(
        Command::new("git")
            .args([
                "remote",
                "set-url",
                "origin",
                "https://wrong:super-secret@github.com/example/project.git",
            ])
            .current_dir(&repository)
            .status()
            .expect("set unsafe remote")
            .success()
    );
    let mut embedded_password = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    embedded_password
        .current_dir(&repository)
        .args(["git", "--", "fetch", "origin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    apply_environment(&mut embedded_password, root);
    embedded_password
        .assert()
        .failure()
        .stderr(predicate::str::contains("HTTPS URL 内嵌用户名和密码"))
        .stderr(predicate::str::contains("super-secret").not());

    assert!(
        Command::new("git")
            .args([
                "remote",
                "set-url",
                "origin",
                "https://github.com/example/project.git",
            ])
            .current_dir(&repository)
            .status()
            .expect("restore remote")
            .success()
    );
    let global_config = root.join("global.gitconfig");
    assert!(
        Command::new("git")
            .args(["config", "--file"])
            .arg(&global_config)
            .arg("http.https://github.com/.extraHeader")
            .arg("Authorization: Bearer top-secret")
            .status()
            .expect("write Authorization header")
            .success()
    );
    let mut header = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    header
        .current_dir(&repository)
        .args(["git", "--", "fetch", "origin"])
        .env("GIT_CONFIG_GLOBAL", &global_config);
    apply_environment(&mut header, root);
    header
        .assert()
        .failure()
        .stderr(predicate::str::contains("Authorization extraHeader"))
        .stderr(predicate::str::contains("top-secret").not());
}

#[test]
fn non_profile_https_host_keeps_its_existing_helper_chain() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let root = temporary.path();
    let repository = init_repository(root);
    let config = write_config(root, "external", None);
    let mut bind = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    bind.args([
        "--config",
        config.to_str().expect("UTF-8 config path"),
        "use",
        "work",
        "--repo",
        repository.to_str().expect("UTF-8 repository path"),
    ])
    .env("GIT_CONFIG_GLOBAL", "/dev/null");
    apply_environment(&mut bind, root);
    bind.assert().success();
    let fake_bin = root.join("fake-bin");
    fs::create_dir_all(&fake_bin).expect("fake bin");
    let helper_trace = root.join("helper.trace");
    let gh_trace = root.join("gh.trace");
    let helper = fake_bin.join("other-host-helper");
    executable(
        &helper,
        r#"#!/bin/sh
printf 'called\n' > "$HELPER_TRACE"
printf '%s\n' 'username=other-user' 'password=other-token' ''
"#,
    );
    executable(
        &fake_bin.join("gh"),
        r#"#!/bin/sh
printf 'called\n' > "$GH_TRACE"
exit 70
"#,
    );
    let global_config = root.join("global.gitconfig");
    assert!(
        Command::new("git")
            .args(["config", "--file"])
            .arg(&global_config)
            .arg("credential.helper")
            .arg(format!("!{}", helper.display()))
            .status()
            .expect("configure helper")
            .success()
    );

    let mut credential = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    credential
        .current_dir(&repository)
        .args(["git", "--", "credential", "fill"])
        .write_stdin("protocol=https\nhost=gitlab.example\n\n")
        .env("PATH", path_with(&fake_bin))
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("HELPER_TRACE", &helper_trace)
        .env("GH_TRACE", &gh_trace);
    apply_environment(&mut credential, root);
    credential
        .assert()
        .success()
        .stdout(predicate::str::contains("username=other-user"))
        .stdout(predicate::str::contains("password=other-token"));
    assert!(helper_trace.is_file(), "the other host helper did not run");
    assert!(!gh_trace.exists(), "the GitHub helper handled another host");
}

#[test]
fn ssh_policies_fail_closed_without_blocking_https() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let root = temporary.path();
    let repository = init_repository(root);
    let fake_bin = root.join("fake-bin");
    fs::create_dir_all(&fake_bin).expect("fake bin");
    let bad_ssh_trace = root.join("bad-ssh.trace");
    let bad_ssh = fake_bin.join("bad-ssh");
    executable(
        &bad_ssh,
        r#"#!/bin/sh
printf 'wrong SSH ran\n' > "$BAD_SSH_TRACE"
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
if [ "${1:-} ${2:-}" = "repo sync" ]; then
  exec git ls-remote backup
fi
exit 64
"#,
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
            .expect("add HTTPS remote")
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
            .expect("add SSH remote")
            .success()
    );
    write_config(root, "external", None);

    let inherited_ssh = format!("{} --wrong-identity", bad_ssh.display());
    let mut external_push = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    external_push
        .current_dir(&repository)
        .args(["git", "--", "push", "backup", "HEAD:refs/heads/main"])
        .env("PATH", path_with(&fake_bin))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_SSH_COMMAND", &inherited_ssh)
        .env("BAD_SSH_TRACE", &bad_ssh_trace);
    apply_environment(&mut external_push, root);
    external_push
        .assert()
        .failure()
        .stderr(predicate::str::contains("已阻止 SSH 操作"));
    assert!(!bad_ssh_trace.exists(), "inherited SSH command ran");

    let public_key = root.join("work.pub");
    fs::write(&public_key, "ssh-ed25519 AAAATEST work\n").expect("public key");
    write_config(root, "one-password", Some(&public_key));
    assert!(
        Command::new("git")
            .args([
                "remote",
                "set-url",
                "origin",
                "https://127.0.0.1:1/example/project.git",
            ])
            .current_dir(&repository)
            .status()
            .expect("set local HTTPS endpoint")
            .success()
    );

    let mut https = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    https
        .current_dir(&repository)
        .args(["git", "--", "ls-remote", "origin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("SSH_AUTH_SOCK")
        .env_remove("XDG_RUNTIME_DIR")
        .env("UID", "999999");
    apply_environment(&mut https, root);
    https
        .assert()
        .failure()
        .stderr(predicate::str::contains("没有可用的 SSH Agent").not())
        .stderr(predicate::str::contains("Failed to connect"));

    let mut gh_sync = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    gh_sync
        .current_dir(&repository)
        .args(["gh", "--", "repo", "sync"])
        .env("PATH", path_with(&fake_bin))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_SSH_COMMAND", inherited_ssh)
        .env("BAD_SSH_TRACE", &bad_ssh_trace)
        .env_remove("SSH_AUTH_SOCK")
        .env_remove("XDG_RUNTIME_DIR")
        .env("UID", "999999");
    apply_environment(&mut gh_sync, root);
    gh_sync
        .assert()
        .failure()
        .stderr(predicate::str::contains("已阻止 SSH 操作"));
    assert!(
        !bad_ssh_trace.exists(),
        "gh inherited the wrong SSH command"
    );
}

#[test]
fn rebind_and_unbind_remove_only_ghis_credential_helpers() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let root = temporary.path();
    let repository = init_repository(root);
    let config_directory = root.join("config/ghis");
    fs::create_dir_all(&config_directory).expect("config directory");
    let config = config_directory.join("config.toml");
    fs::write(
        &config,
        r#"version = 1

[profiles.personal]
host = "GitHub.com.:443"
login = "personal"
git_name = "Personal"
git_email = "personal@example.test"

[profiles.enterprise]
host = "git.enterprise.test:8443"
login = "worker"
git_name = "Work"
git_email = "work@example.test"
"#,
    )
    .expect("config");

    let bind = |profile: &str| {
        let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
        command
            .args([
                "--config",
                config.to_str().expect("UTF-8 config path"),
                "use",
                profile,
                "--repo",
                repository.to_str().expect("UTF-8 repository path"),
            ])
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
        apply_environment(&mut command, root);
        command.assert().success();
    };

    bind("personal");
    assert!(
        Command::new("git")
            .args([
                "config",
                "--worktree",
                "--add",
                "credential.https://github.com.helper",
                "!user-helper",
            ])
            .current_dir(&repository)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("add user helper")
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "config",
                "--worktree",
                "--add",
                "credential.https://github.com.helper",
                "",
            ])
            .current_dir(&repository)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("add user helper reset")
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "config",
                "--worktree",
                "--add",
                "credential.https://github.com.helper",
                "!user-helper-after-reset",
            ])
            .current_dir(&repository)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("add user helper after reset")
            .success()
    );

    bind("enterprise");
    let old_host = Command::new("git")
        .args([
            "config",
            "--worktree",
            "--get-all",
            "credential.https://github.com.helper",
        ])
        .current_dir(&repository)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("read old host helper");
    assert!(old_host.status.success());
    assert_eq!(
        String::from_utf8_lossy(&old_host.stdout),
        "!user-helper\n\n!user-helper-after-reset\n"
    );

    let enterprise_key = "credential.https://git.enterprise.test:8443.helper";
    let enterprise = Command::new("git")
        .args(["config", "--worktree", "--get-all", enterprise_key])
        .current_dir(&repository)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("read enterprise helper");
    assert!(enterprise.status.success());
    let enterprise = String::from_utf8_lossy(&enterprise.stdout);
    assert!(enterprise.starts_with('\n'));
    assert!(enterprise.contains("credential-helper"));

    // Simulate deleting the bound Profile before unbinding. Cleanup must not
    // depend on resolving that Profile from the current user configuration.
    fs::write(
        &config,
        r#"version = 1

[profiles.personal]
host = "github.com"
login = "personal"
git_name = "Personal"
git_email = "personal@example.test"
"#,
    )
    .expect("config without enterprise profile");
    let mut unbind = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    unbind
        .args([
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "unbind",
            "--repo",
            repository.to_str().expect("UTF-8 repository path"),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    apply_environment(&mut unbind, root);
    unbind.assert().success();

    let removed = Command::new("git")
        .args(["config", "--worktree", "--get-all", enterprise_key])
        .current_dir(&repository)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("read removed helper");
    assert_eq!(removed.status.code(), Some(1));
    let retained = Command::new("git")
        .args([
            "config",
            "--worktree",
            "--get-all",
            "credential.https://github.com.helper",
        ])
        .current_dir(&repository)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("read retained user helper");
    assert_eq!(
        String::from_utf8_lossy(&retained.stdout),
        "!user-helper\n\n!user-helper-after-reset\n"
    );
}
