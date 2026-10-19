#![cfg(unix)]

use assert_cmd::Command as AssertCommand;
use predicates::prelude::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const GHIS_CONTROL_ENV: [&str; 14] = [
    "GHIS_CONFIG",
    "GHIS_PROFILE",
    "GHIS_BANNER_SHOWN",
    "GHIS_WRAPPER_ACTIVE",
    "GHIS_AGENT_WRAPPER_ACTIVE",
    "GHIS_AGENT_REAL_PATH",
    "GHIS_BYPASS",
    "GHIS_DISABLE_CHPWD",
    "GHIS_CHPWD_ENABLED",
    "GHIS_REPO_PROFILE",
    "GHIS_REPO_PROFILE_DISPLAY",
    "GHIS_REPO_ROOT",
    "GHIS_SHELL_INTEGRATION",
    "GHIS_SHELL_INTEGRATION_HEALTH",
];

const CONFIG: &str = r#"version = 1

[behavior]
default_profile = "personal"
credential_routing = "current-profile"

[profiles.personal]
host = "github.com"
login = "alice"
git_name = "Alice"
git_email = "alice@example.test"

[profiles.flowecho]
host = "git.example.test"
login = "flowecho"
git_name = "FlowEcho"
git_email = "flowecho@example.test"
credential_mode = "command"
credential_command = ["printf", "test-token"]

[[rules]]
id = "gitlab-flowecho"
profile = "flowecho"
priority = 100
host = "git.example.test"
"#;

fn xdg_environment(temp: &TempDir) -> [(String, PathBuf); 4] {
    let root = fs::canonicalize(temp.path()).unwrap_or_else(|_| temp.path().to_owned());
    [
        ("HOME".into(), root.join("home")),
        ("XDG_CONFIG_HOME".into(), root.join("config")),
        ("XDG_CACHE_HOME".into(), root.join("cache")),
        ("XDG_STATE_HOME".into(), root.join("state")),
    ]
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write executable");
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("permissions");
}

fn prepend_path(directory: &Path) -> String {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(
        std::iter::once(directory.to_path_buf()).chain(std::env::split_paths(&inherited)),
    )
    .expect("PATH")
    .to_string_lossy()
    .into_owned()
}

fn real_git() -> String {
    let output = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("locate git");
    assert!(output.status.success(), "git must be installed");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn write_config(temp: &TempDir, config: &str) {
    let directory = temp.path().join("config/ghis");
    fs::create_dir_all(&directory).expect("config directory");
    fs::write(directory.join("config.toml"), config).expect("config");
}

/// A fake git that answers the insteadOf probe through real Git, records any
/// `clone` / `ls-remote` invocation, and hands everything else to real Git.
fn fake_git_bin(temp: &TempDir) -> PathBuf {
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).expect("fake bin");
    write_executable(
        &bin.join("git"),
        r#"#!/bin/sh
for argument in "$@"; do
  if [ "$argument" = "--get-url" ]; then
    exec "$GHIS_TEST_REAL_GIT" "$@"
  fi
done
for argument in "$@"; do
  if [ "$argument" = "clone" ] || [ "$argument" = "ls-remote" ]; then
    printf '%s\n' "$*" >> "$FAKE_GIT_TRACE"
    printf 'GHIS_PROFILE=%s\n' "${GHIS_PROFILE-}" >> "$FAKE_GIT_TRACE"
    exit 0
  fi
done
exec "$GHIS_TEST_REAL_GIT" "$@"
"#,
    );
    bin
}

fn ghis(temp: &TempDir, bin: &Path) -> AssertCommand {
    let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    for key in GHIS_CONTROL_ENV {
        command.env_remove(key);
    }
    for (key, value) in xdg_environment(temp) {
        command.env(key, value);
    }
    command
        .env("PATH", prepend_path(bin))
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env("GHIS_TEST_REAL_GIT", real_git())
        .env("FAKE_GIT_TRACE", trace_path(temp));
    command
}

fn trace_path(temp: &TempDir) -> PathBuf {
    temp.path().join("git-trace.log")
}

fn trace(temp: &TempDir) -> String {
    fs::read_to_string(trace_path(temp)).unwrap_or_default()
}

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(real_git());
    command.current_dir(dir).args(args);
    for key in GHIS_CONTROL_ENV {
        command.env_remove(key);
    }
    command
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run git")
}

fn init_repository(root: &Path, remote: &str) {
    fs::create_dir_all(root).expect("repository directory");
    let init = git(root, &["init", "-q"]);
    assert!(init.status.success(), "git init failed");
    let add = git(root, &["remote", "add", "origin", remote]);
    assert!(add.status.success(), "git remote add failed");
}

#[test]
fn clone_outside_a_repository_selects_the_profile_from_the_url() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_config(&temp, CONFIG);
    let bin = fake_git_bin(&temp);
    let work = temp.path().join("work");
    fs::create_dir_all(&work).expect("work directory");

    ghis(&temp, &bin)
        .current_dir(&work)
        .args(["git", "--", "clone", "git@git.example.test:acme/tool.git"])
        .assert()
        .success();

    let trace = trace(&temp);
    assert!(trace.contains("GHIS_PROFILE=flowecho"), "{trace}");
    assert!(
        trace.contains("credential.https://git.example.test.helper"),
        "{trace}"
    );
}

#[test]
fn clone_inside_a_bound_repository_keeps_the_binding_untouched() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_config(&temp, CONFIG);
    let bin = fake_git_bin(&temp);
    let outer = temp.path().join("outer");
    init_repository(&outer, "https://github.com/alice/outer.git");
    let bind = git(&outer, &["config", "ghis.profile", "personal"]);
    assert!(bind.status.success(), "binding failed");

    ghis(&temp, &bin)
        .current_dir(&outer)
        .args([
            "git",
            "--",
            "clone",
            "https://git.example.test/acme/tool.git",
        ])
        .assert()
        .success();

    let trace = trace(&temp);
    assert!(trace.contains("GHIS_PROFILE=flowecho"), "{trace}");
    assert!(
        trace.contains("credential.https://git.example.test.helper"),
        "{trace}"
    );
    let binding = git(&outer, &["config", "--get", "ghis.profile"]);
    assert!(binding.status.success());
    assert_eq!(String::from_utf8_lossy(&binding.stdout).trim(), "personal");
}

#[test]
fn clone_inside_an_unbound_repository_does_not_auto_bind_it() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_config(&temp, CONFIG);
    let bin = fake_git_bin(&temp);
    let outer = temp.path().join("outer");
    init_repository(&outer, "https://git.example.test/acme/outer.git");

    ghis(&temp, &bin)
        .current_dir(&outer)
        .args([
            "git",
            "--",
            "clone",
            "https://git.example.test/acme/tool.git",
        ])
        .assert()
        .success();

    let trace = trace(&temp);
    assert!(trace.contains("GHIS_PROFILE=flowecho"), "{trace}");
    let binding = git(&outer, &["config", "--get", "ghis.profile"]);
    assert_eq!(
        binding.status.code(),
        Some(1),
        "the enclosing repository must stay unbound: {}",
        String::from_utf8_lossy(&binding.stderr)
    );
}

#[test]
fn ls_remote_uses_the_url_only_outside_a_repository() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_config(&temp, CONFIG);
    let bin = fake_git_bin(&temp);
    let work = temp.path().join("work");
    fs::create_dir_all(&work).expect("work directory");

    ghis(&temp, &bin)
        .current_dir(&work)
        .args([
            "git",
            "--",
            "ls-remote",
            "https://git.example.test/acme/tool.git",
        ])
        .assert()
        .success();
    let outside_trace = trace(&temp);
    assert!(
        outside_trace.contains("GHIS_PROFILE=flowecho"),
        "{outside_trace}"
    );

    let outer = temp.path().join("outer");
    init_repository(&outer, "https://github.com/alice/outer.git");
    git(&outer, &["config", "ghis.profile", "personal"]);
    fs::remove_file(trace_path(&temp)).expect("clear trace");

    ghis(&temp, &bin)
        .current_dir(&outer)
        .args([
            "git",
            "--",
            "ls-remote",
            "https://git.example.test/acme/tool.git",
        ])
        .assert()
        .success();
    let bound_trace = trace(&temp);
    assert!(
        bound_trace.contains("credential.https://github.com.helper"),
        "{bound_trace}"
    );
    assert!(
        !bound_trace.contains("credential.https://git.example.test.helper"),
        "{bound_trace}"
    );
}

#[test]
fn explicit_profile_still_wins_over_the_url() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_config(&temp, CONFIG);
    let bin = fake_git_bin(&temp);
    let work = temp.path().join("work");
    fs::create_dir_all(&work).expect("work directory");

    ghis(&temp, &bin)
        .current_dir(&work)
        .args([
            "--profile",
            "personal",
            "git",
            "--",
            "clone",
            "https://git.example.test/acme/tool.git",
        ])
        .assert()
        .success();

    let trace = trace(&temp);
    assert!(trace.contains("GHIS_PROFILE=personal"), "{trace}");
    assert!(
        !trace.contains("credential.https://git.example.test.helper"),
        "{trace}"
    );
}

#[test]
fn unknown_options_leave_the_url_out_of_resolution() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_config(&temp, CONFIG);
    let bin = fake_git_bin(&temp);
    let work = temp.path().join("work");
    fs::create_dir_all(&work).expect("work directory");

    ghis(&temp, &bin)
        .current_dir(&work)
        .args([
            "git",
            "--",
            "clone",
            "--frobnicate",
            "https://git.example.test/acme/tool.git",
        ])
        .assert()
        .success();

    let trace = trace(&temp);
    assert!(trace.contains("GHIS_PROFILE=personal"), "{trace}");
    assert!(
        !trace.contains("credential.https://git.example.test.helper"),
        "{trace}"
    );
}

#[test]
fn instead_of_rewrites_apply_before_matching() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_config(&temp, CONFIG);
    let bin = fake_git_bin(&temp);
    let global = temp.path().join("gitconfig");
    fs::write(
        &global,
        "[url \"https://git.example.test/\"]\n\tinsteadOf = gx:\n",
    )
    .expect("global config");
    let work = temp.path().join("work");
    fs::create_dir_all(&work).expect("work directory");

    ghis(&temp, &bin)
        .current_dir(&work)
        .env("GIT_CONFIG_GLOBAL", &global)
        .args(["git", "--", "clone", "gx:acme/tool.git"])
        .assert()
        .success();

    let trace = trace(&temp);
    assert!(trace.contains("GHIS_PROFILE=flowecho"), "{trace}");
    assert!(
        trace.contains("credential.https://git.example.test.helper"),
        "{trace}"
    );
}

#[test]
fn an_unmatched_url_does_not_apply_the_default_profile() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_config(&temp, CONFIG);
    let bin = fake_git_bin(&temp);
    let work = temp.path().join("work");
    fs::create_dir_all(&work).expect("work directory");

    ghis(&temp, &bin)
        .current_dir(&work)
        .args([
            "git",
            "--",
            "clone",
            "https://unknown.example.test/acme/tool.git",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("目标仓库 URL 未匹配任何 Profile"));

    let trace = trace(&temp);
    assert!(!trace.contains("GHIS_PROFILE=personal"), "{trace}");
    assert!(!trace.contains("credential."), "{trace}");
}

#[test]
fn unresolved_fail_stops_a_clone_with_an_unmatched_url() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_config(
        &temp,
        &CONFIG.replace(
            "default_profile = \"personal\"",
            "default_profile = \"personal\"\nunresolved = \"fail\"",
        ),
    );
    let bin = fake_git_bin(&temp);
    let work = temp.path().join("work");
    fs::create_dir_all(&work).expect("work directory");

    ghis(&temp, &bin)
        .current_dir(&work)
        .args([
            "git",
            "--",
            "clone",
            "https://unknown.example.test/acme/tool.git",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unresolved=fail"));

    assert!(trace(&temp).is_empty(), "clone must not run");
}

#[test]
fn command_line_rewrites_disable_url_resolution() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_config(&temp, CONFIG);
    let bin = fake_git_bin(&temp);
    let work = temp.path().join("work");
    fs::create_dir_all(&work).expect("work directory");

    ghis(&temp, &bin)
        .current_dir(&work)
        .args([
            "git",
            "--",
            "-c",
            "url.https://git.example.test/.insteadOf=gx:",
            "clone",
            "gx:acme/tool.git",
        ])
        .assert()
        .success();

    let trace = trace(&temp);
    assert!(trace.contains("GHIS_PROFILE=personal"), "{trace}");
    assert!(
        !trace.contains("credential.https://git.example.test.helper"),
        "{trace}"
    );
}
