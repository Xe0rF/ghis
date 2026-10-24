#![cfg(unix)]

use assert_cmd::Command as AssertCommand;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    global: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temporary directory");
        let global = temp.path().join("global.gitconfig");
        fs::write(
            &global,
            "[user]\n\tname = User Choice\n\temail = user@example.test\n[init]\n\tdefaultBranch = main\n",
        )
        .expect("global git config");
        Self { temp, global }
    }

    fn ghis(&self) -> AssertCommand {
        let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
        command
            .env("HOME", self.temp.path())
            .env("XDG_CONFIG_HOME", self.temp.path().join("config"))
            .env("XDG_CACHE_HOME", self.temp.path().join("cache"))
            .env("XDG_STATE_HOME", self.temp.path().join("state"))
            .env("GIT_CONFIG_GLOBAL", &self.global)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_SYSTEM")
            .env_remove("GHIS_CONFIG")
            .env_remove("GHIS_PROFILE")
            .env_remove("GHIS_BANNER_SHOWN")
            .env_remove("GHIS_WRAPPER_ACTIVE")
            .env_remove("GHIS_BYPASS")
            .env_remove("GHIS_DISABLE_CHPWD");
        command
    }

    fn baseline(&self) -> PathBuf {
        self.temp.path().join("config/ghis/global.gitconfig")
    }

    fn global_text(&self) -> String {
        fs::read_to_string(&self.global).expect("read global config")
    }

    fn include_count(&self) -> usize {
        self.global_text()
            .lines()
            .filter(|line| line.trim() == format!("path = {}", self.baseline().display()))
            .count()
    }
}

#[test]
fn setup_is_idempotent_and_only_touches_its_own_include() {
    let fixture = Fixture::new();
    fixture
        .ghis()
        .args(["gitconfig", "setup"])
        .assert()
        .success();
    let first = fixture.global_text();
    assert_eq!(fixture.include_count(), 1, "{first}");
    assert!(first.contains("name = User Choice"), "{first}");
    assert!(first.contains("defaultBranch = main"), "{first}");
    let baseline = fs::read_to_string(fixture.baseline()).expect("baseline file");
    assert!(baseline.contains("useConfigOnly = true"), "{baseline}");

    fixture
        .ghis()
        .args(["gitconfig", "setup"])
        .assert()
        .success();
    assert_eq!(
        fixture.global_text(),
        first,
        "a second setup must not append"
    );
}

#[test]
fn setup_collapses_duplicate_entries_for_its_own_path() {
    let fixture = Fixture::new();
    let baseline = fixture.baseline();
    for _ in 0..2 {
        assert!(
            Command::new("git")
                .args([
                    "config",
                    "--global",
                    "--add",
                    "include.path",
                    baseline.to_str().expect("UTF-8 path"),
                ])
                .env("GIT_CONFIG_GLOBAL", &fixture.global)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .expect("git config")
                .success()
        );
    }
    fixture
        .ghis()
        .args(["gitconfig", "setup"])
        .assert()
        .success();
    assert_eq!(fixture.include_count(), 1);
}

#[test]
fn uninstall_removes_only_the_baseline_include() {
    let fixture = Fixture::new();
    fixture
        .ghis()
        .args(["gitconfig", "setup"])
        .assert()
        .success();
    fixture
        .ghis()
        .args(["gitconfig", "uninstall"])
        .assert()
        .success();
    let remaining = fixture.global_text();
    assert_eq!(fixture.include_count(), 0, "{remaining}");
    assert!(remaining.contains("name = User Choice"), "{remaining}");
    assert!(remaining.contains("defaultBranch = main"), "{remaining}");
    assert!(!fixture.baseline().exists());
    // A second uninstall finds nothing and still succeeds.
    fixture
        .ghis()
        .args(["gitconfig", "uninstall"])
        .assert()
        .success();
}

#[test]
fn setup_print_writes_nothing() {
    let fixture = Fixture::new();
    let before = fixture.global_text();
    fixture
        .ghis()
        .args(["gitconfig", "setup", "--print"])
        .assert()
        .success();
    assert_eq!(fixture.global_text(), before);
    assert!(!fixture.baseline().exists());
}

#[test]
fn an_unbound_repository_cannot_commit_with_the_baseline_installed() {
    let fixture = Fixture::new();
    // The baseline stops Git from *inventing* an identity; a global identity
    // the user wrote is a separate finding that doctor reports instead.
    fs::write(&fixture.global, "[init]\n\tdefaultBranch = main\n")
        .expect("global config without identity");
    fixture
        .ghis()
        .args(["gitconfig", "setup"])
        .assert()
        .success();
    let repository = fixture.temp.path().join("repo");
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&repository)
            .env("GIT_CONFIG_GLOBAL", &fixture.global)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .expect("git init")
            .success()
    );
    let commit = Command::new("git")
        .args(["commit", "-q", "--allow-empty", "-m", "unbound"])
        .current_dir(&repository)
        .env("GIT_CONFIG_GLOBAL", &fixture.global)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL")
        .output()
        .expect("git commit");
    assert!(
        !commit.status.success(),
        "an unbound commit must fail: {}",
        String::from_utf8_lossy(&commit.stderr)
    );
    assert!(
        !Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(&repository)
            .env("GIT_CONFIG_GLOBAL", &fixture.global)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git rev-parse")
            .status
            .success(),
        "no commit may be created"
    );
}

#[test]
fn teardown_removes_the_baseline() {
    let fixture = Fixture::new();
    fixture
        .ghis()
        .args(["gitconfig", "setup"])
        .assert()
        .success();
    let repository = fixture.temp.path().join("repo");
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&repository)
            .env("GIT_CONFIG_GLOBAL", &fixture.global)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .expect("git init")
            .success()
    );
    let dry_run = fixture
        .ghis()
        .current_dir(&repository)
        .args(["teardown", "--dry-run"])
        .output()
        .expect("teardown dry run");
    assert!(dry_run.status.success());
    assert!(
        String::from_utf8_lossy(&dry_run.stdout).contains("全局 Git 基线"),
        "{}",
        String::from_utf8_lossy(&dry_run.stdout)
    );
    assert!(fixture.baseline().exists(), "a dry run must not delete");

    fixture
        .ghis()
        .current_dir(&repository)
        .args(["teardown"])
        .assert()
        .success();
    assert_eq!(fixture.include_count(), 0);
    assert!(!fixture.baseline().exists());
    assert!(fixture.global_text().contains("name = User Choice"));
}
