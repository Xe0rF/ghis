use assert_cmd::Command as AssertCommand;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn git<I, S>(directory: &Path, arguments: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GHIS_CONFIG")
        .env_remove("GHIS_PROFILE")
        .env_remove("GHIS_BANNER_SHOWN")
        .env_remove("GHIS_WRAPPER_ACTIVE")
        .env_remove("GHIS_BYPASS")
        .env_remove("GHIS_DISABLE_CHPWD")
        .output()
        .expect("run git")
}

fn run_ghis(
    temp: &tempfile::TempDir,
    config: &Path,
    arguments: &[&str],
) -> assert_cmd::assert::Assert {
    let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    command
        .arg("--config")
        .arg(config)
        .args(arguments)
        .env_remove("GHIS_CONFIG")
        .env_remove("GHIS_PROFILE")
        .env_remove("GHIS_BANNER_SHOWN")
        .env_remove("GHIS_WRAPPER_ACTIVE")
        .env_remove("GHIS_BYPASS")
        .env_remove("GHIS_DISABLE_CHPWD")
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join("xdg-config"))
        .env("XDG_CACHE_HOME", temp.path().join("xdg-cache"))
        .env("XDG_STATE_HOME", temp.path().join("xdg-state"));
    command.assert()
}

#[test]
fn main_and_linked_worktrees_keep_all_ghis_git_settings_isolated() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let main = temp.path().join("main");
    let linked = temp.path().join("linked");
    assert!(
        git(temp.path(), ["init", "-q", main.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        git(
            &main,
            [
                "-c",
                "user.name=Bootstrap",
                "-c",
                "user.email=bootstrap@example.test",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "initial",
            ],
        )
        .status
        .success()
    );
    assert!(
        git(
            &main,
            [
                "remote",
                "add",
                "origin",
                "https://github.com/acme/project.git",
            ],
        )
        .status
        .success()
    );
    assert!(
        git(
            &main,
            [
                "worktree",
                "add",
                "-q",
                "-b",
                "linked-branch",
                linked.to_str().unwrap(),
            ],
        )
        .status
        .success()
    );

    let main_config = temp.path().join("main-identities.toml");
    fs::write(
        &main_config,
        r#"version = 1

[profiles.main]
host = "github.com"
login = "main-user"
git_name = "Main Identity"
git_email = "main@example.test"
"#,
    )
    .expect("main config");
    let linked_config = temp.path().join("linked-identities.toml");
    fs::write(
        &linked_config,
        r#"version = 1

[profiles.linked]
host = "github.com"
login = "linked-user"
git_name = "Linked Identity"
git_email = "linked@example.test"
"#,
    )
    .expect("linked config");

    run_ghis(
        &temp,
        &main_config,
        &["use", "main", "--repo", main.to_str().unwrap()],
    )
    .success();
    run_ghis(
        &temp,
        &linked_config,
        &["use", "linked", "--repo", linked.to_str().unwrap()],
    )
    .success();

    let shared = git(
        &main,
        [
            "config",
            "--local",
            "--get-regexp",
            r"^(ghis\.profile|includeIf\.|credential\.https://github\.com\.helper|hook\.ghis-)",
        ],
    );
    assert_eq!(shared.status.code(), Some(1));

    for (worktree, config, profile, email) in [
        (&main, &main_config, "main", "main@example.test"),
        (&linked, &linked_config, "linked", "linked@example.test"),
    ] {
        let marker = git(worktree, ["config", "--worktree", "--get", "ghis.profile"]);
        assert!(marker.status.success());
        assert_eq!(String::from_utf8_lossy(&marker.stdout).trim(), profile);
        let identity = git(worktree, ["config", "--includes", "--get", "user.email"]);
        assert!(identity.status.success());
        assert_eq!(String::from_utf8_lossy(&identity.stdout).trim(), email);
        let helper = git(
            worktree,
            [
                "config",
                "--worktree",
                "--get-all",
                "credential.https://github.com.helper",
            ],
        );
        let helper = String::from_utf8_lossy(&helper.stdout);
        assert!(helper.contains("credential-helper"));
        assert!(helper.contains(config.to_string_lossy().as_ref()));

        if ghis::git::supports_named_hooks() {
            let hook = git(
                worktree,
                [
                    "config",
                    "--worktree",
                    "--get",
                    "hook.ghis-pre-push.command",
                ],
            );
            assert!(hook.status.success());
            assert!(
                String::from_utf8_lossy(&hook.stdout).contains(config.to_string_lossy().as_ref())
            );
        }
    }

    run_ghis(
        &temp,
        &linked_config,
        &["unbind", "--repo", linked.to_str().unwrap()],
    )
    .success();
    let linked_marker = git(&linked, ["config", "--worktree", "--get", "ghis.profile"]);
    assert_eq!(linked_marker.status.code(), Some(1));
    let linked_helper = git(
        &linked,
        [
            "config",
            "--worktree",
            "--get-all",
            "credential.https://github.com.helper",
        ],
    );
    assert_eq!(linked_helper.status.code(), Some(1));
    let main_marker = git(&main, ["config", "--worktree", "--get", "ghis.profile"]);
    assert_eq!(String::from_utf8_lossy(&main_marker.stdout).trim(), "main");
    let main_identity = git(&main, ["config", "--includes", "--get", "user.email"]);
    assert_eq!(
        String::from_utf8_lossy(&main_identity.stdout).trim(),
        "main@example.test"
    );
    let main_helper = git(
        &main,
        [
            "config",
            "--worktree",
            "--get-all",
            "credential.https://github.com.helper",
        ],
    );
    let main_helper = String::from_utf8_lossy(&main_helper.stdout);
    assert!(main_helper.contains("credential-helper"));
    assert!(main_helper.contains(main_config.to_string_lossy().as_ref()));

    if ghis::git::supports_named_hooks() {
        let linked_hook = git(
            &linked,
            [
                "config",
                "--worktree",
                "--get",
                "hook.ghis-pre-push.command",
            ],
        );
        assert_eq!(linked_hook.status.code(), Some(1));
        let main_hook = git(
            &main,
            [
                "config",
                "--worktree",
                "--get",
                "hook.ghis-pre-push.command",
            ],
        );
        assert!(main_hook.status.success());
        assert!(
            String::from_utf8_lossy(&main_hook.stdout)
                .contains(main_config.to_string_lossy().as_ref())
        );
    }
}
