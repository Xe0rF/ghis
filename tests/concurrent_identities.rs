#![cfg(unix)]

use assert_cmd::Command as AssertCommand;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn path_with(directory: &Path) -> String {
    std::env::join_paths(
        std::iter::once(directory.to_path_buf()).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )),
    )
    .unwrap()
    .to_string_lossy()
    .into_owned()
}

fn environment(root: &Path, bin: &Path) -> Vec<(String, PathBuf)> {
    vec![
        ("HOME".into(), root.join("home")),
        ("XDG_CONFIG_HOME".into(), root.join("xdg-config")),
        ("XDG_CACHE_HOME".into(), root.join("xdg-cache")),
        ("XDG_STATE_HOME".into(), root.join("xdg-state")),
        ("PATH".into(), PathBuf::from(path_with(bin))),
        ("GIT_CONFIG_GLOBAL".into(), PathBuf::from("/dev/null")),
    ]
}

fn apply_environment(command: &mut Command, values: &[(String, PathBuf)]) {
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
    command.envs(values.iter().map(|(key, value)| (key, value)));
}

fn bind(root: &Path, bin: &Path, config: &Path, profile: &str, repository: &Path) {
    let mut command = AssertCommand::cargo_bin("ghis").unwrap();
    command
        .args([
            "--config",
            config.to_str().unwrap(),
            "use",
            profile,
            "--repo",
        ])
        .arg(repository)
        .env_remove("GHIS_CONFIG")
        .env_remove("GHIS_PROFILE")
        .env_remove("GHIS_BANNER_SHOWN")
        .env_remove("GHIS_WRAPPER_ACTIVE")
        .env_remove("GHIS_BYPASS")
        .env_remove("GHIS_DISABLE_CHPWD");
    for (key, value) in environment(root, bin) {
        command.env(key, value);
    }
    command.assert().success();
}

fn credential_fill(repository: &Path, environment: &[(String, PathBuf)], trace: &Path) -> Output {
    let mut command = Command::new("git");
    command
        .args(["credential", "fill"])
        .current_dir(repository)
        .env("FAKE_GH_TRACE", trace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_environment(&mut command, environment);
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"protocol=https\nhost=github.com\n\n")
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn concurrent_repositories_keep_commit_and_https_credentials_separate() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let trace = root.join("gh.trace");
    executable(
        &bin.join("gh"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_GH_TRACE"
case "$*" in
  "auth token --hostname github.com --user alice") printf 'token-for-alice\n' ;;
  "auth token --hostname github.com --user bob") printf 'token-for-bob\n' ;;
  *) exit 64 ;;
esac
"#,
    );

    let config = root.join("config.toml");
    fs::write(
        &config,
        r#"version = 1

[profiles.alice]
host = "github.com"
login = "alice"
git_name = "Alice Commit"
git_email = "alice@example.test"

[profiles.bob]
host = "github.com"
login = "bob"
git_name = "Bob Commit"
git_email = "bob@example.test"
"#,
    )
    .unwrap();

    let alice_repo = root.join("alice-repo");
    let bob_repo = root.join("bob-repo");
    for repository in [&alice_repo, &bob_repo] {
        fs::create_dir_all(repository).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(repository)
                .status()
                .unwrap()
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
                .current_dir(repository)
                .status()
                .unwrap()
                .success()
        );
    }
    bind(root, &bin, &config, "alice", &alice_repo);
    bind(root, &bin, &config, "bob", &bob_repo);

    let mut values = environment(root, &bin);
    values.push(("GHIS_CONFIG".into(), config.clone()));
    let commit = |repository: PathBuf, message: &'static str, values: Vec<(String, PathBuf)>| {
        std::thread::spawn(move || {
            let mut command = Command::new(assert_cmd::cargo::cargo_bin!("ghis"));
            command
                .args(["git", "--", "commit", "--allow-empty", "-m", message])
                .current_dir(repository);
            apply_environment(&mut command, &values);
            command.output().unwrap()
        })
    };
    let alice_commit = commit(alice_repo.clone(), "alice", values.clone());
    let bob_commit = commit(bob_repo.clone(), "bob", values.clone());
    assert!(alice_commit.join().unwrap().status.success());
    assert!(bob_commit.join().unwrap().status.success());

    let author = |repository: &Path| {
        String::from_utf8(
            Command::new("git")
                .args(["log", "-1", "--format=%an <%ae>"])
                .current_dir(repository)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
    };
    assert_eq!(
        author(&alice_repo).trim(),
        "Alice Commit <alice@example.test>"
    );
    assert_eq!(author(&bob_repo).trim(), "Bob Commit <bob@example.test>");

    let alice_values = values.clone();
    let bob_values = values;
    let alice_path = alice_repo.clone();
    let bob_path = bob_repo.clone();
    let alice_trace = trace.clone();
    let bob_trace = trace.clone();
    let alice_credential =
        std::thread::spawn(move || credential_fill(&alice_path, &alice_values, &alice_trace));
    let bob_credential =
        std::thread::spawn(move || credential_fill(&bob_path, &bob_values, &bob_trace));
    let alice_credential = alice_credential.join().unwrap();
    let bob_credential = bob_credential.join().unwrap();
    assert!(alice_credential.status.success());
    assert!(bob_credential.status.success());
    let alice_credential = String::from_utf8(alice_credential.stdout).unwrap();
    let bob_credential = String::from_utf8(bob_credential.stdout).unwrap();
    assert!(alice_credential.contains("username=alice"));
    assert!(alice_credential.contains("password=token-for-alice"));
    assert!(!alice_credential.contains("token-for-bob"));
    assert!(bob_credential.contains("username=bob"));
    assert!(bob_credential.contains("password=token-for-bob"));
    assert!(!bob_credential.contains("token-for-alice"));

    let trace = fs::read_to_string(trace).unwrap();
    assert!(trace.contains("auth token --hostname github.com --user alice"));
    assert!(trace.contains("auth token --hostname github.com --user bob"));
    assert!(!trace.contains("auth switch"));
    assert!(!trace.contains("token-for-alice"));
    assert!(!trace.contains("token-for-bob"));
}
