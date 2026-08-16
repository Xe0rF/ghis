#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn ghis_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ghis"))
}

fn common_environment(command: &mut Command, root: &Path) {
    command
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env_remove("GHIS_CONFIG")
        .env_remove("GHIS_PROFILE")
        .env_remove("GHIS_BANNER_SHOWN")
        .env_remove("GHIS_WRAPPER_ACTIVE")
        .env_remove("GHIS_BYPASS")
        .env_remove("GHIS_DISABLE_CHPWD");
}

fn run_git(root: &Path, repository: &Path, args: &[&str]) -> Output {
    let mut command = Command::new("git");
    command.current_dir(repository).args(args);
    common_environment(&mut command, root);
    command.output().expect("run git")
}

fn run_ghis(
    root: &Path,
    repository: &Path,
    config: Option<&Path>,
    profile: Option<&str>,
    args: &[&str],
) -> Output {
    let mut command = Command::new(ghis_binary());
    if let Some(config) = config {
        command.args(["--config", config.to_str().expect("config path")]);
    }
    command.current_dir(repository).arg("git").arg("--");
    command.args(args);
    common_environment(&mut command, root);
    if let Some(profile) = profile {
        command.env("GHIS_PROFILE", profile);
    }
    command.output().expect("run ghis git")
}

fn initialize_repository(root: &Path) -> PathBuf {
    let repository = root.join("repository");
    fs::create_dir_all(&repository).expect("repository directory");
    let initialized = run_git(
        root,
        &repository,
        &["-c", "init.defaultBranch=main", "init", "-q"],
    );
    assert!(
        initialized.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    let committed = run_git(
        root,
        &repository,
        &[
            "-c",
            "user.name=Test Identity",
            "-c",
            "user.email=test@example.test",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    assert!(
        committed.status.success(),
        "initial commit failed: {}",
        String::from_utf8_lossy(&committed.stderr)
    );
    fs::write(repository.join("untracked.txt"), "observable output\n").expect("untracked file");
    repository
}

fn write_config(root: &Path, default_profile: bool) -> PathBuf {
    let directory = root.join("config/ghis");
    fs::create_dir_all(&directory).expect("config directory");
    let behavior = if default_profile {
        "\n[behavior]\ndefault_profile = \"work\"\n"
    } else {
        ""
    };
    let config = directory.join("config.toml");
    fs::write(
        &config,
        format!(
            r#"version = 1
{behavior}
[profiles.work]
host = "github.com"
login = "worker"
git_name = "Work Identity"
git_email = "work@example.test"
"#
        ),
    )
    .expect("ghis config");
    config
}

fn assert_no_ghis_side_effects(root: &Path) {
    assert!(
        !root.join("config/ghis/fragments").exists(),
        "read-only Git command created fragments"
    );
    assert!(
        !root.join("config/ghis/fragments/.lock").exists(),
        "read-only Git command created fragment lock"
    );
    assert!(
        !root.join("config/ghis/config.toml.lock").exists(),
        "read-only Git command created config lock"
    );
    assert!(
        !root.join("cache").exists(),
        "read-only Git command created cache directory"
    );
    assert!(
        !root.join("state").exists(),
        "read-only Git command created state directory"
    );
}

fn assert_output_equivalent(
    root: &Path,
    repository: &Path,
    config: Option<&Path>,
    profile: Option<&str>,
    args: &[&str],
) {
    let expected = run_git(root, repository, args);
    let actual = run_ghis(root, repository, config, profile, args);
    assert_eq!(
        actual.status,
        expected.status,
        "exit status differs for git {args:?}: ghis stderr={} plain stderr={}",
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stderr)
    );
    assert_eq!(
        actual.stdout, expected.stdout,
        "stdout differs for git {args:?}"
    );
    assert_eq!(
        actual.stderr, expected.stderr,
        "stderr differs for git {args:?}"
    );
}

#[test]
fn local_status_and_log_fast_path_preserves_output_without_ghis_writes() {
    for (default_profile, env_profile, bound) in [
        (true, None, false),
        (false, Some("work"), false),
        (false, None, true),
    ] {
        let temporary = TempDir::new().expect("temporary directory");
        let root = temporary.path();
        let repository = initialize_repository(root);
        let config = write_config(root, default_profile);
        if bound {
            let binding = run_git(root, &repository, &["config", "ghis.profile", "work"]);
            assert!(
                binding.status.success(),
                "set repository binding failed: {}",
                String::from_utf8_lossy(&binding.stderr)
            );
        }

        assert_output_equivalent(
            root,
            &repository,
            Some(&config),
            env_profile,
            &["status", "--short"],
        );
        assert_output_equivalent(
            root,
            &repository,
            Some(&config),
            env_profile,
            &["log", "-1", "--oneline", "--decorate"],
        );
        assert_no_ghis_side_effects(root);
    }
}

#[test]
fn mutating_remote_alias_and_unknown_commands_do_not_use_fast_path() {
    let temporary = TempDir::new().expect("temporary directory");
    let root = temporary.path();
    let repository = initialize_repository(root);
    let blocked_config = root.join("config-blocker");
    fs::create_dir(&blocked_config).expect("config blocker directory");

    let commands = [
        vec!["commit", "--allow-empty", "-m", "should-not-run"],
        vec!["push"],
        vec!["fetch"],
        vec!["submodule", "update"],
        vec!["branch", "new-branch"],
        vec!["unknown-command"],
        vec!["-c", "alias.inspect=status", "inspect"],
    ];
    for args in commands {
        let output = run_ghis(root, &repository, Some(&blocked_config), None, &args);
        assert!(
            !output.status.success(),
            "ghis unexpectedly bypassed configuration for git {args:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("I/O error while accessing") && stderr.contains("config-blocker"),
            "git {args:?} did not prove ghis configuration access: {stderr}"
        );
    }
    assert_no_ghis_side_effects(root);
}
