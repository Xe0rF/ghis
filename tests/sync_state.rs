#![cfg(unix)]

use assert_cmd::Command as AssertCommand;
use ghis::config::ConfigPaths;
use predicates::prelude::*;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn git(directory: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GHIS_CONFIG")
        .env_remove("GHIS_PROFILE")
        .env_remove("GHIS_BANNER_SHOWN")
        .env_remove("GHIS_WRAPPER_ACTIVE")
        .env_remove("GHIS_BYPASS")
        .env_remove("GHIS_DISABLE_CHPWD")
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?}");
}

fn write_config(path: &Path, id: &str, name: &str, email: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!(
            r#"version = 1

[profiles.{id}]
host = "github.com"
login = "{id}"
git_name = "{name}"
git_email = "{email}"
"#
        ),
    )
    .unwrap();
}

fn common_environment(command: &mut AssertCommand, root: &Path) {
    command
        .env_remove("GHIS_CONFIG")
        .env_remove("GHIS_PROFILE")
        .env_remove("GHIS_BANNER_SHOWN")
        .env_remove("GHIS_WRAPPER_ACTIVE")
        .env_remove("GHIS_BYPASS")
        .env_remove("GHIS_DISABLE_CHPWD")
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("xdg-config"))
        .env("XDG_CACHE_HOME", root.join("xdg-cache"))
        .env("XDG_STATE_HOME", root.join("xdg-state"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
}

fn bind(root: &Path, config: &Path, profile: &str, repository: &Path) {
    let mut command = AssertCommand::cargo_bin("ghis").unwrap();
    command
        .args([
            "--config",
            config.to_str().unwrap(),
            "use",
            profile,
            "--repo",
        ])
        .arg(repository);
    common_environment(&mut command, root);
    command.assert().success();
}

#[test]
fn sync_repairs_registered_repositories_and_keeps_custom_configs_isolated() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let config_a = root.join("configs/a.toml");
    let config_b = root.join("configs/b.toml");
    write_config(&config_a, "alpha", "Alpha", "alpha@example.test");
    write_config(&config_b, "beta", "Beta", "beta@example.test");

    let repository_a = root.join("repo-a");
    let repository_b = root.join("repo-b");
    fs::create_dir_all(&repository_a).unwrap();
    fs::create_dir_all(&repository_b).unwrap();
    for repository in [&repository_a, &repository_b] {
        git(repository, &["init", "-q"]);
        git(
            repository,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/example/project.git",
            ],
        );
    }
    bind(root, &config_a, "alpha", &repository_a);
    bind(root, &config_b, "beta", &repository_b);

    let configured_helpers = Command::new("git")
        .args([
            "config",
            "--worktree",
            "--get-all",
            "credential.https://github.com.helper",
        ])
        .current_dir(&repository_a)
        .output()
        .unwrap();
    assert!(configured_helpers.status.success());
    let configured_helpers = String::from_utf8(configured_helpers.stdout).unwrap();
    let mut helper_lines = configured_helpers.lines();
    assert_eq!(helper_lines.next(), Some(""));
    let expected_helper = helper_lines.next().unwrap().to_owned();
    assert_eq!(helper_lines.next(), None);
    git(
        &repository_a,
        &[
            "config",
            "--worktree",
            "--unset-all",
            "credential.https://github.com.helper",
        ],
    );
    git(
        &repository_a,
        &[
            "config",
            "--worktree",
            "--add",
            "credential.https://github.com.helper",
            &expected_helper,
        ],
    );
    let beta_hook_before = Command::new("git")
        .args([
            "config",
            "--worktree",
            "--get",
            "hook.ghis-pre-push.command",
        ])
        .current_dir(&repository_b)
        .output()
        .unwrap();

    let mut sync_a = AssertCommand::cargo_bin("ghis").unwrap();
    sync_a.args(["--config", config_a.to_str().unwrap(), "sync"]);
    common_environment(&mut sync_a, root);
    sync_a
        .assert()
        .success()
        .stdout(predicate::str::contains("检查 1 个已登记仓库，修复 1 个"));

    let repaired_helpers = Command::new("git")
        .args([
            "config",
            "--worktree",
            "--get-all",
            "credential.https://github.com.helper",
        ])
        .current_dir(&repository_a)
        .output()
        .unwrap();
    assert!(repaired_helpers.status.success());
    assert_eq!(
        String::from_utf8_lossy(&repaired_helpers.stdout),
        format!("\n{expected_helper}\n")
    );
    let beta_hook_after = Command::new("git")
        .args([
            "config",
            "--worktree",
            "--get",
            "hook.ghis-pre-push.command",
        ])
        .current_dir(&repository_b)
        .output()
        .unwrap();
    assert_eq!(beta_hook_before.stdout, beta_hook_after.stdout);

    git(
        &repository_b,
        &["config", "--worktree", "--unset-all", "ghis.profile"],
    );
    let mut sync_b = AssertCommand::cargo_bin("ghis").unwrap();
    sync_b.args(["--config", config_b.to_str().unwrap(), "sync"]);
    common_environment(&mut sync_b, root);
    sync_b
        .assert()
        .success()
        .stdout(predicate::str::contains("清理 1 条失效记录"));

    let mut paths_a = ConfigPaths::from_bases(
        root.join("xdg-config"),
        root.join("xdg-cache"),
        root.join("xdg-state"),
    );
    paths_a.set_config_file(&config_a).unwrap();
    let mut paths_b = ConfigPaths::from_bases(
        root.join("xdg-config"),
        root.join("xdg-cache"),
        root.join("xdg-state"),
    );
    paths_b.set_config_file(&config_b).unwrap();

    let registry_path = &paths_a.repositories_file;
    let registry: Value = serde_json::from_slice(&fs::read(registry_path).unwrap()).unwrap();
    let records = registry["repositories"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["profile"], "alpha");
    let raw = fs::read_to_string(registry_path).unwrap();
    assert!(!raw.contains("beta@example.test"));
    assert!(!raw.to_ascii_lowercase().contains("token"));
    assert_eq!(
        fs::metadata(registry_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let beta_registry: Value =
        serde_json::from_slice(&fs::read(&paths_b.repositories_file).unwrap()).unwrap();
    assert_eq!(
        beta_registry["repositories"].as_array().unwrap().len(),
        0,
        "清理 beta 记录不能影响 alpha 配置的 state"
    );

    let log_path = &paths_a.log_file;
    let log = fs::read_to_string(log_path).unwrap();
    assert!(log.contains("sync-repair"));
    assert!(!log.contains("alpha@example.test"));
    assert!(!log.to_ascii_lowercase().contains("token"));
    assert_eq!(
        fs::metadata(log_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
