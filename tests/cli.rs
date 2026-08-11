#![cfg(unix)]

use assert_cmd::Command as AssertCommand;
use ghis::config::{Config, SshMode};
use predicates::prelude::*;
use std::fs;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

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

fn xdg_environment(temp: &TempDir) -> [(String, PathBuf); 4] {
    [
        ("HOME".into(), temp.path().join("home")),
        ("XDG_CONFIG_HOME".into(), temp.path().join("config")),
        ("XDG_CACHE_HOME".into(), temp.path().join("cache")),
        ("XDG_STATE_HOME".into(), temp.path().join("state")),
    ]
}

fn write_default_profile(temp: &TempDir) {
    let directory = temp.path().join("config/ghis");
    fs::create_dir_all(&directory).expect("config directory");
    fs::write(
        directory.join("config.toml"),
        r#"version = 1

[behavior]
default_profile = "personal"

[profiles.personal]
host = "github.com"
login = "alice"
git_name = "Alice"
git_email = "alice@example.test"
"#,
    )
    .expect("config");
}

#[test]
fn gh_wrapper_selects_exact_account_and_clears_inherited_tokens() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_default_profile(&temp);
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).expect("bin directory");
    let trace = temp.path().join("trace");
    write_executable(
        &bin.join("gh"),
        r#"#!/bin/sh
if [ "$1" = auth ] && [ "$2" = token ]; then
  printf 'lookup:%s\n' "$*" >> "$FAKE_GH_TRACE"
  if [ -z "${GH_TOKEN+x}" ] && [ -z "${GITHUB_TOKEN+x}" ] && [ -z "${GH_HOST+x}" ]; then
    printf 'lookup-env-cleared\n' >> "$FAKE_GH_TRACE"
  fi
  printf 'selected-token\n'
  exit 0
fi
printf 'run:%s\n' "$*" >> "$FAKE_GH_TRACE"
if [ "$GH_TOKEN" = selected-token ] && [ "$GH_HOST" = github.com ]; then
  printf 'selected-account-ok\n'
else
  printf 'wrong-account\n'
  exit 41
fi
if [ -z "${GITHUB_TOKEN+x}" ]; then
  printf 'inherited-token-cleared\n'
fi
"#,
    );

    let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    command
        .current_dir(temp.path())
        .args(["gh", "--", "api", "user"])
        .env("PATH", prepend_path(&bin))
        .env("FAKE_GH_TRACE", &trace)
        .env("GH_TOKEN", "wrong-inherited-token")
        .env("GITHUB_TOKEN", "another-wrong-token");
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    command
        .assert()
        .success()
        .stdout(predicate::str::contains("selected-account-ok"))
        .stdout(predicate::str::contains("inherited-token-cleared"))
        .stdout(predicate::str::contains("selected-token").not());

    let trace = fs::read_to_string(trace).expect("trace");
    assert!(trace.contains("lookup:auth token --hostname github.com --user alice"));
    assert!(trace.contains("lookup-env-cleared"));
    assert!(trace.contains("run:api user"));
    assert!(!trace.contains("selected-token"));
    assert!(!trace.contains("wrong-inherited-token"));
}

#[test]
fn enterprise_gh_wrapper_uses_the_enterprise_token_variable() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let directory = temp.path().join("config/ghis");
    fs::create_dir_all(&directory).expect("config directory");
    fs::write(
        directory.join("config.toml"),
        r#"version = 1

[behavior]
default_profile = "enterprise"

[profiles.enterprise]
host = "git.company.test"
login = "alice"
git_name = "Alice Enterprise"
git_email = "alice@company.test"
"#,
    )
    .expect("enterprise config");
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).expect("bin directory");
    write_executable(
        &bin.join("gh"),
        r#"#!/bin/sh
if [ "$1" = auth ] && [ "$2" = token ]; then
  [ -z "${GH_TOKEN+x}" ] || exit 61
  [ -z "${GH_ENTERPRISE_TOKEN+x}" ] || exit 62
  printf 'enterprise-token\n'
  exit 0
fi
[ "$GH_HOST" = git.company.test ] || exit 63
[ "$GH_ENTERPRISE_TOKEN" = enterprise-token ] || exit 64
[ -z "${GH_TOKEN+x}" ] || exit 65
[ -z "${GITHUB_TOKEN+x}" ] || exit 66
[ -z "${GITHUB_ENTERPRISE_TOKEN+x}" ] || exit 67
printf 'enterprise-account-ok\n'
"#,
    );

    let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    command
        .current_dir(temp.path())
        .args(["gh", "--", "api", "user"])
        .env("PATH", prepend_path(&bin))
        .env("GH_TOKEN", "wrong-cloud-token")
        .env("GH_ENTERPRISE_TOKEN", "wrong-enterprise-token")
        .env("GITHUB_TOKEN", "wrong-github-token")
        .env("GITHUB_ENTERPRISE_TOKEN", "wrong-github-enterprise-token");
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    command
        .assert()
        .success()
        .stdout("enterprise-account-ok\n")
        .stdout(predicate::str::contains("enterprise-token").not());
}

#[test]
fn gh_wrapper_rejects_an_explicit_cross_host_target_before_token_lookup() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_default_profile(&temp);
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).expect("bin directory");
    let trace = temp.path().join("gh.trace");
    write_executable(
        &bin.join("gh"),
        "#!/bin/sh\nprintf 'called\\n' > \"$FAKE_GH_TRACE\"\nexit 70\n",
    );

    let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    command
        .current_dir(temp.path())
        .args(["gh", "--", "api", "--hostname", "other.example", "user"])
        .env("PATH", prepend_path(&bin))
        .env("FAKE_GH_TRACE", &trace);
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    command.assert().failure().stderr(predicate::str::contains(
        "与当前 Profile 主机 github.com 不一致",
    ));
    assert!(
        !trace.exists(),
        "gh ran before the target host was validated"
    );

    let mut positional = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    positional
        .current_dir(temp.path())
        .args([
            "gh",
            "--",
            "repo",
            "clone",
            "https://other.example/acme/project",
        ])
        .env("PATH", prepend_path(&bin))
        .env("FAKE_GH_TRACE", &trace);
    for (key, value) in xdg_environment(&temp) {
        positional.env(key, value);
    }
    positional
        .assert()
        .failure()
        .stderr(predicate::str::contains("仓库位置参数"));
    assert!(
        !trace.exists(),
        "gh ran before its repository URL was validated"
    );

    let mut item_url = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    item_url
        .current_dir(temp.path())
        .args([
            "gh",
            "--",
            "pr",
            "view",
            "--comments",
            "https://other.example/acme/project/pull/12",
        ])
        .env("PATH", prepend_path(&bin))
        .env("FAKE_GH_TRACE", &trace);
    for (key, value) in xdg_environment(&temp) {
        item_url.env(key, value);
    }
    item_url
        .assert()
        .failure()
        .stderr(predicate::str::contains("Pull Request URL"));
    assert!(
        !trace.exists(),
        "gh ran before its pull request URL was validated"
    );

    for args in [
        vec!["repo", "view", "--web", "other.example/acme/project"],
        vec!["pr", "revert", "https://other.example/acme/project/pull/12"],
        vec!["issue", "transfer", "1", "other.example/acme/destination"],
        vec![
            "repo",
            "sync",
            "acme/destination",
            "--source=other.example/acme/source",
        ],
    ] {
        let mut guarded = AssertCommand::cargo_bin("ghis").expect("ghis binary");
        guarded
            .current_dir(temp.path())
            .arg("gh")
            .arg("--")
            .args(args)
            .env("PATH", prepend_path(&bin))
            .env("FAKE_GH_TRACE", &trace);
        for (key, value) in xdg_environment(&temp) {
            guarded.env(key, value);
        }
        guarded.assert().failure();
        assert!(!trace.exists(), "gh ran before every target was validated");
    }

    let mut body_url = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    body_url
        .current_dir(temp.path())
        .args([
            "gh",
            "--",
            "pr",
            "comment",
            "1",
            "--body",
            "https://github.com/acme/other/pull/2",
        ])
        .env("PATH", prepend_path(&bin))
        .env("FAKE_GH_TRACE", &trace)
        .env("GH_REPO", "other.example/acme/project");
    for (key, value) in xdg_environment(&temp) {
        body_url.env(key, value);
    }
    body_url
        .assert()
        .failure()
        .stderr(predicate::str::contains("GH_REPO"));
    assert!(
        !trace.exists(),
        "a body URL must not hide the effective GH_REPO host"
    );
}

#[test]
fn gh_wrapper_rejects_ambiguous_and_cross_host_targets_before_token_lookup() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_default_profile(&temp);
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).expect("bin directory");
    let trace = temp.path().join("gh.trace");
    write_executable(
        &bin.join("gh"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$FAKE_GH_TRACE\"\nexit 70\n",
    );

    for args in [
        vec!["repo", "edit", "--template", "other.example/acme/project"],
        vec!["repo", "fork", "--remote", "other.example/acme/project"],
        vec![
            "repo",
            "view",
            "--unknown-option",
            "other.example/acme/project",
        ],
        vec![
            "issue",
            "edit",
            "1",
            "--add-blocked-by",
            "https://other.example/acme/project/issues/2",
        ],
        vec![
            "issue",
            "edit",
            "1",
            "--add-sub-issue",
            "https://github.com/acme/project/issues/2,https://other.example/acme/project/issues/3",
        ],
        vec!["issue", "view", "https://other.example/acme/project/pull/4"],
        vec![
            "gist",
            "clone",
            "https://gist.other.example/alice/0123456789abcdef",
        ],
        vec!["my-custom-alias"],
        vec!["my-custom-extension", "run"],
        vec!["extension", "exec", "my-custom-extension"],
    ] {
        let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
        command
            .current_dir(temp.path())
            .arg("gh")
            .arg("--")
            .args(args)
            .env("PATH", prepend_path(&bin))
            .env("FAKE_GH_TRACE", &trace)
            .env_remove("GH_REPO");
        for (key, value) in xdg_environment(&temp) {
            command.env(key, value);
        }
        command.assert().failure();
        assert!(!trace.exists(), "目标校验完成前不应查询 token 或执行 gh");
    }
}

#[test]
fn gh_wrapper_accepts_known_flags_without_misreading_content_as_targets() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_default_profile(&temp);
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).expect("bin directory");
    let trace = temp.path().join("gh.trace");
    write_executable(
        &bin.join("gh"),
        r#"#!/bin/sh
if [ "$1" = auth ] && [ "$2" = token ]; then
  printf 'lookup:%s\n' "$*" >> "$FAKE_GH_TRACE"
  printf 'selected-token\n'
  exit 0
fi
printf 'run:%s repo=%s\n' "$*" "${GH_REPO-<unset>}" >> "$FAKE_GH_TRACE"
[ "$GH_TOKEN" = selected-token ] || exit 71
[ "$GH_HOST" = github.com ] || exit 72
"#,
    );

    for args in [
        vec![
            "gist",
            "view",
            "--filename",
            "notes.txt",
            "https://gist.github.com/alice/0123456789abcdef",
        ],
        vec![
            "gist",
            "create",
            "--desc",
            "https://other.example/acme/project/pull/1",
            "notes.txt",
        ],
        vec!["browse", "--branch", "-Rgithub.com/not/a-selector"],
        vec!["repo", "edit", "--template", "github.com/acme/project"],
        vec!["repo", "fork", "--remote", "github.com/acme/project"],
        vec!["alias", "list"],
    ] {
        let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
        command
            .current_dir(temp.path())
            .arg("gh")
            .arg("--")
            .args(args)
            .env("PATH", prepend_path(&bin))
            .env("FAKE_GH_TRACE", &trace)
            .env_remove("GH_REPO");
        for (key, value) in xdg_environment(&temp) {
            command.env(key, value);
        }
        command.assert().success();
    }

    let mut body = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    body.current_dir(temp.path())
        .args([
            "gh",
            "--",
            "pr",
            "comment",
            "1",
            "--body",
            "https://other.example/acme/project/pull/2",
        ])
        .env("PATH", prepend_path(&bin))
        .env("FAKE_GH_TRACE", &trace)
        .env("GH_REPO", "github.com/acme/project");
    for (key, value) in xdg_environment(&temp) {
        body.env(key, value);
    }
    body.assert().success();

    let trace = fs::read_to_string(trace).expect("gh trace");
    assert_eq!(trace.matches("lookup:auth token").count(), 7);
    assert!(trace.contains(
        "run:pr comment 1 --body https://other.example/acme/project/pull/2 repo=github.com/acme/project"
    ));
    assert!(trace.contains("run:browse --branch -Rgithub.com/not/a-selector repo=<unset>"));
}

#[test]
fn explicit_gh_target_overrides_a_different_current_remote() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_default_profile(&temp);
    let repository = temp.path().join("repository");
    fs::create_dir_all(&repository).expect("repository directory");
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
                "remote",
                "add",
                "origin",
                "https://enterprise.example/acme/project.git",
            ])
            .current_dir(&repository)
            .status()
            .expect("git remote")
            .success()
    );

    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).expect("bin directory");
    let trace = temp.path().join("gh.trace");
    write_executable(
        &bin.join("gh"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_GH_TRACE"
if [ "$1" = auth ] && [ "$2" = token ]; then
  printf 'selected-token\n'
  exit 0
fi
[ "$GH_HOST" = github.com ] || exit 71
[ "$GH_TOKEN" = selected-token ] || exit 72
[ -z "${GH_REPO+x}" ] || exit 73
"#,
    );

    for args in [
        vec!["api", "--hostname=github.com", "user"],
        vec![
            "pr",
            "view",
            "--comments",
            "https://github.com/acme/project/pull/12",
        ],
    ] {
        let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
        command
            .current_dir(&repository)
            .arg("gh")
            .arg("--")
            .args(args)
            .env("PATH", prepend_path(&bin))
            .env("FAKE_GH_TRACE", &trace)
            .env("GH_REPO", "enterprise.example/acme/project");
        for (key, value) in xdg_environment(&temp) {
            command.env(key, value);
        }
        command.assert().success();
    }

    let trace = fs::read_to_string(trace).expect("gh trace");
    assert_eq!(
        trace
            .lines()
            .filter(|line| line.starts_with("auth token "))
            .count(),
        2
    );
    assert!(trace.contains("api --hostname=github.com user"));
    assert!(trace.contains("https://github.com/acme/project/pull/12"));
}

#[test]
fn gh_repository_context_uses_fetch_url_instead_of_pushurl() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_default_profile(&temp);
    let repository = temp.path().join("repository");
    fs::create_dir_all(&repository).expect("repository directory");
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
                "remote",
                "add",
                "origin",
                "https://github.com/acme/project.git",
            ])
            .current_dir(&repository)
            .status()
            .expect("git remote add")
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "remote",
                "set-url",
                "--push",
                "origin",
                "https://enterprise.example/acme/project.git",
            ])
            .current_dir(&repository)
            .status()
            .expect("git remote pushurl")
            .success()
    );

    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).expect("bin directory");
    let trace = temp.path().join("gh.trace");
    write_executable(
        &bin.join("gh"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_GH_TRACE"
if [ "$1" = auth ] && [ "$2" = token ]; then
  printf 'selected-token\n'
  exit 0
fi
[ "$GH_HOST" = github.com ] || exit 71
[ "$GH_TOKEN" = selected-token ] || exit 72
[ "$*" = "pr list" ] || exit 73
"#,
    );

    let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    command
        .current_dir(&repository)
        .args(["gh", "--", "pr", "list"])
        .env("PATH", prepend_path(&bin))
        .env("FAKE_GH_TRACE", &trace);
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    command.assert().success();

    let trace_contents = fs::read_to_string(&trace).expect("gh trace");
    assert!(trace_contents.contains("auth token --hostname github.com --user alice"));
    assert!(trace_contents.lines().any(|line| line == "pr list"));

    assert!(
        Command::new("git")
            .args([
                "remote",
                "set-url",
                "origin",
                "https://enterprise.example/acme/project.git",
            ])
            .current_dir(&repository)
            .status()
            .expect("replace fetch URL")
            .success()
    );
    fs::remove_file(&trace).expect("clear gh trace");
    let mut mismatched = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    mismatched
        .current_dir(&repository)
        .args(["gh", "--", "pr", "list"])
        .env("PATH", prepend_path(&bin))
        .env("FAKE_GH_TRACE", &trace);
    for (key, value) in xdg_environment(&temp) {
        mismatched.env(key, value);
    }
    mismatched
        .assert()
        .failure()
        .stderr(predicate::str::contains("当前仓库 remote"));
    assert!(!trace.exists(), "cross-host remote reached token lookup");
}

#[test]
fn relative_git_c_is_forwarded_only_once() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let repository = temp.path().join("repo");
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&repository)
            .status()
            .expect("git init")
            .success()
    );

    let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    command
        .current_dir(temp.path())
        .args(["git", "--", "-C", "repo", "status", "--short"]);
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    command.assert().success().stdout("");
}

#[test]
fn caller_identity_config_still_overrides_the_profile_fragment() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_default_profile(&temp);

    let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    command.current_dir(temp.path()).args([
        "git",
        "--",
        "-c",
        "user.name=Command Identity",
        "config",
        "user.name",
    ]);
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    command.assert().success().stdout("Command Identity\n");
}

#[test]
fn generated_zsh_wrapper_is_valid_and_preserves_git_exit_status() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let output = AssertCommand::cargo_bin("ghis")
        .expect("ghis binary")
        .args(["init", "zsh"])
        .output()
        .expect("generate init");
    assert!(output.status.success());
    let init = temp.path().join("init.zsh");
    fs::write(&init, output.stdout).expect("init script");

    assert!(
        Command::new("zsh")
            .args(["-n"])
            .arg(&init)
            .status()
            .expect("zsh syntax")
            .success()
    );

    let bin = assert_cmd::cargo::cargo_bin!("ghis");
    let binary_dir = bin.parent().expect("binary directory");
    let mut command = Command::new("zsh");
    command
        .args([
            "-f",
            "-c",
            "source \"$1\"; git definitely-not-a-command",
            "zsh",
        ])
        .arg(&init)
        .current_dir(temp.path())
        .env("PATH", prepend_path(binary_dir))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GHIS_CONFIG")
        .env_remove("GHIS_PROFILE");
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    let status = command.status().expect("run wrapper");
    assert_eq!(status.code(), Some(1));
}

#[test]
fn zsh_wrapper_preserves_tty_and_sigint_status() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let fake_bin = temp.path().join("fake-bin");
    fs::create_dir_all(&fake_bin).expect("fake bin");
    write_executable(
        &fake_bin.join("git"),
        r#"#!/bin/sh
if [ "${1:-}" = tty-probe ]; then
  if [ -t 0 ] && [ -t 1 ]; then
    printf 'tty-ok\n'
  else
    printf 'tty-lost\n'
    exit 72
  fi
  kill -INT "$$"
  exit 99
fi
exec "$GHIS_REAL_GIT" "$@"
"#,
    );
    let init = temp.path().join("init.zsh");
    fs::write(&init, ghis::shell::zsh_init_script("ghis")).expect("init");
    let probe = temp.path().join("probe.zsh");
    fs::write(
        &probe,
        format!(
            "#!/usr/bin/zsh -f\nsource {}\ngit tty-probe\n",
            ghis::shell::shell_quote(&init.to_string_lossy())
        ),
    )
    .expect("probe");
    let mut probe_permissions = fs::metadata(&probe).unwrap().permissions();
    probe_permissions.set_mode(0o755);
    fs::set_permissions(&probe, probe_permissions).unwrap();

    let ghis_bin = assert_cmd::cargo::cargo_bin!("ghis");
    let binary_dir = ghis_bin.parent().unwrap();
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        [fake_bin.as_path(), binary_dir]
            .into_iter()
            .map(Path::to_path_buf)
            .chain(std::env::split_paths(&inherited)),
    )
    .unwrap();
    let real_git = std::env::var_os("GHIS_TEST_REAL_GIT").unwrap_or_else(|| {
        std::env::split_paths(&inherited)
            .map(|directory| directory.join("git"))
            .find(|candidate| candidate.is_file())
            .expect("real git in PATH")
            .into_os_string()
    });
    let output = Command::new("script")
        .args(["-q", "-e", "-c"])
        .arg(&probe)
        .arg("/dev/null")
        .current_dir(temp.path())
        .env("PATH", path)
        .env("GHIS_REAL_GIT", real_git)
        .env("HOME", temp.path().join("home"))
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_CACHE_HOME", temp.path().join("cache"))
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .output()
        .expect("run wrapper in a pseudo-terminal");

    assert_eq!(output.status.code(), Some(130));
    assert!(String::from_utf8_lossy(&output.stdout).contains("tty-ok"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("tty-lost"));
}

#[test]
fn setup_respects_zdotdir_and_requires_explicit_noninteractive_consent() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let home = temp.path().join("home");
    let zdotdir = temp.path().join("zsh-config");
    fs::create_dir_all(&home).expect("home directory");
    fs::create_dir_all(&zdotdir).expect("ZDOTDIR");
    fs::write(zdotdir.join(".zshrc"), "export EXISTING=1\n").expect("zshrc");

    let configure = |command: &mut AssertCommand| {
        command
            .env("HOME", &home)
            .env("ZDOTDIR", &zdotdir)
            .write_stdin("");
        for (key, value) in xdg_environment(&temp) {
            command.env(key, value);
        }
    };

    let mut refused = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    refused.arg("setup");
    configure(&mut refused);
    refused
        .assert()
        .failure()
        .stderr(predicate::str::contains("ghis setup --yes"));

    let mut setup = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    setup.args(["setup", "--yes"]);
    configure(&mut setup);
    setup.assert().success().stdout(predicate::str::contains(
        zdotdir.join(".zshrc").display().to_string(),
    ));

    let contents = fs::read_to_string(zdotdir.join(".zshrc")).expect("configured zshrc");
    assert!(contents.contains(ghis::shell::START_MARKER));
    assert!(!home.join(".zshrc").exists());

    let mut second = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    second.args(["setup", "--yes"]);
    configure(&mut second);
    second
        .assert()
        .success()
        .stdout(predicate::str::contains("无需更新"));

    let mut uninstall = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    uninstall.arg("uninstall");
    configure(&mut uninstall);
    uninstall.assert().success();
    assert_eq!(
        fs::read_to_string(zdotdir.join(".zshrc")).expect("uninstalled zshrc"),
        "export EXISTING=1\n"
    );
}

#[test]
fn concurrent_profile_add_commands_keep_every_identity() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let binary = assert_cmd::cargo::cargo_bin!("ghis");
    let mut children = Vec::new();

    for index in 0..8 {
        let mut command = Command::new(binary);
        command
            .args([
                "profile",
                "add",
                &format!("profile-{index}"),
                "--login",
                &format!("user-{index}"),
                "--name",
                &format!("User {index}"),
                "--email",
                &format!("user-{index}@example.test"),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in xdg_environment(&temp) {
            command.env(key, value);
        }
        children.push(command.spawn().expect("spawn concurrent profile add"));
    }

    for child in children {
        let output = child.wait_with_output().expect("wait for profile add");
        assert!(
            output.status.success(),
            "profile add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let config = Config::load(temp.path().join("config/ghis/config.toml")).expect("load config");
    assert_eq!(config.profiles.len(), 8);
    for index in 0..8 {
        assert!(config.profiles.contains_key(&format!("profile-{index}")));
        assert!(
            temp.path()
                .join(format!("config/ghis/fragments/profile-{index}.gitconfig"))
                .is_file()
        );
    }
}

#[test]
fn completion_treats_a_closed_stdout_as_success() {
    let (closed_reader, writer) = UnixStream::pair().expect("unix stream pair");
    drop(closed_reader);
    let writer: OwnedFd = writer.into();

    let status = Command::new(assert_cmd::cargo::cargo_bin!("ghis"))
        .args(["completion", "zsh"])
        .stdout(Stdio::from(writer))
        .status()
        .expect("run completion with closed stdout");

    assert!(status.success());
}

#[test]
fn init_output_is_valid_when_piped_directly_to_zsh() {
    let status = Command::new("zsh")
        .args([
            "-f",
            "-c",
            "setopt pipefail; \"$GHIS_TEST_BIN\" init zsh | command zsh -n",
        ])
        .env("GHIS_TEST_BIN", assert_cmd::cargo::cargo_bin!("ghis"))
        .status()
        .expect("pipe init into zsh");

    assert!(status.success());
}

#[test]
fn git_wrapper_resolves_bound_profile_after_value_option_and_repeated_c() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let outer = temp.path().join("outer");
    let repository = outer.join("repo");
    fs::create_dir_all(&outer).expect("outer directory");
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&repository)
            .status()
            .expect("git init")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "--local", "ghis.profile", "work"])
            .current_dir(&repository)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("bind profile marker")
            .success()
    );

    let config_directory = temp.path().join("config/ghis");
    fs::create_dir_all(&config_directory).expect("config directory");
    fs::write(
        config_directory.join("config.toml"),
        r#"version = 1

[profiles.work]
host = "github.com"
login = "worker"
git_name = "Work Identity"
git_email = "work@example.test"
"#,
    )
    .expect("config");

    let init_output = AssertCommand::cargo_bin("ghis")
        .expect("ghis binary")
        .args(["init", "zsh"])
        .output()
        .expect("generate init");
    assert!(init_output.status.success());
    let init = temp.path().join("init.zsh");
    fs::write(&init, init_output.stdout).expect("init script");

    let binary = assert_cmd::cargo::cargo_bin!("ghis");
    let mut command = Command::new("zsh");
    command
        .args([
            "-f",
            "-c",
            "source \"$GHIS_TEST_INIT\"; git -c color.ui=false -C outer -C repo config user.name",
        ])
        .current_dir(temp.path())
        .env("GHIS_TEST_INIT", &init)
        .env(
            "PATH",
            prepend_path(binary.parent().expect("binary directory")),
        )
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    let output = command.output().expect("run wrapped git");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "Work Identity\n");
}

#[test]
fn deleted_repository_binding_never_falls_back_to_default_profile() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let repository = temp.path().join("repo");
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&repository)
            .status()
            .expect("git init")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "--local", "ghis.profile", "deleted"])
            .current_dir(&repository)
            .status()
            .expect("stale binding")
            .success()
    );
    write_default_profile(&temp);

    let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    command
        .current_dir(&repository)
        .args(["git", "--", "var", "GIT_AUTHOR_IDENT"]);
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    command
        .assert()
        .failure()
        .stdout(predicate::str::contains("Alice").not())
        .stderr(predicate::str::contains("仓库绑定的 Profile 已不存在"));
}

#[test]
fn stale_binding_credential_helper_stops_fallback_chain() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let repository = temp.path().join("repo");
    let bin = temp.path().join("bin");
    let trace = temp.path().join("gh-trace");
    fs::create_dir_all(&bin).expect("bin directory");
    write_executable(
        &bin.join("gh"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_GH_TRACE"
printf 'must-not-be-returned\n'
"#,
    );
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&repository)
            .status()
            .expect("git init")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "--local", "ghis.profile", "deleted"])
            .current_dir(&repository)
            .status()
            .expect("stale binding")
            .success()
    );
    write_default_profile(&temp);

    let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    command
        .current_dir(&repository)
        .args(["credential-helper", "get"])
        .write_stdin("protocol=https\nhost=github.com\n\n")
        .env("PATH", prepend_path(&bin))
        .env("FAKE_GH_TRACE", &trace);
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    command
        .assert()
        .failure()
        .stdout("quit=true\n\n")
        .stderr(predicate::str::contains("仓库绑定的 Profile 已不存在"))
        .stderr(predicate::str::contains("Alice").not())
        .stderr(predicate::str::contains("must-not-be-returned").not());
    assert!(!trace.exists(), "失效绑定不应尝试从 gh 读取 token");
}

#[test]
fn custom_config_is_pinned_for_native_git_credentials_and_named_hooks() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let repository = temp.path().join("repo");
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&repository)
            .status()
            .expect("git init")
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/worker/example.git",
            ])
            .current_dir(&repository)
            .status()
            .expect("git remote")
            .success()
    );

    // Keep a valid but different default config around. If a generated
    // command drops --config, the repository's `work` binding will be invalid.
    write_default_profile(&temp);
    let custom_config_relative = PathBuf::from("custom config's directory").join("identities.toml");
    let custom_config = temp.path().join(&custom_config_relative);
    fs::create_dir_all(custom_config.parent().expect("custom config parent"))
        .expect("custom config directory");
    fs::write(
        &custom_config,
        r#"version = 1

[profiles.work]
host = "github.com"
login = "worker"
git_name = "Work Identity"
git_email = "work@example.test"
"#,
    )
    .expect("custom config");

    let mut use_command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    use_command
        .current_dir(temp.path())
        .arg("--config")
        .arg(&custom_config_relative)
        .args(["use", "work", "--repo"])
        .arg(&repository)
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for (key, value) in xdg_environment(&temp) {
        use_command.env(key, value);
    }
    use_command
        .assert()
        .success()
        .stdout(predicate::str::contains("Work Identity"))
        .stdout(predicate::str::contains("Alice").not());

    let fake_bin = temp.path().join("bin");
    let trace = temp.path().join("gh-trace");
    fs::create_dir_all(&fake_bin).expect("bin directory");
    write_executable(
        &fake_bin.join("gh"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_GH_TRACE"
if [ "$*" = "auth token --hostname github.com --user worker" ]; then
  printf 'custom-config-token\n'
  exit 0
fi
printf 'unexpected account lookup: %s\n' "$*" >&2
exit 42
"#,
    );

    let mut credential = AssertCommand::new("git");
    credential
        .current_dir(&repository)
        .args(["credential", "fill"])
        .write_stdin("protocol=https\nhost=github.com\n\n")
        .env("PATH", prepend_path(&fake_bin))
        .env("FAKE_GH_TRACE", &trace)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0");
    for (key, value) in xdg_environment(&temp) {
        credential.env(key, value);
    }
    credential
        .assert()
        .success()
        .stdout(predicate::str::contains("username=worker"))
        .stdout(predicate::str::contains("password=custom-config-token"))
        .stdout(predicate::str::contains("alice").not());

    let trace = fs::read_to_string(&trace).expect("gh trace");
    assert_eq!(trace, "auth token --hostname github.com --user worker\n");

    if ghis::git::supports_named_hooks() {
        let mut hook = AssertCommand::new("git");
        hook.current_dir(&repository)
            .args(["hook", "run", "pre-push"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
        for (key, value) in xdg_environment(&temp) {
            hook.env(key, value);
        }
        hook.assert()
            .success()
            .stderr(predicate::str::contains("profile=work"))
            .stderr(predicate::str::contains("Work Identity"))
            .stderr(predicate::str::contains("Alice").not());
    }
}

#[test]
fn invalid_custom_config_stops_credentials_without_default_fallback() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let repository = temp.path().join("repo");
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&repository)
            .status()
            .expect("git init")
            .success()
    );
    write_default_profile(&temp);

    let invalid_config = temp.path().join("broken-custom.toml");
    fs::write(&invalid_config, "this is not valid = [toml\n").expect("invalid config");

    let fake_bin = temp.path().join("bin");
    let trace = temp.path().join("gh-trace");
    fs::create_dir_all(&fake_bin).expect("bin directory");
    write_executable(
        &fake_bin.join("gh"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_GH_TRACE"
printf 'fallback-secret\n'
"#,
    );

    let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    command
        .current_dir(&repository)
        .arg("--config")
        .arg(&invalid_config)
        .args(["credential-helper", "get"])
        .write_stdin("protocol=https\nhost=github.com\n\n")
        .env("PATH", prepend_path(&fake_bin))
        .env("FAKE_GH_TRACE", &trace);
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    command
        .assert()
        .failure()
        .stdout("quit=true\n\n")
        .stderr(predicate::str::contains("配置错误"))
        .stderr(predicate::str::contains("Alice").not())
        .stderr(predicate::str::contains("fallback-secret").not());
    assert!(!trace.exists(), "配置解析失败不应尝试从 gh 读取 token");
}

#[test]
fn git_wrapper_uses_explicit_git_dir_and_work_tree_for_profile_resolution() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let outer = temp.path().join("outer");
    let repository = outer.join("repo");
    fs::create_dir_all(&outer).expect("outer directory");
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&repository)
            .status()
            .expect("git init")
            .success()
    );

    let config_directory = temp.path().join("config/ghis");
    fs::create_dir_all(&config_directory).expect("config directory");
    fs::write(
        config_directory.join("config.toml"),
        r#"version = 1

[behavior]
default_profile = "personal"

[profiles.personal]
host = "github.com"
login = "personal"
git_name = "Personal Identity"
git_email = "personal@example.test"

[profiles.work]
host = "github.com"
login = "worker"
git_name = "Work Identity"
git_email = "work@example.test"
"#,
    )
    .expect("config");

    let mut bind = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    bind.current_dir(temp.path())
        .args(["use", "work", "--repo", "outer/repo"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for (key, value) in xdg_environment(&temp) {
        bind.env(key, value);
    }
    bind.assert().success();

    let mut equals_form = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    equals_form
        .current_dir(temp.path())
        .args([
            "git",
            "--",
            "-C",
            "outer",
            "--git-dir=repo/.git",
            "--work-tree",
            "repo",
            "config",
            "--includes",
            "--get",
            "user.email",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for (key, value) in xdg_environment(&temp) {
        equals_form.env(key, value);
    }
    equals_form.assert().success().stdout("work@example.test\n");

    let mut separated_form = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    separated_form
        .current_dir(temp.path())
        .args([
            "git",
            "--",
            "-C",
            "outer",
            "--git-dir",
            "repo/.git",
            "--work-tree=repo",
            "config",
            "--includes",
            "--get",
            "user.name",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for (key, value) in xdg_environment(&temp) {
        separated_form.env(key, value);
    }
    separated_form.assert().success().stdout("Work Identity\n");
}

#[test]
fn git_aliases_receive_sensitive_banners_and_preflight_checks() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let repository = temp.path().join("repo");
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&repository)
            .status()
            .expect("git init")
            .success()
    );
    for (key, value) in [
        ("alias.ci", "commit"),
        (
            "alias.as-other",
            "commit --author='Alias Author <alias@example.test>'",
        ),
        ("alias.publish", "!git push"),
        ("remote.origin.url", "git@github.com:worker/example.git"),
    ] {
        assert!(
            Command::new("git")
                .args(["config", "--local", key, value])
                .current_dir(&repository)
                .status()
                .expect("git config")
                .success()
        );
    }
    let config_directory = temp.path().join("config/ghis");
    fs::create_dir_all(&config_directory).expect("config directory");
    fs::write(
        config_directory.join("config.toml"),
        r#"version = 1

[behavior]
default_profile = "work"
ssh_unmanaged = "fail"

[profiles.work]
host = "github.com"
login = "worker"
git_name = "Work Identity"
git_email = "work@example.test"
"#,
    )
    .expect("config");

    let mut commit = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    commit
        .current_dir(&repository)
        .args(["git", "--", "ci", "--allow-empty", "-m", "alias commit"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for (key, value) in xdg_environment(&temp) {
        commit.env(key, value);
    }
    commit
        .assert()
        .success()
        .stderr(predicate::str::contains("profile=work"))
        .stderr(predicate::str::contains(
            "Work Identity <work@example.test>",
        ));

    let mut alias_author = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    alias_author
        .current_dir(&repository)
        .args([
            "git",
            "--",
            "as-other",
            "--allow-empty",
            "-m",
            "alias author",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for (key, value) in xdg_environment(&temp) {
        alias_author.env(key, value);
    }
    alias_author
        .assert()
        .success()
        .stderr(predicate::str::contains("commit --author"))
        .stderr(predicate::str::contains(
            "作者=Alias Author <alias@example.test>",
        ));

    let mut reuse_author = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    reuse_author
        .current_dir(&repository)
        .args(["git", "--", "commit", "--allow-empty", "-C", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for (key, value) in xdg_environment(&temp) {
        reuse_author.env(key, value);
    }
    reuse_author
        .assert()
        .success()
        .stderr(predicate::str::contains("commit -C"))
        .stderr(predicate::str::contains(
            "由被复用的提交决定（无法预先解析）",
        ));

    let author = Command::new("git")
        .args(["log", "-1", "--format=%an|%ae"])
        .current_dir(&repository)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("read reused author");
    assert!(author.status.success());
    assert_eq!(
        String::from_utf8_lossy(&author.stdout).trim(),
        "Alias Author|alias@example.test"
    );

    let mut push = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    push.current_dir(&repository)
        .args(["git", "--", "publish"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for (key, value) in xdg_environment(&temp) {
        push.env(key, value);
    }
    push.assert()
        .failure()
        .stderr(predicate::str::contains("当前 Profile 未纳管 SSH key"));
}

#[test]
fn commit_identity_overrides_are_reported_without_blocking() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let repository = temp.path().join("repo");
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&repository)
            .status()
            .expect("git init")
            .success()
    );
    let config_directory = temp.path().join("config/ghis");
    fs::create_dir_all(&config_directory).expect("config directory");
    fs::write(
        config_directory.join("config.toml"),
        r#"version = 1

[behavior]
default_profile = "work"

[profiles.work]
host = "github.com"
login = "worker"
git_name = "Work Identity"
git_email = "work@example.test"
"#,
    )
    .expect("config");

    let mut commit = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    commit
        .current_dir(&repository)
        .args([
            "git",
            "--",
            "-c",
            "user.name=Command Identity",
            "-c",
            "user.email=command@example.test",
            "commit",
            "--author",
            "Flag Author <flag@example.test>",
            "--allow-empty",
            "-m",
            "identity override",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Environment Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.test")
        .env("GIT_COMMITTER_NAME", "Environment Committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.test");
    for (key, value) in xdg_environment(&temp) {
        commit.env(key, value);
    }
    commit
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "配置身份=Work Identity <work@example.test>",
        ))
        .stderr(predicate::str::contains(
            "作者=Flag Author <flag@example.test>",
        ))
        .stderr(predicate::str::contains(
            "提交者=Environment Committer <committer@example.test>",
        ))
        .stderr(predicate::str::contains("git -c user.name"))
        .stderr(predicate::str::contains("环境变量 GIT_AUTHOR_NAME"))
        .stderr(predicate::str::contains("commit --author"))
        .stderr(predicate::str::contains("实际身份可能与配置身份不同"));

    let identity = Command::new("git")
        .args(["log", "-1", "--format=%an|%ae|%cn|%ce"])
        .current_dir(&repository)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("read commit identity");
    assert!(identity.status.success());
    assert_eq!(
        String::from_utf8_lossy(&identity.stdout).trim(),
        "Flag Author|flag@example.test|Environment Committer|committer@example.test"
    );
}

#[test]
fn direct_hook_banner_reports_git_resolved_author_and_committer() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_default_profile(&temp);
    let repository = temp.path().join("repository");
    fs::create_dir_all(&repository).expect("repository directory");
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repository)
            .status()
            .expect("git init")
            .success()
    );

    let mut bind = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    bind.args([
        "use",
        "personal",
        "--repo",
        repository.to_str().expect("UTF-8 repository path"),
    ])
    .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for (key, value) in xdg_environment(&temp) {
        bind.env(key, value);
    }
    bind.assert().success();

    let mut hook = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    hook.current_dir(&repository)
        .args(["hook", "--hook", "prepare-commit-msg"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Actual Author")
        .env("GIT_AUTHOR_EMAIL", "actual-author@example.test")
        .env("GIT_COMMITTER_NAME", "Actual Committer")
        .env("GIT_COMMITTER_EMAIL", "actual-committer@example.test");
    for (key, value) in xdg_environment(&temp) {
        hook.env(key, value);
    }
    hook.assert()
        .success()
        .stderr(predicate::str::contains(
            "实际作者=Actual Author <actual-author@example.test>",
        ))
        .stderr(predicate::str::contains(
            "实际提交者=Actual Committer <actual-committer@example.test>",
        ));
}

#[test]
fn profile_edit_preserves_unmentioned_one_password_and_signing_fields() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let config_directory = temp.path().join("config/ghis");
    fs::create_dir_all(&config_directory).expect("config directory");
    let config_file = config_directory.join("config.toml");
    fs::write(
        &config_file,
        r#"version = 1

[profiles.work]
host = "github.com"
login = "worker"
git_name = "Work Identity"
git_email = "old@example.test"

[profiles.work.ssh]
mode = "one-password"
public_key = "/keys/work.pub"
fingerprint = "SHA256:work"
agent_socket = "/run/user/1000/op-agent.sock"

[profiles.work.signing]
enabled = true
signing_key = "ssh-ed25519 AAAATEST"
program = "/opt/1Password/op-ssh-sign"
"#,
    )
    .expect("config");

    let mut edit = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    edit.args(["profile", "edit", "work", "--email", "new@example.test"]);
    for (key, value) in xdg_environment(&temp) {
        edit.env(key, value);
    }
    edit.assert().success();

    let config = Config::load(&config_file).expect("load edited config");
    let profile = config.profiles.get("work").expect("work profile");
    assert_eq!(profile.host, "github.com");
    assert_eq!(profile.login, "worker");
    assert_eq!(profile.git_name, "Work Identity");
    assert_eq!(profile.git_email, "new@example.test");
    let ssh = profile.ssh.as_ref().expect("SSH profile");
    assert_eq!(ssh.mode, SshMode::OnePassword);
    assert_eq!(ssh.public_key.as_deref(), Some(Path::new("/keys/work.pub")));
    assert_eq!(ssh.fingerprint.as_deref(), Some("SHA256:work"));
    assert_eq!(
        ssh.agent_socket.as_deref(),
        Some(Path::new("/run/user/1000/op-agent.sock"))
    );
    assert!(profile.signing.enabled);
    assert_eq!(
        profile.signing.signing_key.as_deref(),
        Some("ssh-ed25519 AAAATEST")
    );
    assert_eq!(
        profile.signing.program.as_deref(),
        Some(Path::new("/opt/1Password/op-ssh-sign"))
    );
}

#[test]
fn discover_and_doctor_refresh_the_same_redacted_account_cache() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let fake_bin = temp.path().join("bin");
    fs::create_dir_all(&fake_bin).expect("fake bin directory");
    write_executable(
        &fake_bin.join("gh"),
        r#"#!/bin/sh
if [ "$1" = auth ] && [ "$2" = status ]; then
  printf '{"hosts":{"git.example.com":[{"host":"git.example.com","login":"%s","active":true,"state":"success","tokenSource":"keyring"}]}}\n' "$FAKE_GH_LOGIN"
  exit 0
fi
printf 'gh version test\n'
"#,
    );

    let mut discover = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    discover
        .args(["discover", "--json"])
        .env("PATH", prepend_path(&fake_bin))
        .env("FAKE_GH_LOGIN", "alice");
    for (key, value) in xdg_environment(&temp) {
        discover.env(key, value);
    }
    discover.assert().success();

    let cache = temp.path().join("cache/ghis/accounts.json");
    let first = ghis::github::load_discovery_cache(&cache)
        .expect("read discovery cache")
        .expect("discovery cache exists");
    assert_eq!(first.accounts[0].login, "alice");
    assert_eq!(
        fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let mut doctor = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    doctor
        .args(["doctor", "--json"])
        .env("PATH", prepend_path(&fake_bin))
        .env("FAKE_GH_LOGIN", "bob");
    for (key, value) in xdg_environment(&temp) {
        doctor.env(key, value);
    }
    doctor.assert().success();

    let second = ghis::github::load_discovery_cache(&cache)
        .expect("read refreshed discovery cache")
        .expect("refreshed discovery cache exists");
    assert_eq!(second.accounts[0].login, "bob");
    assert_eq!(second.accounts[0].host, "git.example.com");
}

#[test]
fn discover_keeps_state_failure_accounts_as_offline_cache_entries() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let fake_bin = temp.path().join("bin");
    fs::create_dir_all(&fake_bin).expect("fake bin directory");
    write_executable(
        &fake_bin.join("gh"),
        r#"#!/bin/sh
printf '{"hosts":{"github.com":[{"login":"alice","state":"failure","error":"offline"}]}}\n'
exit 1
"#,
    );

    let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    command
        .args(["discover", "--json"])
        .env("PATH", prepend_path(&fake_bin));
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    command
        .assert()
        .success()
        .stdout(predicate::str::contains("\"command_succeeded\": false"))
        .stdout(predicate::str::contains("\"offline\": true"));

    let cache = temp.path().join("cache/ghis/accounts.json");
    let cached = ghis::github::load_discovery_cache(&cache)
        .expect("read discovery cache")
        .expect("discovery cache exists");
    assert!(cached.offline);
    assert!(!cached.accounts[0].verified);
    assert_eq!(cached.accounts[0].state.as_deref(), Some("failure"));
}
