#![cfg(unix)]

use assert_cmd::Command as AssertCommand;
use std::fs;
use std::path::Path;
use std::process::Command;

fn write_config(path: &Path, name: &str, email: &str) {
    fs::create_dir_all(path.parent().expect("config parent")).expect("config directory");
    fs::write(
        path,
        format!(
            r#"version = 1

[profiles.work]
host = "github.com"
login = "worker"
git_name = {name:?}
git_email = {email:?}
"#,
        ),
    )
    .expect("config");
}

fn init_repository(path: &Path) {
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(path)
            .status()
            .expect("git init")
            .success()
    );
}

#[test]
fn same_profile_id_in_two_config_files_keeps_distinct_commit_identities() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let config_a = temp.path().join("identities/a.toml");
    let config_b = temp.path().join("identities/b.toml");
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    write_config(&config_a, "Identity A", "a@example.test");
    write_config(&config_b, "Identity B", "b@example.test");
    init_repository(&repo_a);
    init_repository(&repo_b);

    for (config, repository) in [(&config_a, &repo_a), (&config_b, &repo_b)] {
        AssertCommand::cargo_bin("ghis")
            .expect("ghis binary")
            .args([
                "--config",
                config.to_str().expect("UTF-8 config path"),
                "use",
                "work",
                "--repo",
                repository.to_str().expect("UTF-8 repository path"),
            ])
            .env_remove("GHIS_CONFIG")
            .env_remove("GHIS_PROFILE")
            .env_remove("GHIS_BANNER_SHOWN")
            .env_remove("GHIS_WRAPPER_ACTIVE")
            .env_remove("GHIS_BYPASS")
            .env_remove("GHIS_DISABLE_CHPWD")
            .env("HOME", temp.path())
            .env("XDG_CONFIG_HOME", temp.path().join("xdg-config"))
            .env("XDG_CACHE_HOME", temp.path().join("xdg-cache"))
            .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
            .assert()
            .success();
    }

    // Config B was written last. If fragments are keyed only by Profile ID,
    // both repositories now commit as B and this assertion catches it.
    for (repository, expected) in [
        (&repo_a, "Identity A|a@example.test"),
        (&repo_b, "Identity B|b@example.test"),
    ] {
        assert!(
            Command::new("git")
                .args(["commit", "-q", "--allow-empty", "-m", "identity check"])
                .current_dir(repository)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env_remove("GHIS_CONFIG")
                .env_remove("GHIS_PROFILE")
                .env_remove("GHIS_BANNER_SHOWN")
                .env_remove("GHIS_WRAPPER_ACTIVE")
                .env_remove("GHIS_BYPASS")
                .env_remove("GHIS_DISABLE_CHPWD")
                .env("HOME", temp.path())
                .env("XDG_CONFIG_HOME", temp.path().join("xdg-config"))
                .env("XDG_CACHE_HOME", temp.path().join("xdg-cache"))
                .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
                .status()
                .expect("git commit")
                .success()
        );
        let identity = Command::new("git")
            .args(["log", "-1", "--format=%an|%ae"])
            .current_dir(repository)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GHIS_CONFIG")
            .env_remove("GHIS_PROFILE")
            .env_remove("GHIS_BANNER_SHOWN")
            .env_remove("GHIS_WRAPPER_ACTIVE")
            .env_remove("GHIS_BYPASS")
            .env_remove("GHIS_DISABLE_CHPWD")
            .env("HOME", temp.path())
            .env("XDG_CONFIG_HOME", temp.path().join("xdg-config"))
            .env("XDG_CACHE_HOME", temp.path().join("xdg-cache"))
            .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
            .output()
            .expect("git log");
        assert!(identity.status.success());
        assert_eq!(String::from_utf8_lossy(&identity.stdout).trim(), expected);
    }
}

/// A global `author.*`/`committer.*` value outranks `user.*` in Git's own
/// identity resolution.  The fragment must therefore pin both role sections,
/// not just `[user]`, or a leftover global value silently wins every commit.
#[test]
fn profile_identity_wins_over_global_role_specific_config() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let config = temp.path().join("ghis/config.toml");
    let repository = temp.path().join("repo");
    let global = temp.path().join("global.gitconfig");
    write_config(&config, "Profile Identity", "profile@example.test");
    init_repository(&repository);
    fs::write(
        &global,
        r#"[user]
    name = Global User
    email = global@example.test
[author]
    name = Global Author
    email = global-author@example.test
[committer]
    name = Global Committer
    email = global-committer@example.test
"#,
    )
    .expect("global git config");

    AssertCommand::cargo_bin("ghis")
        .expect("ghis binary")
        .args([
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "use",
            "work",
            "--repo",
            repository.to_str().expect("UTF-8 repository path"),
        ])
        .env("GIT_CONFIG_GLOBAL", &global)
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join("xdg-config"))
        .env("XDG_CACHE_HOME", temp.path().join("xdg-cache"))
        .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .assert()
        .success();

    let output = Command::new("git")
        .args(["var", "GIT_AUTHOR_IDENT"])
        .current_dir(&repository)
        .env("GIT_CONFIG_GLOBAL", &global)
        .env("HOME", temp.path())
        .output()
        .expect("git var");
    assert!(output.status.success());
    let author = String::from_utf8_lossy(&output.stdout);
    assert!(
        author.starts_with("Profile Identity <profile@example.test> "),
        "global author.* must not beat the fragment: {author}"
    );

    let output = Command::new("git")
        .args(["var", "GIT_COMMITTER_IDENT"])
        .current_dir(&repository)
        .env("GIT_CONFIG_GLOBAL", &global)
        .env("HOME", temp.path())
        .output()
        .expect("git var");
    assert!(output.status.success());
    let committer = String::from_utf8_lossy(&output.stdout);
    assert!(
        committer.starts_with("Profile Identity <profile@example.test> "),
        "global committer.* must not beat the fragment: {committer}"
    );
}
