#![cfg(unix)]

//! Verify the boundary between shell-time ghis dispatch and Git's own config.
//! Every command uses a temporary HOME/XDG namespace and local repositories.

use ghis::shell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn git_path() -> PathBuf {
    let output = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("locate git");
    assert!(output.status.success());
    PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
}

fn base_environment(command: &mut Command, temp: &TempDir, global: &Path) {
    command
        .env("HOME", temp.path().join("home"))
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_CACHE_HOME", temp.path().join("cache"))
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", global)
        .env_remove("GIT_CONFIG_NOSYSTEM")
        .env_remove("GHIS_CONFIG")
        .env_remove("GHIS_PROFILE")
        .env_remove("GHIS_BANNER_SHOWN")
        .env_remove("GHIS_WRAPPER_ACTIVE")
        .env_remove("GHIS_BYPASS")
        .env_remove("GHIS_DISABLE_CHPWD");
}

fn run_git(git: &Path, repository: &Path, temp: &TempDir, global: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(git);
    command.current_dir(repository).args(args);
    base_environment(&mut command, temp, global);
    command.output().expect("run git")
}

fn identity(git: &Path, repository: &Path, temp: &TempDir, global: &Path) -> String {
    let output = run_git(
        git,
        repository,
        temp,
        global,
        &["log", "-1", "--format=%an|%ae"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn run_wrapped(
    binary: &Path,
    init_file: &Path,
    repository: &Path,
    temp: &TempDir,
    global: &Path,
    command_text: &str,
) -> Output {
    // zsh -f keeps the test independent from the developer's startup files.
    let script = format!(
        "source \"$1\"\ncd {}\n{}\n",
        shell::shell_quote(&repository.to_string_lossy()),
        command_text
    );
    let mut command = Command::new("zsh");
    command.args(["-f", "-c", &script, "ghis-wrapper-test"]);
    command.arg(init_file);
    base_environment(&mut command, temp, global);
    command.env("PATH", std::env::var_os("PATH").expect("PATH"));
    // The generated script calls this exact binary, so no installed ghis can
    // accidentally participate in the test.
    command.env("GHIS_TEST_BINARY", binary);
    command.output().expect("run zsh wrapper")
}

fn init_repository(git: &Path, repository: &Path, temp: &TempDir, global: &Path) {
    let output = run_git(
        git,
        temp.path(),
        temp,
        global,
        &["init", "-q", repository.to_str().unwrap()],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit(git: &Path, repository: &Path, temp: &TempDir, global: &Path, no_verify: bool) -> Output {
    let mut args = vec!["commit", "--allow-empty", "-m", "identity boundary"];
    if no_verify {
        args.push("--no-verify");
    }
    run_git(git, repository, temp, global, &args)
}

#[test]
fn direct_git_persists_only_repository_config_while_wrapper_and_bypass_are_distinct() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let global = temp.path().join("global.gitconfig");
    fs::create_dir_all(temp.path().join("home")).expect("home");
    fs::write(
        &global,
        "[user]\n\tname = Global Identity\n\temail = global@example.test\n",
    )
    .expect("global config");
    let git = git_path();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_ghis"));
    let plain = temp.path().join("plain");
    let bound = temp.path().join("bound");
    init_repository(&git, &plain, &temp, &global);
    init_repository(&git, &bound, &temp, &global);

    let config = temp.path().join("config/ghis/config.toml");
    fs::create_dir_all(config.parent().expect("config parent")).expect("config directory");
    fs::write(
        &config,
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
    .expect("ghis config");

    // With no wrapper and no repository include, Git sees only its normal
    // global configuration.
    let output = commit(&git, &plain, &temp, &global, true);
    assert!(output.status.success());
    assert_eq!(
        identity(&git, &plain, &temp, &global),
        "Global Identity|global@example.test"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("ghis:"));

    // A repository binding survives wrapper bypass because Git reads the
    // generated include and named hook itself.
    let mut bind = Command::new(&binary);
    bind.args(["use", "work", "--repo", bound.to_str().unwrap()]);
    base_environment(&mut bind, &temp, &global);
    let bind_output = bind.output().expect("bind repository");
    assert!(
        bind_output.status.success(),
        "{}",
        String::from_utf8_lossy(&bind_output.stderr)
    );
    let output = commit(&git, &bound, &temp, &global, true);
    assert!(output.status.success());
    assert_eq!(
        identity(&git, &bound, &temp, &global),
        "Work Identity|work@example.test"
    );

    let init_file = temp.path().join("config/ghis/init.zsh");
    fs::write(
        &init_file,
        shell::zsh_init_script(&binary.to_string_lossy()),
    )
    .expect("init script");

    // A loaded wrapper resolves the default Profile for an otherwise unbound
    // repository and prints its Profile before the commit.
    let output = run_wrapped(
        &binary,
        &init_file,
        &plain,
        &temp,
        &global,
        "git commit --allow-empty --no-verify -m wrapped",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        identity(&git, &plain, &temp, &global),
        "Work Identity|work@example.test"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("GHIS Profile: work"));
    assert!(!stderr.contains("Work Identity"));
    assert!(!stderr.contains("work@example.test"));

    // GHIS_BYPASS skips the shell wrapper for this invocation, so the same
    // unbound repository falls back to Git's global identity.
    let output = run_wrapped(
        &binary,
        &init_file,
        &plain,
        &temp,
        &global,
        "GHIS_BYPASS=1 git commit --allow-empty --no-verify -m bypass",
    );
    assert!(output.status.success());
    assert_eq!(
        identity(&git, &plain, &temp, &global),
        "Global Identity|global@example.test"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("GHIS Profile: work"));

    // `command git` and an absolute path bypass the zsh function too.
    let output = run_wrapped(
        &binary,
        &init_file,
        &plain,
        &temp,
        &global,
        &format!(
            "command git commit --allow-empty --no-verify -m command\n{} commit --allow-empty --no-verify -m absolute",
            shell::shell_quote(&git.to_string_lossy())
        ),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        identity(&git, &plain, &temp, &global),
        "Global Identity|global@example.test"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Work Identity <work@example.test>"));
}
