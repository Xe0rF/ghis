#![cfg(unix)]

use assert_cmd::Command as AssertCommand;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command as ProcessCommand;

fn executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write executable");
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("permissions");
}

fn git_ok(cwd: &Path, args: &[&str]) {
    let output = ProcessCommand::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("run isolated git setup");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn doctor_text_and_json_distinguish_repository_installed_and_loaded_shell_states() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let repository = temp.path().join("repo");
    let fake_bin = temp.path().join("bin");
    let home = temp.path().join("home");
    let config_home = temp.path().join("config");
    let cache_home = temp.path().join("cache");
    let state_home = temp.path().join("state");
    let runtime_home = temp.path().join("runtime");
    let config_file = config_home.join("ghis/config.toml");
    fs::create_dir_all(&fake_bin).expect("fake bin");
    fs::create_dir_all(&home).expect("home");
    fs::create_dir_all(&runtime_home).expect("runtime");
    fs::create_dir_all(config_file.parent().expect("config parent")).expect("config directory");
    fs::write(&config_file, "version = 1\n").expect("ghis config");
    git_ok(
        temp.path(),
        &["init", "-q", repository.to_str().expect("UTF-8 path")],
    );
    git_ok(
        &repository,
        &[
            "config",
            "--local",
            "credential.https://github.com.helper",
            "!'/usr/bin/ghis' --config '/tmp/ghis.toml' credential-helper",
        ],
    );
    executable(
        &fake_bin.join("gh"),
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf '%s\n' 'gh version test'
elif [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{"hosts":{}}'
else
  exit 64
fi
"#,
    );

    let inherited_path = std::env::var_os("PATH").expect("PATH");
    let path = std::env::join_paths(
        std::iter::once(fake_bin).chain(std::env::split_paths(&inherited_path)),
    )
    .expect("joined PATH");
    let run = |json: bool, integration: Option<&str>| {
        let mut command = AssertCommand::cargo_bin("ghis").expect("ghis binary");
        command
            .args(["--config", config_file.to_str().expect("UTF-8 config")])
            .arg("doctor")
            .current_dir(&repository)
            .env("PATH", &path)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &config_home)
            .env("XDG_CACHE_HOME", &cache_home)
            .env("XDG_STATE_HOME", &state_home)
            .env("XDG_RUNTIME_DIR", &runtime_home)
            .env("UID", "999999999")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_CONFIG_NOSYSTEM")
            .env_remove("SSH_AUTH_SOCK")
            .env_remove("GHIS_CONFIG")
            .env_remove("GHIS_PROFILE")
            .env_remove("GHIS_BANNER_SHOWN")
            .env_remove("GHIS_WRAPPER_ACTIVE")
            .env_remove("GHIS_BYPASS")
            .env_remove("GHIS_DISABLE_CHPWD")
            .env_remove("GHIS_SHELL_INTEGRATION");
        if json {
            command.arg("--json");
        }
        if let Some(integration) = integration {
            command.env("GHIS_SHELL_INTEGRATION", integration);
        }
        command.output().expect("run doctor")
    };

    let output = run(true, None);
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).expect("doctor JSON");
    assert_eq!(report["shell_integration"]["state"], "repository_only");
    assert_eq!(report["shell_integration"]["wrapper_loaded"], false);
    assert_eq!(report["shell_integration"]["setup_installed"], false);
    assert_eq!(report["shell_integration"]["repository_bound"], true);

    let output = run(false, None);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("仅检测到仓库本地 ghis 配置"));

    let init_file = config_home.join("ghis/init.zsh");
    ghis::shell::setup(home.join(".zshrc"), &init_file, "ghis").expect("install shell files");
    let output = run(true, None);
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).expect("doctor JSON");
    assert_eq!(report["shell_integration"]["state"], "installed_not_loaded");
    assert_eq!(report["shell_integration"]["setup_installed"], true);
    assert_eq!(report["shell_integration"]["repository_bound"], true);

    let output = run(true, Some("1"));
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).expect("doctor JSON");
    assert_eq!(report["shell_integration"]["state"], "wrapper_loaded");
    assert_eq!(report["shell_integration"]["wrapper_loaded"], true);
}

#[test]
fn doctor_checks_credential_agent_key_and_signing_program_without_leaking_token() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let fake_bin = temp.path().join("bin");
    let config_home = temp.path().join("config");
    let cache_home = temp.path().join("cache");
    let state_home = temp.path().join("state");
    fs::create_dir_all(&fake_bin).expect("fake bin");
    fs::create_dir_all(config_home.join("ghis")).expect("config directory");

    let public_key = temp.path().join("work key.pub");
    fs::write(&public_key, "ssh-ed25519 AAAATEST work\n").expect("public key");
    let agent_socket = temp.path().join("1password agent.sock");
    fs::write(&agent_socket, "socket placeholder").expect("socket placeholder");
    let signing_program = fake_bin.join("op-ssh-sign");
    executable(&signing_program, "#!/bin/sh\nexit 0\n");
    executable(
        &fake_bin.join("gh"),
        r#"#!/bin/sh
case "$1 $2" in
  "auth status")
    printf '%s\n' '{"hosts":{"github.com":[{"host":"github.com","login":"worker","state":"success","active":true}]}}'
    ;;
  "auth token")
    printf '%s\n' 'doctor-secret-token'
    ;;
  *) exit 64 ;;
esac
"#,
    );
    executable(
        &fake_bin.join("ssh-add"),
        &format!(
            "#!/bin/sh\n[ \"$SSH_AUTH_SOCK\" = '{}' ] || exit 65\nprintf '%s\\n' 'ssh-ed25519 AAAATEST work'\n",
            agent_socket.display()
        ),
    );
    executable(
        &fake_bin.join("ssh-keygen"),
        "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '256 SHA256:work work (ED25519)'\n",
    );

    let config_file = config_home.join("ghis/config.toml");
    fs::write(
        &config_file,
        format!(
            r#"version = 1

[profiles.work]
host = "github.com"
login = "worker"
git_name = "Work Identity"
git_email = "work@example.test"

[profiles.work.ssh]
mode = "one-password"
public_key = {public_key:?}
fingerprint = "SHA256:work"
agent_socket = {agent_socket:?}

[profiles.work.signing]
enabled = true
signing_key = {public_key:?}
program = {signing_program:?}
"#,
            public_key = public_key,
            agent_socket = agent_socket,
            signing_program = signing_program,
        ),
    )
    .expect("config");

    let inherited_path = std::env::var_os("PATH").expect("PATH");
    let path = std::env::join_paths(
        std::iter::once(fake_bin.clone()).chain(std::env::split_paths(&inherited_path)),
    )
    .expect("joined PATH");
    let output = AssertCommand::cargo_bin("ghis")
        .expect("ghis binary")
        .args([
            "--config",
            config_file.to_str().expect("UTF-8 config path"),
            "--profile",
            "work",
            "doctor",
            "--json",
        ])
        .current_dir(temp.path())
        .env("PATH", path)
        .env_remove("GHIS_CONFIG")
        .env_remove("GHIS_PROFILE")
        .env_remove("GHIS_BANNER_SHOWN")
        .env_remove("GHIS_WRAPPER_ACTIVE")
        .env_remove("GHIS_BYPASS")
        .env_remove("GHIS_DISABLE_CHPWD")
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_CONFIG_NOSYSTEM")
        .env("GH_TOKEN", "wrong-inherited-token")
        .output()
        .expect("run doctor");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("doctor JSON");
    assert_eq!(report["credential_available"], true);
    assert_eq!(report["ssh_agent"]["source"], "1password");
    assert_eq!(report["ssh_agent"]["available"], true);
    assert_eq!(
        report["ssh_agent"]["selected_key_fingerprint"],
        "SHA256:work"
    );
    assert_eq!(report["signing_program"]["available"], true);
    assert_eq!(report["warnings"], serde_json::json!([]));

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!combined.contains("doctor-secret-token"));
    assert!(!combined.contains("wrong-inherited-token"));
}

#[test]
fn doctor_json_reports_system_and_global_git_conflicts_without_leaking_headers() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let fake_bin = temp.path().join("bin");
    let config_home = temp.path().join("config");
    let cache_home = temp.path().join("cache");
    let state_home = temp.path().join("state");
    let system_config = temp.path().join("system.gitconfig");
    let global_config = temp.path().join("global.gitconfig");
    let ghis_config = config_home.join("ghis/config.toml");
    fs::create_dir_all(&fake_bin).expect("fake bin");
    fs::create_dir_all(ghis_config.parent().expect("config parent")).expect("config directory");

    executable(
        &fake_bin.join("gh"),
        r#"#!/bin/sh
case "$1 $2" in
  "auth status") printf '%s\n' '{"hosts":{}}' ;;
  *) exit 64 ;;
esac
"#,
    );
    fs::write(&ghis_config, "version = 1\n").expect("ghis config");
    fs::write(
        &system_config,
        r#"[credential]
    helper = cache
[http]
    extraHeader = Authorization: Basic system-header-secret
"#,
    )
    .expect("system git config");
    fs::write(
        &global_config,
        r#"[credential]
    helper =
    helper = store
[url "ssh://git@github.com/"]
    insteadOf = https://github.com/
"#,
    )
    .expect("global git config");

    let inherited_path = std::env::var_os("PATH").expect("PATH");
    let path = std::env::join_paths(
        std::iter::once(fake_bin).chain(std::env::split_paths(&inherited_path)),
    )
    .expect("joined PATH");
    let output = AssertCommand::cargo_bin("ghis")
        .expect("ghis binary")
        .args([
            "--config",
            ghis_config.to_str().expect("UTF-8 config path"),
            "doctor",
            "--json",
        ])
        .current_dir(temp.path())
        .env("PATH", path)
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("GIT_CONFIG_SYSTEM", &system_config)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env_remove("GIT_CONFIG_NOSYSTEM")
        .env_remove("GHIS_CONFIG")
        .env_remove("GHIS_PROFILE")
        .env_remove("GHIS_BANNER_SHOWN")
        .env_remove("GHIS_WRAPPER_ACTIVE")
        .env_remove("GHIS_BYPASS")
        .env_remove("GHIS_DISABLE_CHPWD")
        .output()
        .expect("run doctor");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("doctor JSON");
    let diagnostics = report["git_config"]["diagnostics"]
        .as_array()
        .expect("git config diagnostics");
    assert_eq!(report["git_config"]["errors"], 1);
    assert_eq!(report["git_config"]["warnings"], 3);
    assert_eq!(report["git_config"]["info"], 1);

    let helpers = diagnostics
        .iter()
        .filter(|item| item["key"] == "credential.helper")
        .collect::<Vec<_>>();
    assert_eq!(helpers.len(), 3);
    assert!(helpers.iter().any(|item| {
        item["scope"] == "system"
            && item["origin"] == format!("file:{}", system_config.display())
            && item["value"] == "cache"
    }));
    assert!(helpers.iter().any(|item| {
        item["scope"] == "global"
            && item["origin"] == format!("file:{}", global_config.display())
            && item["value"] == "<空值：重置 helper 链>"
    }));
    assert!(helpers.iter().any(|item| {
        item["scope"] == "global"
            && item["origin"] == format!("file:{}", global_config.display())
            && item["value"] == "store"
    }));

    let authorization = diagnostics
        .iter()
        .find(|item| item["key"] == "http.extraheader")
        .expect("Authorization extraHeader diagnostic");
    assert_eq!(authorization["severity"], "error");
    assert_eq!(authorization["scope"], "system");
    assert_eq!(authorization["value"], "<已隐藏>");
    assert_eq!(
        authorization["origin"],
        format!("file:{}", system_config.display())
    );

    let rewrite = diagnostics
        .iter()
        .find(|item| {
            item["key"].as_str().is_some_and(|key| {
                key.starts_with("url.ssh://") && key.ends_with("@github.com/.insteadof")
            })
        })
        .expect("URL rewrite diagnostic");
    assert_eq!(rewrite["severity"], "info");
    assert_eq!(rewrite["scope"], "global");
    assert_eq!(rewrite["value"], "https://github.com/");
    assert_eq!(
        rewrite["origin"],
        format!("file:{}", global_config.display())
    );

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!combined.contains("system-header-secret"));
}

#[test]
fn doctor_json_reports_includeif_local_and_worktree_sources() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let repo = temp.path().join("repo");
    let fake_bin = temp.path().join("bin");
    let config_home = temp.path().join("config");
    let cache_home = temp.path().join("cache");
    let state_home = temp.path().join("state");
    let global_config = temp.path().join("global.gitconfig");
    let included_config = temp.path().join("included.gitconfig");
    let ghis_config = config_home.join("ghis/config.toml");
    fs::create_dir_all(&fake_bin).expect("fake bin");
    fs::create_dir_all(ghis_config.parent().expect("config parent")).expect("config directory");

    git_ok(
        temp.path(),
        &[
            "init",
            "--quiet",
            repo.to_str().expect("UTF-8 repository path"),
        ],
    );
    git_ok(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/repo.git",
        ],
    );
    git_ok(
        &repo,
        &["config", "--local", "user.email", "local@example.test"],
    );
    git_ok(
        &repo,
        &[
            "config",
            "--local",
            "url.ssh://git@github.com/.insteadOf",
            "https://github.com/",
        ],
    );
    git_ok(
        &repo,
        &["config", "--local", "extensions.worktreeConfig", "true"],
    );
    git_ok(
        &repo,
        &[
            "config",
            "--worktree",
            "http.https://github.com/.extraHeader",
            "Authorization: Bearer worktree-header-secret",
        ],
    );

    fs::write(
        &included_config,
        r#"[user]
    name = Included Identity
[credential]
    helper = cache
    helper =
    helper = store
"#,
    )
    .expect("included Git config");
    let git_dir = repo.join(".git");
    let condition = git_dir.to_string_lossy().replace('\\', "/");
    fs::write(
        &global_config,
        format!(
            "[includeIf \"gitdir:{condition}\"]\n\tpath = {}\n",
            included_config.display()
        ),
    )
    .expect("global Git config");
    fs::write(
        &ghis_config,
        r#"version = 1

[profiles.work]
host = "github.com"
login = "worker"
git_name = "Work Identity"
git_email = "work@example.test"
"#,
    )
    .expect("ghis config");
    executable(
        &fake_bin.join("gh"),
        r#"#!/bin/sh
case "$1 $2" in
  "auth status")
    printf '%s\n' '{"hosts":{"github.com":[{"host":"github.com","login":"worker","state":"success","active":true}]}}'
    ;;
  "auth token") printf '%s\n' 'test-token' ;;
  "--version ") printf '%s\n' 'gh version test' ;;
  *) exit 64 ;;
esac
"#,
    );

    let inherited_path = std::env::var_os("PATH").expect("PATH");
    let path = std::env::join_paths(
        std::iter::once(fake_bin).chain(std::env::split_paths(&inherited_path)),
    )
    .expect("joined PATH");
    let output = AssertCommand::cargo_bin("ghis")
        .expect("ghis binary")
        .args([
            "--config",
            ghis_config.to_str().expect("UTF-8 config path"),
            "--profile",
            "work",
            "doctor",
            "--json",
        ])
        .current_dir(&repo)
        .env("PATH", path)
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env_remove("GIT_CONFIG_NOSYSTEM")
        .env_remove("GHIS_CONFIG")
        .env_remove("GHIS_PROFILE")
        .env_remove("GHIS_BANNER_SHOWN")
        .env_remove("GHIS_WRAPPER_ACTIVE")
        .env_remove("GHIS_BYPASS")
        .env_remove("GHIS_DISABLE_CHPWD")
        .output()
        .expect("run doctor");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("doctor JSON");
    let diagnostics = report["git_config"]["diagnostics"]
        .as_array()
        .expect("git config diagnostics");

    let included_name = diagnostics
        .iter()
        .find(|item| item["key"] == "user.name")
        .expect("includeIf user.name diagnostic");
    assert_eq!(included_name["scope"], "global");
    assert_eq!(
        included_name["origin"],
        format!("file:{}", included_config.display())
    );

    let helpers = diagnostics
        .iter()
        .filter(|item| item["key"] == "credential.helper")
        .map(|item| item["value"].as_str().expect("helper value"))
        .collect::<Vec<_>>();
    assert_eq!(helpers, vec!["cache", "<空值：重置 helper 链>", "store"]);

    let local_email = diagnostics
        .iter()
        .find(|item| item["key"] == "user.email")
        .expect("local user.email diagnostic");
    assert_eq!(local_email["scope"], "local");
    assert!(
        local_email["origin"]
            .as_str()
            .is_some_and(|origin| origin.contains("config"))
    );

    let header = diagnostics
        .iter()
        .find(|item| item["key"] == "http.https://github.com/.extraheader")
        .expect("worktree extraHeader diagnostic");
    assert_eq!(header["severity"], "error");
    assert_eq!(header["scope"], "worktree");
    assert_eq!(header["value"], "<已隐藏>");
    assert!(
        header["origin"]
            .as_str()
            .is_some_and(|origin| origin.contains("config.worktree"))
    );

    let rewrite = diagnostics
        .iter()
        .find(|item| item["key"] == "url.ssh://<已隐藏>@github.com/.insteadof")
        .expect("local URL rewrite diagnostic");
    assert_eq!(rewrite["severity"], "warning");
    assert_eq!(rewrite["scope"], "local");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!combined.contains("worktree-header-secret"));
    assert!(!combined.contains("test-token"));
}
