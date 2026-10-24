#![cfg(unix)]

use assert_cmd::Command as AssertCommand;
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

const BASE: &str = r#"version = 1

[behavior]
default_profile = "personal"

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

/// A fake git that records every invocation and answers `clone` / `ls-remote`
/// without touching the network.  Everything else goes to real Git.
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
printf '%s\n' "$*" >> "$FAKE_GIT_TRACE"
for argument in "$@"; do
  if [ "$argument" = "clone" ] || [ "$argument" = "ls-remote" ]; then
    printf 'GHIS_PROFILE=%s\n' "${GHIS_PROFILE-}" >> "$FAKE_GIT_TRACE"
    exit 0
  fi
done
exec "$GHIS_TEST_REAL_GIT" "$@"
"#,
    );
    bin
}

fn ghis(temp: &TempDir, bin: Option<&Path>) -> AssertCommand {
    let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    for key in GHIS_CONTROL_ENV {
        command.env_remove(key);
    }
    for (key, value) in xdg_environment(temp) {
        command.env(key, value);
    }
    command
        .env("PATH", prepend_path(bin.unwrap_or(temp.path())))
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

#[test]
fn a_unique_other_host_gets_the_same_helper_at_runtime() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_config(&temp, BASE);
    let bin = fake_git_bin(&temp);
    let work = temp.path().join("work");
    fs::create_dir_all(&work).expect("work directory");

    ghis(&temp, Some(&bin))
        .current_dir(&work)
        .args([
            "--profile",
            "personal",
            "git",
            "--",
            "clone",
            "https://github.com/acme/tool.git",
        ])
        .assert()
        .success();

    let trace = trace(&temp);
    assert!(
        trace.contains("credential.https://github.com.helper"),
        "{trace}"
    );
    assert!(
        trace.contains("credential.https://git.example.test.helper"),
        "{trace}"
    );
    assert!(trace.contains("GHIS_PROFILE=personal"), "{trace}");
}

#[test]
fn a_host_with_two_profiles_is_never_routed() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_config(
        &temp,
        &format!(
            "{BASE}\n[profiles.flowecho-ops]\nhost = \"git.example.test\"\nlogin = \"flowecho-ops\"\ngit_name = \"FlowEcho Ops\"\ngit_email = \"ops@example.test\"\n"
        ),
    );
    let bin = fake_git_bin(&temp);
    let work = temp.path().join("work");
    fs::create_dir_all(&work).expect("work directory");

    ghis(&temp, Some(&bin))
        .current_dir(&work)
        .args([
            "--profile",
            "personal",
            "git",
            "--",
            "clone",
            "https://github.com/acme/tool.git",
        ])
        .assert()
        .success();

    let trace = trace(&temp);
    assert!(
        !trace.contains("credential.https://git.example.test.helper"),
        "{trace}"
    );
}

#[test]
fn a_none_host_is_never_routed() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_config(
        &temp,
        &BASE.replace(
            "credential_mode = \"command\"\ncredential_command = [\"printf\", \"test-token\"]",
            "credential_mode = \"none\"",
        ),
    );
    let bin = fake_git_bin(&temp);
    let work = temp.path().join("work");
    fs::create_dir_all(&work).expect("work directory");

    ghis(&temp, Some(&bin))
        .current_dir(&work)
        .args([
            "--profile",
            "personal",
            "git",
            "--",
            "clone",
            "https://github.com/acme/tool.git",
        ])
        .assert()
        .success();

    assert!(
        !trace(&temp).contains("credential.https://git.example.test.helper"),
        "{}",
        trace(&temp)
    );
}

#[test]
fn current_profile_mode_disables_routing() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_config(
        &temp,
        &BASE.replace(
            "default_profile = \"personal\"",
            "default_profile = \"personal\"\ncredential_routing = \"current-profile\"",
        ),
    );
    let bin = fake_git_bin(&temp);
    let work = temp.path().join("work");
    fs::create_dir_all(&work).expect("work directory");

    ghis(&temp, Some(&bin))
        .current_dir(&work)
        .args([
            "--profile",
            "personal",
            "git",
            "--",
            "clone",
            "https://github.com/acme/tool.git",
        ])
        .assert()
        .success();

    let trace = trace(&temp);
    assert!(
        trace.contains("credential.https://github.com.helper"),
        "{trace}"
    );
    assert!(
        !trace.contains("credential.https://git.example.test.helper"),
        "{trace}"
    );
}

#[test]
fn a_command_line_helper_override_disables_routing_without_a_profile() {
    let temp = tempfile::tempdir().expect("temporary directory");
    // No resolved Profile: with one, every credential-helper override is
    // refused outright, so the routing rule is only reachable here.
    write_config(&temp, &BASE.replace("default_profile = \"personal\"\n", ""));
    let bin = fake_git_bin(&temp);
    let work = temp.path().join("work");
    fs::create_dir_all(&work).expect("work directory");

    ghis(&temp, Some(&bin))
        .current_dir(&work)
        .args([
            "git",
            "--",
            "-c",
            "credential.helper=store",
            "clone",
            "https://github.com/acme/tool.git",
        ])
        .assert()
        .success();

    let trace = trace(&temp);
    assert!(trace.contains("credential.helper=store"), "{trace}");
    assert!(
        !trace.contains("credential.https://git.example.test.helper"),
        "{trace}"
    );
}

#[test]
fn an_unresolved_profile_still_routes_unique_hosts() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_config(&temp, &BASE.replace("default_profile = \"personal\"\n", ""));
    let bin = fake_git_bin(&temp);
    let work = temp.path().join("work");
    fs::create_dir_all(&work).expect("work directory");

    ghis(&temp, Some(&bin))
        .current_dir(&work)
        .args([
            "git",
            "--",
            "clone",
            "https://unknown.example.test/acme/tool.git",
        ])
        .assert()
        .success();

    let trace = trace(&temp);
    assert!(
        trace.contains("credential.https://github.com.helper"),
        "{trace}"
    );
    assert!(
        trace.contains("credential.https://git.example.test.helper"),
        "{trace}"
    );
}

#[test]
fn the_helper_answers_a_routed_host_and_quits_for_shared_hosts() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_config(&temp, BASE);

    ghis(&temp, None)
        .env("GHIS_PROFILE", "personal")
        .args(["credential-helper", "get"])
        .write_stdin("protocol=https\nhost=git.example.test\n\n")
        .assert()
        .success()
        .stdout(predicates::str::contains("username=flowecho"))
        .stdout(predicates::str::contains("password=test-token"));

    write_config(
        &temp,
        &format!(
            "{BASE}\n[profiles.flowecho-ops]\nhost = \"git.example.test\"\nlogin = \"flowecho-ops\"\ngit_name = \"FlowEcho Ops\"\ngit_email = \"ops@example.test\"\ncredential_mode = \"command\"\ncredential_command = [\"printf\", \"ops-token\"]\n"
        ),
    );

    ghis(&temp, None)
        .env("GHIS_PROFILE", "personal")
        .args(["credential-helper", "get"])
        .write_stdin("protocol=https\nhost=git.example.test\n\n")
        .assert()
        .success()
        .stdout("quit=true\n\n");
}
