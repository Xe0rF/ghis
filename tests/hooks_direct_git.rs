#![cfg(unix)]

//! Exercise repository integrations from direct Git clients rather than the
//! interactive shell wrapper. Every subprocess uses an isolated XDG namespace.

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

fn ghis_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ghis"))
}

fn git_binary() -> PathBuf {
    let output = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("locate git");
    assert!(output.status.success(), "git is unavailable");
    PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
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
    let mut command = Command::new(git_binary());
    command.current_dir(repository).args(args);
    common_environment(&mut command, root);
    command.output().expect("run direct Git")
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
    repository
}

fn initialize_bare_remote(root: &Path) -> PathBuf {
    let remote = root.join("push remote.git");
    let initialized = run_git(
        root,
        root,
        &[
            "-c",
            "init.defaultBranch=main",
            "init",
            "--bare",
            "-q",
            remote.to_str().expect("remote path"),
        ],
    );
    assert_success(&initialized, "initialize bare remote");
    remote
}

fn canonical_worktree(repository: &Path) -> PathBuf {
    fs::canonicalize(repository).expect("canonical repository path")
}

fn write_config(root: &Path) -> PathBuf {
    let config = root.join("identity config/ghis.toml");
    fs::create_dir_all(config.parent().expect("config parent")).expect("config directory");
    fs::write(
        &config,
        r#"version = 1

[profiles.work]
host = "github.com"
login = "worker"
git_name = "Work Identity"
git_email = "work@example.test"
"#,
    )
    .expect("ghis config");
    config
}

fn bind(root: &Path, repository: &Path, config: &Path, path: Option<&Path>) -> Output {
    let mut command = Command::new(ghis_binary());
    command
        .args(["--config"])
        .arg(config)
        .args(["use", "work", "--repo"])
        .arg(repository);
    common_environment(&mut command, root);
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command.output().expect("bind repository")
}

fn assert_success(output: &Output, action: &str) {
    assert!(
        output.status.success(),
        "{action} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn named_hooks_available() -> bool {
    let output = Command::new(git_binary())
        .args(["help", "--config"])
        .output()
        .expect("inspect Git capabilities");
    output.status.success()
        && String::from_utf8_lossy(&output.stdout).contains("hook.<friendly-name>.command")
}

fn assert_ghis_named_hooks(root: &Path, repository: &Path) {
    for (name, expected_event) in [
        ("ghis-prepare-commit-msg", "prepare-commit-msg"),
        ("ghis-pre-push", "pre-push"),
    ] {
        let event_key = format!("hook.{name}.event");
        let event_output = run_git(
            root,
            repository,
            &["config", "--worktree", "--get", &event_key],
        );
        assert_success(&event_output, "read named hook event");
        assert_eq!(
            String::from_utf8_lossy(&event_output.stdout).trim(),
            expected_event
        );

        let command_key = format!("hook.{name}.command");
        let command_output = run_git(
            root,
            repository,
            &["config", "--worktree", "--get", &command_key],
        );
        assert_success(&command_output, "read named hook command");
        assert!(
            String::from_utf8_lossy(&command_output.stdout)
                .contains(&format!("hook --hook '{expected_event}'")),
            "unexpected named hook command: {}",
            String::from_utf8_lossy(&command_output.stdout)
        );
    }

    for key in ["hook.ghis-pre-commit.command", "hook.ghis-pre-commit.event"] {
        let output = run_git(root, repository, &["config", "--worktree", "--get", key]);
        assert_eq!(
            output.status.code(),
            Some(1),
            "ghis must not register a named pre-commit hook: {key}"
        );
    }
}

fn set_executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write executable");
    let mut permissions = fs::metadata(path)
        .expect("executable metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("make executable");
}

fn shell_quote(value: &Path) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "'\\''"))
}

fn prepend_path(directory: &Path) -> PathBuf {
    let mut entries = vec![directory.to_path_buf()];
    entries.extend(env::split_paths(&env::var_os("PATH").expect("PATH")));
    env::join_paths(entries).expect("build PATH").into()
}

#[test]
fn named_hooks_coexist_with_core_hooks_path_and_run_on_direct_push() {
    if !named_hooks_available() {
        return;
    }

    let temporary = TempDir::new().expect("temporary directory");
    let root = temporary.path();
    let repository = initialize_repository(root);
    let canonical_repository = canonical_worktree(&repository);
    let remote = initialize_bare_remote(root);
    let config = write_config(root);
    let hook_directory = root.join("existing hooks");
    let hook_record = root.join("traditional-hook-record");
    let push_record = root.join("traditional-pre-push-record");
    fs::create_dir_all(&hook_directory).expect("hook directory");
    set_executable(
        &hook_directory.join("prepare-commit-msg"),
        &format!(
            "#!/bin/sh\nprintf 'cwd=%s\\ngit_dir=%s\\ngit_work_tree=%s\\nargs=%s\\n' \\\n  \"$PWD\" \"${{GIT_DIR-}}\" \"${{GIT_WORK_TREE-}}\" \"$*\" > {}\n",
            shell_quote(&hook_record)
        ),
    );
    set_executable(
        &hook_directory.join("pre-push"),
        &format!(
            "#!/bin/sh\n{{\n  printf 'repository=%s\\nremote_name=%s\\nremote_url=%s\\nstdin:\\n' \"$PWD\" \"$1\" \"$2\"\n  cat\n}} > {}\n",
            shell_quote(&push_record)
        ),
    );
    assert_success(
        &run_git(
            root,
            &repository,
            &[
                "config",
                "core.hooksPath",
                hook_directory.to_str().expect("hook path"),
            ],
        ),
        "configure existing hooks path",
    );

    assert_success(&bind(root, &repository, &config, None), "bind repository");

    let configured_path = run_git(root, &repository, &["config", "--get", "core.hooksPath"]);
    assert_success(&configured_path, "read core.hooksPath");
    assert_eq!(
        String::from_utf8_lossy(&configured_path.stdout).trim(),
        hook_directory.to_string_lossy()
    );
    assert_ghis_named_hooks(root, &repository);

    let committed = run_git(
        root,
        &repository,
        &["commit", "--allow-empty", "-m", "named hook integration"],
    );
    assert_success(&committed, "commit through direct Git");
    assert!(
        String::from_utf8_lossy(&committed.stderr).contains("GHIS Profile: work"),
        "named ghis hook did not run: {}",
        String::from_utf8_lossy(&committed.stderr)
    );
    let record = fs::read_to_string(&hook_record).expect("traditional hook record");
    let expected_hook_cwd = format!("cwd={}", canonical_repository.display());
    assert_eq!(
        record.lines().next(),
        Some(expected_hook_cwd.as_str()),
        "traditional hook did not run in the canonical repository working directory: {record}"
    );
    assert!(
        !record.contains("args=\n"),
        "hook did not receive hook arguments: {record}"
    );

    let identity = run_git(root, &repository, &["log", "-1", "--format=%an|%ae"]);
    assert_success(&identity, "read committed identity");
    assert_eq!(
        String::from_utf8_lossy(&identity.stdout).trim(),
        "Work Identity|work@example.test"
    );
    assert_success(
        &run_git(
            root,
            &repository,
            &[
                "remote",
                "add",
                "recording",
                remote.to_str().expect("remote path"),
            ],
        ),
        "configure local bare remote",
    );

    let pushed = run_git(
        root,
        &repository,
        &["push", "recording", "HEAD:refs/heads/integration"],
    );
    assert_eq!(
        pushed.status.code(),
        Some(0),
        "direct absolute git push failed: stdout={} stderr={}",
        String::from_utf8_lossy(&pushed.stdout),
        String::from_utf8_lossy(&pushed.stderr)
    );
    assert!(
        String::from_utf8_lossy(&pushed.stderr).contains("GHIS Profile: work"),
        "named ghis pre-push hook did not run: {}",
        String::from_utf8_lossy(&pushed.stderr)
    );

    let head = run_git(root, &repository, &["rev-parse", "HEAD"]);
    assert_success(&head, "read pushed commit");
    let head = String::from_utf8_lossy(&head.stdout).trim().to_owned();
    let expected_stdin = format!(
        "HEAD {head} refs/heads/integration {}\n",
        "0".repeat(head.len())
    );
    assert_eq!(
        fs::read_to_string(&push_record).expect("traditional pre-push record"),
        format!(
            "repository={}\nremote_name=recording\nremote_url={}\nstdin:\n{expected_stdin}",
            canonical_repository.display(),
            remote.display(),
        )
    );

    let remote_head = run_git(
        root,
        &repository,
        &[
            "--git-dir",
            remote.to_str().expect("remote path"),
            "rev-parse",
            "refs/heads/integration",
        ],
    );
    assert_success(&remote_head, "read pushed bare remote ref");
    assert_eq!(String::from_utf8_lossy(&remote_head.stdout).trim(), head);
}

#[test]
fn husky_style_hooks_path_coexists_with_named_hooks_and_nested_absolute_git() {
    if !named_hooks_available() {
        return;
    }

    let temporary = TempDir::new().expect("temporary directory");
    let root = temporary.path();
    let repository = initialize_repository(root);
    let config = write_config(root);
    let husky_directory = repository.join(".husky");
    let hook_record = root.join("husky-pre-commit-record");
    fs::create_dir_all(&husky_directory).expect("Husky hook directory");
    set_executable(
        &husky_directory.join("pre-commit"),
        &format!(
            "#!/bin/sh\nset -eu\nprintf 'husky-pre-commit\\n' > {record}\nemail=$({git} config --includes --get user.email)\nprintf 'nested_email=%s\\n' \"$email\" >> {record}\n",
            record = shell_quote(&hook_record),
            git = shell_quote(&git_binary()),
        ),
    );
    assert_success(
        &run_git(root, &repository, &["config", "core.hooksPath", ".husky"]),
        "configure Husky-style hooks path",
    );

    assert_success(&bind(root, &repository, &config, None), "bind repository");

    let configured_path = run_git(root, &repository, &["config", "--get", "core.hooksPath"]);
    assert_success(&configured_path, "read Husky core.hooksPath");
    assert_eq!(
        String::from_utf8_lossy(&configured_path.stdout).trim(),
        ".husky"
    );
    assert_ghis_named_hooks(root, &repository);

    let committed = run_git(
        root,
        &repository,
        &["commit", "--allow-empty", "-m", "Husky integration"],
    );
    assert_success(&committed, "commit through Husky-style hooks path");
    assert!(
        String::from_utf8_lossy(&committed.stderr).contains("GHIS Profile: work"),
        "ghis prepare-commit-msg named hook did not run: {}",
        String::from_utf8_lossy(&committed.stderr)
    );
    assert_eq!(
        fs::read_to_string(&hook_record).expect("Husky pre-commit record"),
        "husky-pre-commit\nnested_email=work@example.test\n"
    );
}

#[test]
fn pre_commit_style_legacy_chain_coexists_with_ghis_named_hooks() {
    if !named_hooks_available() {
        return;
    }

    let temporary = TempDir::new().expect("temporary directory");
    let root = temporary.path();
    let repository = initialize_repository(root);
    let canonical_repository = canonical_worktree(&repository);
    let config = write_config(root);
    let hooks_directory = repository.join(".git/hooks");
    let hook_record = root.join("pre-commit-chain-record");
    let legacy_hook = hooks_directory.join("pre-commit.legacy");
    let installed_hook = hooks_directory.join("pre-commit");

    // This mirrors pre-commit's offline legacy-hook contract: the installed
    // runner performs its work, then delegates to the preserved `.legacy` hook.
    set_executable(
        &legacy_hook,
        &format!(
            "#!/bin/sh\nset -eu\ntoplevel=$({git} rev-parse --show-toplevel)\nprintf 'legacy=%s\\n' \"$toplevel\" >> {record}\n",
            git = shell_quote(&git_binary()),
            record = shell_quote(&hook_record),
        ),
    );
    set_executable(
        &installed_hook,
        &format!(
            "#!/bin/sh\nset -eu\nprintf 'runner=pre-commit\\n' > {record}\nlegacy=\"$(dirname \"$0\")/pre-commit.legacy\"\n\"$legacy\" \"$@\"\n",
            record = shell_quote(&hook_record),
        ),
    );
    let installed_before = fs::read_to_string(&installed_hook).expect("installed pre-commit hook");
    let legacy_before = fs::read_to_string(&legacy_hook).expect("legacy pre-commit hook");

    assert_success(&bind(root, &repository, &config, None), "bind repository");
    assert_ghis_named_hooks(root, &repository);
    assert_eq!(
        fs::read_to_string(&installed_hook).expect("retained installed hook"),
        installed_before
    );
    assert_eq!(
        fs::read_to_string(&legacy_hook).expect("retained legacy hook"),
        legacy_before
    );

    let committed = run_git(
        root,
        &repository,
        &["commit", "--allow-empty", "-m", "legacy hook integration"],
    );
    assert_success(&committed, "commit through legacy hook chain");
    assert!(
        String::from_utf8_lossy(&committed.stderr).contains("GHIS Profile: work"),
        "ghis prepare-commit-msg named hook did not run: {}",
        String::from_utf8_lossy(&committed.stderr)
    );
    assert_eq!(
        fs::read_to_string(&hook_record).expect("pre-commit chain record"),
        format!(
            "runner=pre-commit\nlegacy={}\n",
            canonical_repository.display()
        )
    );
}

#[test]
fn editor_and_tui_direct_git_invocations_read_fragment_and_helper() {
    let temporary = TempDir::new().expect("temporary directory");
    let root = temporary.path();
    let repository = initialize_repository(root);
    let canonical_repository = canonical_worktree(&repository);
    let config = write_config(root);
    let bin_directory = root.join("bin");
    let gh_record = root.join("gh-invocation");
    fs::create_dir_all(&bin_directory).expect("bin directory");
    set_executable(
        &bin_directory.join("gh"),
        &format!(
            "#!/bin/sh\nprintf 'cwd=%s\\nargs=%s\\n' \"$PWD\" \"$*\" >> {}\nif [ \"$1 $2 $3 $4 $5 $6\" = \"auth token --hostname github.com --user worker\" ]; then\n  printf 'credential-token\\n'\n  exit 0\nfi\nexit 1\n",
            shell_quote(&gh_record)
        ),
    );
    let path = prepend_path(&bin_directory);

    assert_success(
        &bind(root, &repository, &config, Some(&path)),
        "bind repository",
    );

    // VS Code, JetBrains IDEs, and LazyGit all ultimately launch Git directly.
    // Model only that shared process contract here; this is intentionally not
    // GUI automation or a claim that those products were launched.
    for client in ["VS Code", "JetBrains", "LazyGit"] {
        let fragment_email = run_git(
            root,
            &repository,
            &["config", "--includes", "--get", "user.email"],
        );
        assert_success(
            &fragment_email,
            &format!("{client} direct Git reads included identity"),
        );
        assert_eq!(
            String::from_utf8_lossy(&fragment_email.stdout).trim(),
            "work@example.test",
            "{client} direct Git did not load the ghis fragment"
        );

        let mut credential = Command::new(git_binary());
        credential
            .current_dir(&repository)
            .args(["credential", "fill"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        common_environment(&mut credential, root);
        credential.env("PATH", &path);
        let mut credential = credential
            .spawn()
            .expect("start direct git credential fill");
        use std::io::Write as _;
        credential
            .stdin
            .take()
            .expect("credential stdin")
            .write_all(b"protocol=https\nhost=github.com\nusername=worker\n\n")
            .expect("write credential request");
        let credential = credential
            .wait_with_output()
            .expect("read credential response");
        assert_success(&credential, &format!("{client} direct Git credential fill"));
        let response = String::from_utf8_lossy(&credential.stdout);
        assert!(
            response.contains("username=worker"),
            "{client} credential response omitted username: {response}"
        );
        assert!(
            response.contains("password=credential-token"),
            "{client} credential response omitted password: {response}"
        );
    }

    let one_invocation = format!(
        "cwd={}\nargs=auth token --hostname github.com --user worker\n",
        canonical_repository.display()
    );
    assert_eq!(
        fs::read_to_string(&gh_record).expect("gh invocation record"),
        one_invocation.repeat(3)
    );
}

#[test]
fn missing_named_hook_capability_leaves_existing_hooks_untouched() {
    let temporary = TempDir::new().expect("temporary directory");
    let root = temporary.path();
    let repository = initialize_repository(root);
    let config = write_config(root);
    let hook_directory = root.join("existing hooks");
    let hook = hook_directory.join("prepare-commit-msg");
    let git_shim_directory = root.join("git-shim");
    fs::create_dir_all(&hook_directory).expect("hook directory");
    fs::create_dir_all(&git_shim_directory).expect("git shim directory");
    set_executable(&hook, "#!/bin/sh\nprintf existing-hook\\n");
    assert_success(
        &run_git(
            root,
            &repository,
            &[
                "config",
                "core.hooksPath",
                hook_directory.to_str().expect("hook path"),
            ],
        ),
        "configure existing hooks path",
    );
    let original_hook = fs::read_to_string(&hook).expect("existing hook");
    set_executable(
        &git_shim_directory.join("git"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = help ] && [ \"$2\" = --config ]; then\n  printf 'core.hooksPath\\n'\n  exit 0\nfi\nexec {} \"$@\"\n",
            shell_quote(&git_binary())
        ),
    );
    let path = prepend_path(&git_shim_directory);

    assert_success(
        &bind(root, &repository, &config, Some(&path)),
        "bind without named hook capability",
    );

    let configured_path = run_git(root, &repository, &["config", "--get", "core.hooksPath"]);
    assert_success(&configured_path, "read retained core.hooksPath");
    assert_eq!(
        String::from_utf8_lossy(&configured_path.stdout).trim(),
        hook_directory.to_string_lossy()
    );
    assert_eq!(
        fs::read_to_string(&hook).expect("existing hook"),
        original_hook
    );
    for key in [
        "hook.ghis-prepare-commit-msg.command",
        "hook.ghis-prepare-commit-msg.event",
        "hook.ghis-pre-push.command",
        "hook.ghis-pre-push.event",
    ] {
        let output = run_git(root, &repository, &["config", "--get", key]);
        assert_eq!(
            output.status.code(),
            Some(1),
            "unexpected named hook key: {key}"
        );
    }
}
