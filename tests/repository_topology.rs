#![cfg(unix)]

//! Repository-topology policy coverage using isolated repositories and fake
//! network-facing GitHub/Git commands. No test contacts a remote service.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn ghis_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ghis"))
}

fn real_git() -> PathBuf {
    let output = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("locate git");
    assert!(output.status.success());
    PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
}

fn isolated_environment(command: &mut Command, root: &Path) {
    command
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GHIS_CONFIG")
        .env_remove("GHIS_PROFILE")
        .env_remove("GHIS_BANNER_SHOWN")
        .env_remove("GHIS_WRAPPER_ACTIVE")
        .env_remove("GHIS_BYPASS")
        .env_remove("GHIS_DISABLE_CHPWD");
}

fn run_git(root: &Path, repository: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(real_git());
    command.current_dir(repository).args(args);
    isolated_environment(&mut command, root);
    command.output().expect("run git")
}

fn require_git(root: &Path, repository: &Path, args: &[&str]) {
    let output = run_git(root, repository, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_ghis(root: &Path, repository: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(ghis_binary());
    command.current_dir(repository).args(args);
    isolated_environment(&mut command, root);
    command.output().expect("run ghis")
}

fn require_ghis(root: &Path, repository: &Path, args: &[&str]) -> Output {
    let output = run_ghis(root, repository, args);
    assert!(
        output.status.success(),
        "ghis {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn initialize_repository(root: &Path, name: &str) -> PathBuf {
    let repository = root.join(name);
    fs::create_dir_all(&repository).expect("repository directory");
    require_git(root, &repository, &["init", "-q"]);
    repository
}

fn commit_files(root: &Path, repository: &Path) {
    require_git(root, repository, &["add", "."]);
    require_git(
        root,
        repository,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-qm",
            "fixture",
        ],
    );
}

fn write_profiles(root: &Path, ssh_unmanaged: Option<&str>) {
    let config = root.join("config/ghis/config.toml");
    fs::create_dir_all(config.parent().expect("config parent")).expect("config directory");
    let behavior = ssh_unmanaged
        .map(|policy| format!("[behavior]\nssh_unmanaged = \"{policy}\"\n\n"))
        .unwrap_or_default();
    fs::write(
        config,
        format!(
            r#"version = 1

{behavior}[profiles.parent]
host = "github.com"
login = "parent"
git_name = "Parent Identity"
git_email = "parent@example.test"

[profiles.module]
host = "github.com"
login = "module"
git_name = "Module Identity"
git_email = "module@example.test"

[profiles.monorepo]
host = "github.com"
login = "monorepo"
git_name = "Monorepo Identity"
git_email = "monorepo@example.test"

[profiles.sparse]
host = "github.com"
login = "sparse"
git_name = "Sparse Identity"
git_email = "sparse@example.test"

[profiles.public]
host = "github.com"
login = "public"
git_name = "Public Identity"
git_email = "public@example.test"

[profiles.enterprise]
host = "enterprise.example"
login = "enterprise"
git_name = "Enterprise Identity"
git_email = "enterprise@example.test"
"#
        ),
    )
    .expect("ghis config");
}

fn bind(root: &Path, repository: &Path, profile: &str) {
    require_ghis(
        root,
        repository,
        &[
            "use",
            profile,
            "--repo",
            repository.to_str().expect("repository path"),
        ],
    );
}

fn profile_name(root: &Path, repository: &Path) -> String {
    let output = require_ghis(root, repository, &["git", "--", "config", "user.name"]);
    String::from_utf8(output.stdout)
        .expect("profile name is UTF-8")
        .trim()
        .to_owned()
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write executable");
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("permissions");
}

fn prepend_path(directory: &Path) -> OsString {
    std::env::join_paths(
        std::iter::once(directory.to_path_buf()).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )),
    )
    .expect("PATH")
}

#[test]
fn submodule_uses_its_own_repository_binding() {
    let temporary = TempDir::new().expect("temporary directory");
    let root = temporary.path();
    write_profiles(root, None);
    let parent = initialize_repository(root, "parent");
    let module_source = initialize_repository(root, "module-source");
    fs::write(module_source.join("module.txt"), "module\n").expect("module file");
    commit_files(root, &module_source);

    require_git(
        root,
        &parent,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            module_source.to_str().expect("module source"),
            "deps/module",
        ],
    );
    let module = parent.join("deps/module");
    bind(root, &parent, "parent");
    bind(root, &module, "module");

    assert_eq!(profile_name(root, &parent), "Parent Identity");
    assert_eq!(profile_name(root, &module), "Module Identity");
}

#[test]
fn subtree_directory_inherits_its_parent_repository_binding() {
    let temporary = TempDir::new().expect("temporary directory");
    let root = temporary.path();
    write_profiles(root, None);
    let repository = initialize_repository(root, "parent");
    let subtree = repository.join("vendor/component/src");
    fs::create_dir_all(&subtree).expect("subtree directory");
    fs::write(subtree.join("library.rs"), "pub fn component() {}\n").expect("subtree file");
    bind(root, &repository, "parent");

    assert_eq!(profile_name(root, &subtree), "Parent Identity");
}

#[test]
fn monorepo_binding_applies_to_each_package_directory() {
    let temporary = TempDir::new().expect("temporary directory");
    let root = temporary.path();
    write_profiles(root, None);
    let repository = initialize_repository(root, "monorepo");
    let api = repository.join("packages/api");
    let web = repository.join("packages/web");
    fs::create_dir_all(&api).expect("api package");
    fs::create_dir_all(&web).expect("web package");
    bind(root, &repository, "monorepo");

    assert_eq!(profile_name(root, &api), "Monorepo Identity");
    assert_eq!(profile_name(root, &web), "Monorepo Identity");
}

#[test]
fn sparse_checkout_keeps_one_binding_regardless_of_checked_out_path() {
    let temporary = TempDir::new().expect("temporary directory");
    let root = temporary.path();
    write_profiles(root, None);
    let repository = initialize_repository(root, "sparse");
    fs::create_dir_all(repository.join("app")).expect("app directory");
    fs::create_dir_all(repository.join("docs")).expect("docs directory");
    fs::write(repository.join("app/main.rs"), "fn main() {}\n").expect("app file");
    fs::write(repository.join("docs/readme.md"), "docs\n").expect("docs file");
    commit_files(root, &repository);
    require_git(root, &repository, &["sparse-checkout", "init", "--cone"]);
    require_git(root, &repository, &["sparse-checkout", "set", "app"]);
    bind(root, &repository, "sparse");

    assert_eq!(profile_name(root, &repository), "Sparse Identity");
    assert_eq!(
        profile_name(root, &repository.join("app")),
        "Sparse Identity"
    );
    assert!(!repository.join("docs").exists(), "fixture is sparse");
}

#[test]
fn remote_fetch_pushurl_and_multiple_remote_boundaries_stay_separate() {
    let temporary = TempDir::new().expect("temporary directory");
    let root = temporary.path();
    write_profiles(root, Some("fail"));
    let repository = initialize_repository(root, "remote-boundaries");
    require_git(
        root,
        &repository,
        &["remote", "add", "origin", "git@github.com:acme/project.git"],
    );
    require_git(
        root,
        &repository,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            "https://github.com/acme/project.git",
        ],
    );
    require_git(
        root,
        &repository,
        &[
            "remote",
            "add",
            "upstream",
            "https://enterprise.example/acme/project.git",
        ],
    );
    bind(root, &repository, "public");

    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("fake bin");
    let git_trace = root.join("git.trace");
    let gh_trace = root.join("gh.trace");
    write_executable(
        &bin.join("git"),
        r#"#!/bin/sh
for argument in "$@"; do
  case "$argument" in
    push|fetch)
      printf '%s\n' "$argument" >> "$FAKE_GIT_TRACE"
      exit 0
      ;;
  esac
done
exec "$GHIS_REAL_GIT" "$@"
"#,
    );
    write_executable(
        &bin.join("gh"),
        r#"#!/bin/sh
case "$*" in
  'auth token --hostname github.com --user public') printf 'public-token\n'; exit 0 ;;
  'auth token --hostname enterprise.example --user enterprise') printf 'enterprise-token\n'; exit 0 ;;
esac
[ "$GH_HOST" = enterprise.example ] || exit 90
selected_fetch=$("$GHIS_REAL_GIT" remote get-url upstream)
[ "$selected_fetch" = "https://$GH_HOST/acme/project.git" ] || exit 91
printf 'host=%s selected_fetch=upstream:%s token=%s args=%s\n' "$GH_HOST" "$selected_fetch" "${GH_ENTERPRISE_TOKEN-}" "$*" >> "$FAKE_GH_TRACE"
"#,
    );

    let mut push = Command::new(ghis_binary());
    push.current_dir(&repository)
        .args(["git", "--", "push", "origin"])
        .env("PATH", prepend_path(&bin))
        .env("GHIS_REAL_GIT", real_git())
        .env("FAKE_GIT_TRACE", &git_trace);
    isolated_environment(&mut push, root);
    let push = push.output().expect("run fake git push");
    assert!(
        push.status.success(),
        "push should use origin pushurl, not its SSH fetch URL: {}",
        String::from_utf8_lossy(&push.stderr)
    );
    assert_eq!(fs::read_to_string(&git_trace).expect("git trace"), "push\n");

    let mut fetch = Command::new(ghis_binary());
    fetch
        .current_dir(&repository)
        .args(["git", "--", "fetch", "origin"])
        .env("PATH", prepend_path(&bin))
        .env("GHIS_REAL_GIT", real_git())
        .env("FAKE_GIT_TRACE", &git_trace);
    isolated_environment(&mut fetch, root);
    let fetch = fetch.output().expect("run fake git fetch");
    assert!(!fetch.status.success(), "SSH fetch must be stopped");
    assert!(
        String::from_utf8_lossy(&fetch.stderr).contains("已阻止 SSH 操作"),
        "unexpected fetch error: {}",
        String::from_utf8_lossy(&fetch.stderr)
    );
    assert_eq!(fs::read_to_string(&git_trace).expect("git trace"), "push\n");

    let mut github = Command::new(ghis_binary());
    github
        .current_dir(&repository)
        .args(["--profile", "enterprise", "gh", "--", "pr", "list"])
        .env("PATH", prepend_path(&bin))
        .env("GHIS_REAL_GIT", real_git())
        .env("FAKE_GH_TRACE", &gh_trace);
    isolated_environment(&mut github, root);
    let github = github.output().expect("run fake gh");
    assert!(
        github.status.success(),
        "gh should select the matching fetch remote: {}",
        String::from_utf8_lossy(&github.stderr)
    );
    assert_eq!(
        fs::read_to_string(&gh_trace).expect("gh trace"),
        "host=enterprise.example selected_fetch=upstream:https://enterprise.example/acme/project.git token=enterprise-token args=pr list\n"
    );
}

#[test]
fn enterprise_gh_without_matching_fetch_remote_stops_before_gh() {
    let temporary = TempDir::new().expect("temporary directory");
    let root = temporary.path();
    write_profiles(root, None);
    let repository = initialize_repository(root, "missing-enterprise-remote");
    require_git(
        root,
        &repository,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/project.git",
        ],
    );
    bind(root, &repository, "public");

    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("fake bin");
    let gh_trace = root.join("gh.trace");
    write_executable(
        &bin.join("gh"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_GH_TRACE"
exit 92
"#,
    );

    let mut github = Command::new(ghis_binary());
    github
        .current_dir(&repository)
        .args(["--profile", "enterprise", "gh", "--", "pr", "list"])
        .env("PATH", prepend_path(&bin))
        .env("FAKE_GH_TRACE", &gh_trace);
    isolated_environment(&mut github, root);
    let github = github.output().expect("run gh without matching remote");
    assert!(
        !github.status.success(),
        "missing enterprise remote must stop"
    );
    assert!(
        String::from_utf8_lossy(&github.stderr).contains("当前仓库 remote"),
        "unexpected gh error: {}",
        String::from_utf8_lossy(&github.stderr)
    );
    assert!(
        !gh_trace.exists(),
        "gh must not be invoked before rejecting the missing matching remote"
    );
}
