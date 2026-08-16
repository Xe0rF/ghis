#![cfg(unix)]

use assert_cmd::Command as AssertCommand;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write executable");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("make executable");
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

fn write_profile(root: &Path) -> PathBuf {
    let config = root.join("config/ghis/config.toml");
    fs::create_dir_all(config.parent().expect("config parent")).expect("config directory");
    fs::write(
        &config,
        r#"version = 1

[behavior]
default_profile = "work"

[profiles.work]
host = "github.com"
login = "worker"
git_name = "CI Worker"
git_email = "worker@example.test"
"#,
    )
    .expect("config");
    config
}

fn apply_roots(command: &mut AssertCommand, root: &Path) {
    for key in [
        "GHIS_CONFIG",
        "GHIS_PROFILE",
        "GHIS_BANNER_SHOWN",
        "GHIS_WRAPPER_ACTIVE",
        "GHIS_BYPASS",
        "GHIS_DISABLE_CHPWD",
        "GH_CONFIG_DIR",
    ] {
        command.env_remove(key);
    }
    command
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"));
}

fn apply_roots_to_command(command: &mut Command, root: &Path) {
    for key in [
        "GHIS_CONFIG",
        "GHIS_PROFILE",
        "GHIS_BANNER_SHOWN",
        "GHIS_WRAPPER_ACTIVE",
        "GHIS_BYPASS",
        "GHIS_DISABLE_CHPWD",
        "GH_CONFIG_DIR",
    ] {
        command.env_remove(key);
    }
    command
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"));
}

fn run_as_unprivileged_if_root(command: &mut Command, root: &Path, state: &Path) {
    if unsafe { libc::geteuid() } == 0 {
        fs::set_permissions(root, fs::Permissions::from_mode(0o755))
            .expect("make test root traversable");
        fs::create_dir_all(state).expect("state directory");
        fs::set_permissions(state, fs::Permissions::from_mode(0o777))
            .expect("make state directory writable for test user");
        command.uid(65_534).gid(65_534);
    }
}

#[test]
fn managed_ci_child_uses_isolated_roots_and_selected_credentials() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let root = temporary.path();
    write_profile(root);
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("fake bin");
    let trace = root.join("trace");
    let expected_home = root.join("home");

    write_executable(
        &bin.join("git"),
        r#"#!/bin/sh
set -eu
case "${1-} ${2-}" in
  "rev-parse --git-dir") printf '%s\n%s\n%s\n' .git .git false ;;
  "rev-parse --show-toplevel") printf '%s\n' "$PWD" ;;
  "config --get-regexp") : ;;
  "remote -v") : ;;
  "config --local") exit 1 ;;
  *)
    [ -z "${GH_TOKEN+x}" ] || exit 61
    [ -z "${GITHUB_TOKEN+x}" ] || exit 62
    printf 'git-managed\n' >> "$TRACE"
    ;;
esac
exit 0
"#,
    );
    write_executable(
        &bin.join("gh"),
        r#"#!/bin/sh
set -eu
if [ "$1 $2" = "auth token" ]; then
  [ "$*" = "auth token --hostname github.com --user worker" ] || exit 69
  [ "$HOME" = "$EXPECTED_HOME" ] || exit 63
  [ -z "${GH_TOKEN+x}" ] || exit 66
  [ -z "${GITHUB_TOKEN+x}" ] || exit 67
  [ -z "${GH_HOST+x}" ] || exit 68
  printf 'selected-worker-token\n'
  exit 0
fi
[ "$1 $2" = "api user" ] || exit 0
[ "$HOME" = "$EXPECTED_HOME" ] || exit 63
[ "$GH_TOKEN" = selected-worker-token ] || exit 70
[ "$GH_TOKEN" != default-account-token ] || exit 77
[ "$GH_TOKEN" != wrong-account-token ] || exit 78
[ -z "${GITHUB_TOKEN+x}" ] || exit 71
[ "$GH_HOST" = github.com ] || exit 72
[ "${CI-}" = true ] || exit 73
[ "${GITHUB_ACTIONS-}" = true ] || exit 74
printf 'gh-managed\n' >> "$TRACE"
"#,
    );
    write_executable(
        &bin.join("codex"),
        r#"#!/bin/sh
set -eu
[ -z "${GH_TOKEN+x}" ] || exit 75
[ -z "${GITHUB_TOKEN+x}" ] || exit 76
git status
"#,
    );

    let mut sync = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    sync.current_dir(root)
        .arg("sync")
        .env("PATH", prepend_path(&bin));
    apply_roots(&mut sync, root);
    sync.assert().success();
    assert!(root.join("config/ghis/fragments").is_dir());

    let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    command
        .current_dir(root)
        .args(["agent", "run", "codex", "--", "inspect"])
        .env("PATH", prepend_path(&bin))
        .env("TRACE", &trace)
        .env("EXPECTED_HOME", &expected_home)
        .env("CI", "true")
        .env("GITHUB_ACTIONS", "true")
        .env("GH_TOKEN", "default-account-token")
        .env("GITHUB_TOKEN", "wrong-account-token");
    apply_roots(&mut command, root);
    command.assert().success();

    let mut gh = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    gh.current_dir(root)
        .args(["gh", "--", "api", "user"])
        .env("PATH", prepend_path(&bin))
        .env("TRACE", &trace)
        .env("EXPECTED_HOME", &expected_home)
        .env("CI", "true")
        .env("GITHUB_ACTIONS", "true")
        .env("GH_TOKEN", "default-account-token")
        .env("GITHUB_TOKEN", "wrong-account-token");
    apply_roots(&mut gh, root);
    gh.assert().success();

    assert_eq!(
        fs::read_to_string(&trace).expect("managed command trace"),
        "git-managed\ngh-managed\n"
    );
    assert!(
        !root.join("home/.config/gh").exists(),
        "managed gh must not fall back to the home config directory"
    );
}

#[test]
fn read_only_config_can_use_independent_cache_and_state_roots() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let root = temporary.path();
    let config = write_profile(root);
    let config_contents = fs::read(&config).expect("initial config");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("fake bin");
    write_executable(
        &bin.join("git"),
        r#"#!/bin/sh
set -eu
case "${1-} ${2-}" in
  "rev-parse --git-dir") printf '%s\n%s\n%s\n' .git .git false ;;
  "rev-parse --show-toplevel") printf '%s\n' "$PWD" ;;
  "config --local") exit 1 ;;
  *) exit 0 ;;
esac
"#,
    );

    let config_dir = root.join("config/ghis");
    let config_root = root.join("config");
    fs::set_permissions(&config, fs::Permissions::from_mode(0o444))
        .expect("make config file read-only");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o555))
        .expect("make ghis config directory read-only");
    fs::set_permissions(&config_root, fs::Permissions::from_mode(0o555))
        .expect("make config root read-only");
    assert_eq!(
        fs::metadata(&config)
            .expect("config metadata")
            .permissions()
            .mode()
            & 0o222,
        0,
        "config file must have no write bits"
    );
    assert_eq!(
        fs::metadata(&config_dir)
            .expect("config directory metadata")
            .permissions()
            .mode()
            & 0o222,
        0,
        "ghis config directory must have no write bits"
    );

    let state = root.join("state");
    let mut unbind = Command::new(env!("CARGO_BIN_EXE_ghis"));
    unbind
        .current_dir(root)
        .arg("unbind")
        .env("PATH", prepend_path(&bin));
    apply_roots_to_command(&mut unbind, root);
    run_as_unprivileged_if_root(&mut unbind, root, &state);
    let output = unbind.output().expect("run ghis unbind");
    assert!(
        output.status.success(),
        "ghis unbind failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let audit_log = state.join("ghis/ghis.log");
    assert!(
        audit_log.is_file(),
        "unbind must write its audit event under XDG_STATE_HOME"
    );
    assert!(
        fs::read_to_string(&audit_log)
            .expect("read audit event")
            .contains("\"action\":\"unbind\""),
        "audit log must be a product state artifact"
    );
    assert_eq!(
        fs::read(&config).expect("config remains readable"),
        config_contents,
        "read-only config bytes must remain unchanged"
    );
}
