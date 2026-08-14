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

const GHIS_CONTROL_ENV: [&str; 10] = [
    "GHIS_CONFIG",
    "GHIS_PROFILE",
    "GHIS_BANNER_SHOWN",
    "GHIS_WRAPPER_ACTIVE",
    "GHIS_BYPASS",
    "GHIS_DISABLE_CHPWD",
    "GHIS_CHPWD_ENABLED",
    "GHIS_REPO_PROFILE",
    "GHIS_REPO_PROFILE_DISPLAY",
    "GHIS_REPO_ROOT",
];

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

/// Construct a ghis child with caller-side selection state removed.
///
/// Integration tests always provide their own HOME/XDG roots below.  Clearing
/// the ghis control variables as well prevents a developer's shell wrapper or
/// custom config from leaking into a child process and writing real user
/// state while the test suite is running.
fn isolated_ghis_command() -> AssertCommand {
    let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    clear_ghis_assert_environment(&mut command);
    command
}

fn clear_ghis_assert_environment(command: &mut AssertCommand) {
    for key in GHIS_CONTROL_ENV {
        command.env_remove(key);
    }
}

fn clear_ghis_environment(command: &mut Command) {
    for key in GHIS_CONTROL_ENV {
        command.env_remove(key);
    }
}

fn isolated_git_command() -> AssertCommand {
    let mut command = AssertCommand::new("git");
    clear_ghis_assert_environment(&mut command);
    command
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
fn long_version_reports_build_provenance() {
    isolated_ghis_command()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with(format!(
            "ghis {}\n",
            env!("CARGO_PKG_VERSION")
        )))
        .stdout(predicate::str::contains("commit: "))
        .stdout(predicate::str::contains("tag: "))
        .stdout(predicate::str::contains("built: "))
        .stdout(predicate::str::contains("SOURCE_DATE_EPOCH: "))
        .stdout(predicate::str::contains("target: "))
        .stdout(predicate::str::contains("profile: "));
}

#[test]
fn profile_list_details_are_human_readable_and_match_show() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let directory = temp.path().join("config/ghis");
    fs::create_dir_all(&directory).expect("config directory");
    fs::write(
        directory.join("config.toml"),
        r#"version = 1

[profiles.personal]
host = "github.com"
login = "alice"
git_name = "Alice"
git_email = "alice@example.test"
description = "个人项目"

[profiles.work]
host = "github.example.test"
login = "alice-work"
git_name = "Alice Work"
git_email = "alice@work.example"
"#,
    )
    .expect("config");

    let configure = |command: &mut AssertCommand| {
        for (key, value) in xdg_environment(&temp) {
            command.env(key, value);
        }
    };
    let mut details = isolated_ghis_command();
    details.args(["profile", "list", "-d"]);
    configure(&mut details);
    let details = details.assert().success().get_output().stdout.clone();
    let details = String::from_utf8(details).expect("utf-8 details");
    assert_eq!(
        details,
        "Profile：personal\n描述：个人项目\n提交身份：Alice <alice@example.test>\nGitHub：alice@github.com\n签名：关闭\n\nProfile：work\n提交身份：Alice Work <alice@work.example>\nGitHub：alice-work@github.example.test\n签名：关闭\n"
    );

    let mut show = isolated_ghis_command();
    show.args(["profile", "show", "personal"]);
    configure(&mut show);
    let show = show.assert().success().get_output().stdout.clone();
    let show = String::from_utf8(show).expect("utf-8 show");
    assert!(details.starts_with(&show));

    let mut conflict = isolated_ghis_command();
    conflict.args(["profile", "list", "-d", "-j"]);
    configure(&mut conflict);
    conflict.assert().failure();
}

#[test]
fn profile_short_options_write_expected_fields() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let mut add = isolated_ghis_command();
    add.args([
        "profile",
        "add",
        "work",
        "-H",
        "github.example.test",
        "-l",
        "alice-work",
        "-n",
        "Alice Work",
        "-e",
        "alice@work.example",
        "-d",
        "公司项目",
    ]);
    for (key, value) in xdg_environment(&temp) {
        add.env(key, value);
    }
    add.assert().success();

    let mut show = isolated_ghis_command();
    show.args(["profile", "show", "work", "-j"]);
    for (key, value) in xdg_environment(&temp) {
        show.env(key, value);
    }
    show.assert()
        .success()
        .stdout(predicate::str::contains(
            "\"host\": \"github.example.test\"",
        ))
        .stdout(predicate::str::contains("\"login\": \"alice-work\""))
        .stdout(predicate::str::contains("\"git_name\": \"Alice Work\""))
        .stdout(predicate::str::contains(
            "\"git_email\": \"alice@work.example\"",
        ))
        .stdout(predicate::str::contains("\"description\": \"公司项目\""));
}

#[test]
fn status_shows_only_profile_and_description() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_default_profile(&temp);
    let config = temp.path().join("config/ghis/config.toml");
    let mut contents = fs::read_to_string(&config).expect("config");
    contents.push_str("description = \"个人开源项目\"\n");
    fs::write(&config, contents).expect("update config");

    let mut command = isolated_ghis_command();
    command.current_dir(temp.path()).arg("status");
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    command
        .assert()
        .success()
        .stdout("当前 Profile：personal\n  个人开源项目\n")
        .stdout(predicate::str::contains(temp.path().display().to_string()).not())
        .stdout(predicate::str::contains("Alice").not())
        .stdout(predicate::str::contains("alice@example.test").not());
}

#[test]
fn bare_command_defaults_to_status() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_default_profile(&temp);

    let run = |args: &[&str]| {
        let mut command = isolated_ghis_command();
        command.current_dir(temp.path()).args(args);
        for (key, value) in xdg_environment(&temp) {
            command.env(key, value);
        }
        command.output().expect("run ghis")
    };

    let bare = run(&[]);
    let status = run(&["status"]);
    assert_eq!(bare.status, status.status);
    assert_eq!(bare.stdout, status.stdout);
    assert_eq!(bare.stderr, status.stderr);
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

    let mut command = isolated_ghis_command();
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

    let mut command = isolated_ghis_command();
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

    let mut command = isolated_ghis_command();
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

    let mut positional = isolated_ghis_command();
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

    let mut item_url = isolated_ghis_command();
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
        let mut guarded = isolated_ghis_command();
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

    let mut body_url = isolated_ghis_command();
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
        let mut command = isolated_ghis_command();
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
        let mut command = isolated_ghis_command();
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

    let mut body = isolated_ghis_command();
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
        let mut command = isolated_ghis_command();
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

    let mut command = isolated_ghis_command();
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
    let mut mismatched = isolated_ghis_command();
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
fn gh_explicit_targets_select_non_default_profiles_outside_git() {
    let temp = tempfile::tempdir().expect("temporary directory");
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
git_name = "Personal"
git_email = "personal@example.test"

[profiles.acme]
host = "github.com"
login = "acme"
git_name = "Acme"
git_email = "acme@example.test"

[profiles.enterprise]
host = "git.example.test"
login = "platform"
git_name = "Platform"
git_email = "platform@example.test"

[[rules]]
id = "enterprise-owner"
profile = "enterprise"
priority = 100
host = "git.example.test"
owner = "platform"
"#,
    )
    .expect("config");
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).expect("bin directory");
    let trace = temp.path().join("gh.trace");
    write_executable(
        &bin.join("gh"),
        r#"#!/bin/sh
printf '%s host=%s repo=%s\n' "$*" "${GH_HOST-}" "${GH_REPO-}" >> "$FAKE_GH_TRACE"
case "$*" in
  "auth token --hostname github.com --user acme") printf 'acme-token\n'; exit 0 ;;
  "auth token --hostname github.com --user personal") printf 'personal-token\n'; exit 0 ;;
  "auth token --hostname git.example.test --user platform") printf 'enterprise-token\n'; exit 0 ;;
esac
case "$GH_HOST" in github.com|git.example.test) exit 0 ;; *) exit 91 ;; esac
"#,
    );

    let cases = [
        (vec!["repo", "view", "--repo", "acme/project"], None),
        (
            vec!["pr", "view", "https://github.com/acme/project/pull/7"],
            None,
        ),
        (
            vec![
                "issue",
                "view",
                "https://git.example.test/platform/app/issues/9",
            ],
            None,
        ),
        (vec!["pr", "list"], Some("git.example.test/platform/app")),
    ];
    for (args, gh_repo) in cases {
        let mut command = isolated_ghis_command();
        command
            .current_dir("/tmp")
            .arg("gh")
            .arg("--")
            .args(args)
            .env("PATH", prepend_path(&bin))
            .env("FAKE_GH_TRACE", &trace);
        if let Some(repo) = gh_repo {
            command.env("GH_REPO", repo);
        } else {
            command.env_remove("GH_REPO");
        }
        for (key, value) in xdg_environment(&temp) {
            command.env(key, value);
        }
        command.assert().success();
    }
    let trace = fs::read_to_string(trace).expect("trace");
    assert!(trace.contains("--user acme"));
    assert!(trace.contains("--user platform"));
    assert!(!trace.contains("--user personal"));
}

#[test]
fn explicit_profile_still_precedes_gh_target_and_number_only_does_not_guess() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_default_profile(&temp);
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).expect("bin directory");
    let trace = temp.path().join("gh.trace");
    write_executable(
        &bin.join("gh"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_GH_TRACE"
if [ "$1" = auth ] && [ "$2" = token ]; then printf 'token\n'; exit 0; fi
exit 0
"#,
    );
    let mut command = isolated_ghis_command();
    command
        .current_dir("/tmp")
        .args([
            "--profile",
            "personal",
            "gh",
            "--",
            "pr",
            "view",
            "https://github.com/other/project/pull/1",
        ])
        .env("PATH", prepend_path(&bin))
        .env("FAKE_GH_TRACE", &trace);
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    command.assert().success();
    assert!(fs::read_to_string(&trace).unwrap().contains("--user alice"));

    fs::remove_file(&trace).unwrap();
    let mut number = isolated_ghis_command();
    number
        .current_dir("/tmp")
        .args(["gh", "--", "pr", "view", "7"])
        .env("PATH", prepend_path(&bin))
        .env("FAKE_GH_TRACE", &trace);
    for (key, value) in xdg_environment(&temp) {
        number.env(key, value);
    }
    number.assert().success();
    assert!(fs::read_to_string(&trace).unwrap().contains("--user alice"));
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

    let mut command = isolated_ghis_command();
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

    let mut command = isolated_ghis_command();
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
fn git_version_options_keep_injected_config_before_the_option_terminator() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let mut direct = isolated_ghis_command();
    direct
        .current_dir(temp.path())
        .args(["git", "--", "--version"]);
    for (key, value) in xdg_environment(&temp) {
        direct.env(key, value);
    }
    direct
        .assert()
        .success()
        .stdout(predicate::str::starts_with("git version "));

    let init = temp.path().join("init.zsh");
    fs::write(&init, ghis::shell::zsh_init_script("ghis")).expect("init script");
    let binary = assert_cmd::cargo::cargo_bin!("ghis");
    let mut wrapped = Command::new("zsh");
    wrapped
        .args([
            "-f",
            "-c",
            "source \"$GHIS_TEST_INIT\"; unfunction _ghis_dispatch _ghis_chpwd 2>/dev/null || true; git --version",
        ])
        .current_dir(&temp)
        .env("GHIS_TEST_INIT", &init)
        .env(
            "PATH",
            prepend_path(binary.parent().expect("binary directory")),
        )
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for (key, value) in xdg_environment(&temp) {
        wrapped.env(key, value);
    }
    clear_ghis_environment(&mut wrapped);
    let output = wrapped.output().expect("run wrapped git version");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("git version "));
}

#[test]
fn chpwd_status_is_disabled_without_repository_probe_by_default() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let mut command = isolated_ghis_command();
    command.args(["status", "--shell"]);
    command.current_dir(temp.path());
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    command
        .assert()
        .success()
        .stdout(predicate::str::contains("GHIS_CHPWD_ENABLED=0"))
        .stdout(predicate::str::contains("GHIS_REPO_PROFILE=''"));
}

#[test]
fn chpwd_status_reports_non_authoritative_profile_when_enabled() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo directory");
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .expect("git init");
    assert!(init.success());
    let config_dir = temp.path().join("config/ghis");
    fs::create_dir_all(&config_dir).expect("config directory");
    fs::write(
        config_dir.join("config.toml"),
        "version = 1\n[behavior]\ndefault_profile = \"personal\"\ndisplay_profile_on_chpwd = true\n[profiles.personal]\nhost = \"github.com\"\nlogin = \"alice\"\ngit_name = \"Alice\"\ngit_email = \"alice@example.test\"\n",
    )
    .expect("config");

    let mut command = isolated_ghis_command();
    command.args(["status", "--shell"]);
    command.current_dir(&repo);
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    command
        .assert()
        .success()
        .stdout(predicate::str::contains("GHIS_CHPWD_ENABLED=1"))
        .stdout(predicate::str::contains("GHIS_REPO_PROFILE='personal'"))
        .stdout(predicate::str::contains("GHIS_REPO_ROOT="))
        .stdout(predicate::str::contains("GHIS_PROFILE=").not());
}

#[test]
fn zsh_chpwd_is_silent_on_source_and_displays_profile_after_directory_change() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo directory");
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("git init")
            .success()
    );
    let config_dir = temp.path().join("config/ghis");
    fs::create_dir_all(&config_dir).expect("config directory");
    fs::write(
        config_dir.join("config.toml"),
        "version = 1\n[behavior]\ndefault_profile = \"personal\"\ndisplay_profile_on_chpwd = true\n[profiles.personal]\nhost = \"github.com\"\nlogin = \"alice\"\ngit_name = \"Alice\"\ngit_email = \"alice@example.test\"\n",
    )
    .expect("config");
    let init = temp.path().join("init.zsh");
    let ghis_bin = assert_cmd::cargo::cargo_bin!("ghis");
    fs::write(
        &init,
        ghis::shell::zsh_init_script(&ghis_bin.to_string_lossy()),
    )
    .expect("init script");

    let mut command = Command::new("zsh");
    command
        .args([
            "-f",
            "-c",
            "source \"$1\"; cd \"$2\"; print -r -- \"repo=$GHIS_REPO_PROFILE authoritative=${GHIS_PROFILE-unset}\"",
            "zsh",
        ])
        .arg(&init)
        .arg(&repo)
        .current_dir(temp.path());
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    clear_ghis_environment(&mut command);
    let output = command.output().expect("run zsh chpwd hook");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.matches("GhIS  Profile  personal").count(),
        1,
        "{stdout}"
    );
    assert!(
        stdout.contains("repo=personal authoritative=unset"),
        "{stdout}"
    );
}

#[test]
fn generated_zsh_wrapper_is_valid_and_preserves_git_exit_status() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let output = isolated_ghis_command()
        .args(["init", "zsh"])
        .output()
        .expect("generate init");
    assert!(output.status.success());
    let init = temp.path().join("init.zsh");
    fs::write(&init, output.stdout).expect("init script");

    let mut syntax = Command::new("zsh");
    clear_ghis_environment(&mut syntax);
    assert!(
        syntax
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
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    clear_ghis_environment(&mut command);
    let status = command.status().expect("run wrapper");
    assert_eq!(status.code(), Some(1));
}

#[test]
fn codex_wrapper_routes_direct_launch_and_allows_explicit_bypass() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_default_profile(&temp);
    let fake_bin = temp.path().join("fake-bin");
    fs::create_dir_all(&fake_bin).expect("fake bin");
    let trace = temp.path().join("codex-trace");
    write_executable(
        &fake_bin.join("codex"),
        r#"#!/bin/sh
printf '%s\n' "$@" > "$CODEX_TRACE"
"#,
    );
    let init = temp.path().join("init.zsh");
    let ghis_bin = assert_cmd::cargo::cargo_bin!("ghis");
    fs::write(
        &init,
        ghis::shell::zsh_init_script(&ghis_bin.to_string_lossy()),
    )
    .expect("init");
    let path = std::env::join_paths(std::iter::once(fake_bin.clone()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .expect("PATH");
    let mut command = Command::new("zsh");
    command
        .args(["-f", "-c", "source \"$GHIS_TEST_INIT\"; codex --model test"])
        .current_dir(temp.path())
        .env("GHIS_TEST_INIT", &init)
        .env("CODEX_TRACE", &trace)
        .env("PATH", &path);
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    clear_ghis_environment(&mut command);
    assert!(command.status().expect("run wrapped codex").success());
    let arguments = fs::read_to_string(&trace).expect("trace");
    assert!(arguments.contains("developer_instructions=ghis session context"));
    assert!(arguments.contains("--model\ntest"));

    let mut bypass = Command::new("zsh");
    bypass
        .args([
            "-f",
            "-c",
            "source \"$GHIS_TEST_INIT\"; GHIS_BYPASS=1 codex --model raw",
        ])
        .current_dir(temp.path())
        .env("GHIS_TEST_INIT", &init)
        .env("CODEX_TRACE", &trace)
        .env("PATH", path);
    clear_ghis_environment(&mut bypass);
    assert!(bypass.status().expect("run bypassed codex").success());
    assert_eq!(fs::read_to_string(trace).expect("trace"), "--model\nraw\n");
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
    let mut command = Command::new("script");
    command
        .args(["-q", "-e", "-c"])
        .arg(&probe)
        .arg("/dev/null")
        .current_dir(temp.path())
        .env("PATH", path)
        .env("GHIS_REAL_GIT", real_git)
        .env("HOME", temp.path().join("home"))
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_CACHE_HOME", temp.path().join("cache"))
        .env("XDG_STATE_HOME", temp.path().join("state"));
    clear_ghis_environment(&mut command);
    let output = command.output().expect("run wrapper in a pseudo-terminal");

    assert_eq!(output.status.code(), Some(130));
    assert!(String::from_utf8_lossy(&output.stdout).contains("tty-ok"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("tty-lost"));
}

#[test]
fn onboard_rejects_noninteractive_input_without_writing_config() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let mut command = isolated_ghis_command();
    command.arg("onboard").write_stdin("alice\n");
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    command
        .assert()
        .failure()
        .stderr(predicate::str::contains("ghis onboard"));
    assert!(!temp.path().join("config").exists());
    assert!(!temp.path().join("cache").exists());
    assert!(!temp.path().join("state").exists());
}

#[test]
fn onboard_dynamic_prompt_supports_hjkl_text_and_keeps_terminal_local() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let fake_bin = temp.path().join("bin");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&fake_bin).expect("fake bin");
    fs::create_dir_all(&repository).expect("repository");
    write_executable(
        &fake_bin.join("gh"),
        r#"#!/bin/sh
set -eu
case "$*" in
  'auth status --json hosts')
    printf '%s\n' '{"hosts":{"github.com":[{"host":"github.com","login":"alice","active":true,"state":"success"}]}}'
    ;;
  'auth token --hostname github.com --user alice') printf '%s\n' test-token ;;
  'api user') printf '%s\n' '{"id":12345,"login":"alice"}' ;;
  'api user/emails') printf '%s\n' '[{"email":"alice@example.test","primary":true,"verified":true}]' ;;
  *) printf 'unexpected gh args: %s\n' "$*" >&2; exit 64 ;;
esac
"#,
    );
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repository)
            .status()
            .expect("initialize repository")
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/alice/example.git",
            ])
            .current_dir(&repository)
            .status()
            .expect("add remote")
            .success()
    );

    let binary = assert_cmd::cargo::cargo_bin!("ghis");
    let child_command = format!(
        "{} onboard --repo {}",
        ghis::shell::shell_quote(&binary.to_string_lossy()),
        ghis::shell::shell_quote(&repository.to_string_lossy())
    );
    let mut command = Command::new("script");
    command
        .args(["-q", "-e", "-c"])
        .arg(child_command)
        .arg("/dev/null")
        .env("PATH", prepend_path(&fake_bin))
        .env("NO_COLOR", "1")
        .env("COLUMNS", "80")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    clear_ghis_environment(&mut command);
    let mut child = command.spawn().expect("spawn onboarding PTY");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let mut stdin = child.stdin.take().expect("PTY stdin");
    std::io::Write::write_all(
        &mut stdin,
        b"\r\x7f\x7f\x7f\x7f\x7fpersonal\rHjkl User\r\rll",
    )
    .expect("drive onboarding");
    drop(stdin);
    let output = child.wait_with_output().expect("wait for onboarding");
    assert!(
        output.status.success(),
        "onboarding failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let transcript = String::from_utf8_lossy(&output.stdout);
    assert!(transcript.contains("ghis 初始设置"));
    assert!(transcript.contains("Hjkl User"));
    assert!(transcript.contains("✓ 已完成 ghis 初始设置"));
    assert!(transcript.contains("\x1b["));
    assert!(!transcript.contains("\x1b[J"));
    assert!(!transcript.contains("\x1b[2J"));
    assert!(!transcript.contains("\x1b[?1049h"));

    let config = Config::load(temp.path().join("config/ghis/config.toml")).expect("saved config");
    let profile = config.profiles.get("personal").expect("created profile");
    assert_eq!(profile.git_name, "Hjkl User");
    assert_eq!(profile.git_email, "12345+alice@users.noreply.github.com");
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

    let mut refused = isolated_ghis_command();
    refused.arg("setup");
    configure(&mut refused);
    refused
        .assert()
        .failure()
        .stderr(predicate::str::contains("ghis setup --yes"));

    let mut setup = isolated_ghis_command();
    setup.args(["setup", "--yes"]);
    configure(&mut setup);
    setup.assert().success().stdout(predicate::str::contains(
        zdotdir.join(".zshrc").display().to_string(),
    ));

    let contents = fs::read_to_string(zdotdir.join(".zshrc")).expect("configured zshrc");
    assert!(contents.contains(ghis::shell::START_MARKER));
    assert!(!home.join(".zshrc").exists());

    let mut second = isolated_ghis_command();
    second.args(["setup", "--yes"]);
    configure(&mut second);
    second
        .assert()
        .success()
        .stdout(predicate::str::contains("无需更新"));

    let mut uninstall = isolated_ghis_command();
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
        clear_ghis_environment(&mut command);
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

    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("ghis"));
    clear_ghis_environment(&mut command);
    let status = command
        .args(["completion", "zsh"])
        .stdout(Stdio::from(writer))
        .status()
        .expect("run completion with closed stdout");

    assert!(status.success());
}

#[test]
fn init_output_is_valid_when_piped_directly_to_zsh() {
    let mut command = Command::new("zsh");
    command
        .args([
            "-f",
            "-c",
            "setopt pipefail; \"$GHIS_TEST_BIN\" init zsh | command zsh -n",
        ])
        .env("GHIS_TEST_BIN", assert_cmd::cargo::cargo_bin!("ghis"));
    clear_ghis_environment(&mut command);
    let status = command.status().expect("pipe init into zsh");

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

    let init_output = isolated_ghis_command()
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
    clear_ghis_environment(&mut command);
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

    let mut command = isolated_ghis_command();
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

    let mut command = isolated_ghis_command();
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

    let mut use_command = isolated_ghis_command();
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
        .stdout(predicate::str::contains("已绑定 Profile `work`。"))
        .stdout(predicate::str::contains("Work Identity").not())
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

    let mut credential = isolated_git_command();
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
        let mut hook = isolated_git_command();
        hook.current_dir(&repository)
            .args(["hook", "run", "pre-push"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
        for (key, value) in xdg_environment(&temp) {
            hook.env(key, value);
        }
        hook.assert()
            .success()
            .stderr(predicate::str::contains("profile=work"))
            .stderr(predicate::str::contains("Work Identity").not())
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

    let mut command = isolated_ghis_command();
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

    let mut bind = isolated_ghis_command();
    bind.current_dir(temp.path())
        .args(["use", "work", "--repo", "outer/repo"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for (key, value) in xdg_environment(&temp) {
        bind.env(key, value);
    }
    bind.assert().success();

    let mut equals_form = isolated_ghis_command();
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

    let mut separated_form = isolated_ghis_command();
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

    let mut commit = isolated_ghis_command();
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
        .stderr(predicate::str::contains("Work Identity").not())
        .stderr(predicate::str::contains("work@example.test").not());

    let mut alias_author = isolated_ghis_command();
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

    let mut reuse_author = isolated_ghis_command();
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

    let mut push = isolated_ghis_command();
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

    let mut commit = isolated_ghis_command();
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

    let mut bind = isolated_ghis_command();
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

    let mut hook = isolated_ghis_command();
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
fn profile_description_can_be_added_and_cleared() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let configure = |command: &mut AssertCommand| {
        for (key, value) in xdg_environment(&temp) {
            command.env(key, value);
        }
    };

    let mut add = isolated_ghis_command();
    add.args([
        "profile",
        "add",
        "work",
        "--login",
        "worker",
        "--name",
        "Work Identity",
        "--email",
        "work@example.test",
        "--description",
        "公司项目",
    ]);
    configure(&mut add);
    add.assert().success();

    let mut list = isolated_ghis_command();
    list.args(["profile", "list"]);
    configure(&mut list);
    list.assert().success().stdout("work\n  公司项目\n");

    let mut show = isolated_ghis_command();
    show.args(["profile", "show", "work"]);
    configure(&mut show);
    show.assert()
        .success()
        .stdout(predicate::str::contains("描述：公司项目"));

    let mut clear = isolated_ghis_command();
    clear.args(["profile", "edit", "work", "--clear-description"]);
    configure(&mut clear);
    clear.assert().success();

    let mut list_after_clear = isolated_ghis_command();
    list_after_clear.args(["profile", "list"]);
    configure(&mut list_after_clear);
    list_after_clear.assert().success().stdout("work\n");
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

    let mut edit = isolated_ghis_command();
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
fn profile_add_noreply_uses_canonical_github_login_without_email_scope() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let bin = temp.path().join("bin");
    let trace = temp.path().join("gh.trace");
    fs::create_dir_all(&bin).expect("bin directory");
    write_executable(
        &bin.join("gh"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_GH_TRACE"
case "$*" in
  "auth token --hostname github.com --user input-login") printf 'selected-token\n' ;;
  "api user")
    [ "$GH_TOKEN" = selected-token ] || exit 81
    [ "$GH_HOST" = github.com ] || exit 82
    printf '%s\n' '{"id":12345678,"login":"Xe0rF"}'
    ;;
  *) exit 83 ;;
esac
"#,
    );

    let mut add = isolated_ghis_command();
    add.args([
        "profile",
        "add",
        "personal",
        "--login",
        "input-login",
        "--name",
        "Xe0rF",
        "--github-noreply",
    ])
    .env("PATH", prepend_path(&bin))
    .env("FAKE_GH_TRACE", &trace);
    for (key, value) in xdg_environment(&temp) {
        add.env(key, value);
    }
    add.assert().success();

    let config = Config::load(temp.path().join("config/ghis/config.toml")).expect("config");
    assert_eq!(
        config.profiles["personal"].git_email,
        "12345678+Xe0rF@users.noreply.github.com"
    );
    let trace = fs::read_to_string(trace).expect("gh trace");
    assert!(trace.contains("api user\n"));
    assert!(!trace.contains("user/emails"));
    assert!(!trace.contains("selected-token"));
}

#[test]
fn profile_mail_lists_noreply_when_email_endpoint_has_no_scope() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_default_profile(&temp);
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).expect("bin directory");
    write_executable(
        &bin.join("gh"),
        r#"#!/bin/sh
case "$*" in
  "auth token --hostname github.com --user alice") printf 'selected-token\n' ;;
  "api user") printf '%s\n' '{"id":42,"login":"CanonicalLogin"}' ;;
  "api user/emails")
    printf 'requires user:email scope; token=selected-token' >&2
    exit 1
    ;;
  *) exit 84 ;;
esac
"#,
    );

    let mut mail = isolated_ghis_command();
    mail.args(["profile", "mail", "personal", "--json"])
        .env("PATH", prepend_path(&bin));
    for (key, value) in xdg_environment(&temp) {
        mail.env(key, value);
    }
    let output = mail.output().expect("run profile mail");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let candidates: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(candidates.as_array().unwrap().len(), 1);
    assert_eq!(
        candidates[0]["email"],
        "42+CanonicalLogin@users.noreply.github.com"
    );
    assert_eq!(candidates[0]["noreply"], true);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!combined.contains("selected-token"));
}

#[test]
fn profile_mail_text_labels_the_account_and_indents_candidates() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_default_profile(&temp);
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).expect("bin directory");
    write_executable(
        &bin.join("gh"),
        r#"#!/bin/sh
case "$*" in
  "auth token --hostname github.com --user alice") printf 'selected-token\n' ;;
  "api user") printf '%s\n' '{"id":42,"login":"CanonicalLogin"}' ;;
  "api user/emails") printf '%s\n' '[]' ;;
  *) exit 84 ;;
esac
"#,
    );

    let mut mail = isolated_ghis_command();
    mail.args(["profile", "mail", "personal"])
        .env("PATH", prepend_path(&bin));
    for (key, value) in xdg_environment(&temp) {
        mail.env(key, value);
    }
    mail.assert().success().stdout(
        "github.com/alice 的提交邮箱候选：\n  42+CanonicalLogin@users.noreply.github.com（GitHub noreply，已验证）\n",
    );
}

#[test]
fn profile_noreply_rejects_enterprise_hosts_without_contacting_gh() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let bin = temp.path().join("bin");
    let trace = temp.path().join("gh.trace");
    fs::create_dir_all(&bin).expect("bin directory");
    write_executable(
        &bin.join("gh"),
        "#!/bin/sh\nprintf 'unexpected\\n' > \"$FAKE_GH_TRACE\"\nexit 85\n",
    );

    let mut add = isolated_ghis_command();
    add.args([
        "profile",
        "add",
        "enterprise",
        "--host",
        "git.example.test",
        "--login",
        "alice",
        "--name",
        "Alice",
        "-N",
    ])
    .env("PATH", prepend_path(&bin))
    .env("FAKE_GH_TRACE", &trace);
    for (key, value) in xdg_environment(&temp) {
        add.env(key, value);
    }
    add.assert()
        .failure()
        .stderr(predicate::str::contains("只支持 github.com"));
    assert!(!trace.exists());
}

#[test]
fn config_commands_list_and_update_behavior_with_short_keys() {
    let temp = tempfile::tempdir().expect("temporary directory");
    write_default_profile(&temp);

    let mut list = isolated_ghis_command();
    list.args(["config", "list", "--json"]);
    for (key, value) in xdg_environment(&temp) {
        list.env(key, value);
    }
    let output = list.output().expect("list behavior settings");
    assert!(output.status.success());
    let settings: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(settings.as_array().unwrap().len(), 7);

    let mut set = isolated_ghis_command();
    set.args(["config", "set", "display-identity", "never"]);
    for (key, value) in xdg_environment(&temp) {
        set.env(key, value);
    }
    set.assert()
        .success()
        .stdout(predicate::str::contains("behavior.display_identity=never"));

    let mut get = isolated_ghis_command();
    get.args(["config", "get", "behavior.display-identity"]);
    for (key, value) in xdg_environment(&temp) {
        get.env(key, value);
    }
    get.assert().success().stdout("never\n");

    let mut unknown = isolated_ghis_command();
    unknown.args(["config", "get", "missing"]);
    for (key, value) in xdg_environment(&temp) {
        unknown.env(key, value);
    }
    unknown
        .assert()
        .failure()
        .stderr(predicate::str::contains("可用值"));
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

    let mut discover = isolated_ghis_command();
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

    let mut doctor = isolated_ghis_command();
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

    let mut command = isolated_ghis_command();
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

#[test]
fn profile_mutation_never_writes_an_inherited_ghis_config_path() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let outside_config = temp.path().join("sentinel/real-config.toml");
    fs::create_dir_all(outside_config.parent().expect("sentinel parent"))
        .expect("sentinel directory");
    let sentinel = r#"version = 1

[profiles.sentinel]
host = "github.com"
login = "sentinel"
git_name = "Sentinel"
git_email = "sentinel@example.test"
"#;
    fs::write(&outside_config, sentinel).expect("sentinel config");

    let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    command
        .args([
            "profile",
            "add",
            "isolated",
            "--login",
            "isolated-user",
            "--name",
            "Isolated User",
            "--email",
            "isolated@example.test",
        ])
        .env("GHIS_CONFIG", &outside_config)
        .env("GHIS_PROFILE", "sentinel");
    clear_ghis_assert_environment(&mut command);
    for (key, value) in xdg_environment(&temp) {
        command.env(key, value);
    }
    command.assert().success();

    assert_eq!(
        fs::read_to_string(&outside_config).expect("sentinel remains"),
        sentinel
    );
    assert!(
        !outside_config.with_extension("toml.lock").exists(),
        "an inherited config path must not even receive a lock file"
    );
    let isolated_config = temp.path().join("config/ghis/config.toml");
    let config = Config::load(&isolated_config).expect("isolated config");
    assert!(config.profiles.contains_key("isolated"));
}
