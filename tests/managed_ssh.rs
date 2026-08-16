#![cfg(unix)]

use assert_cmd::Command as AssertCommand;
use assert_cmd::cargo::cargo_bin;
use predicates::prelude::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write executable");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("set executable mode");
}

fn prepend_path(directory: &Path) -> String {
    std::env::join_paths(
        std::iter::once(directory.to_path_buf()).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )),
    )
    .expect("join PATH")
    .to_string_lossy()
    .into_owned()
}

fn xdg_environment(root: &Path) -> [(String, PathBuf); 4] {
    [
        ("HOME".into(), root.join("home")),
        ("XDG_CONFIG_HOME".into(), root.join("xdg-config")),
        ("XDG_CACHE_HOME".into(), root.join("xdg-cache")),
        ("XDG_STATE_HOME".into(), root.join("xdg-state")),
    ]
}

fn apply_environment(command: &mut AssertCommand, root: &Path) {
    for key in [
        "GHIS_CONFIG",
        "GHIS_PROFILE",
        "GHIS_BANNER_SHOWN",
        "GHIS_WRAPPER_ACTIVE",
        "GHIS_BYPASS",
        "GHIS_DISABLE_CHPWD",
    ] {
        command.env_remove(key);
    }
    for (key, value) in xdg_environment(root) {
        command.env(key, value);
    }
}

#[test]
fn managed_ssh_push_overrides_inherited_git_ssh_environment() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let root = temporary.path();
    let fake_bin = root.join("fake-bin");
    let repository = root.join("repository");
    let identity_directory = root.join("managed identity");
    fs::create_dir_all(&fake_bin).expect("fake bin");
    fs::create_dir_all(&repository).expect("repository");
    fs::create_dir_all(&identity_directory).expect("identity directory");

    let public_key = identity_directory.join("work key.pub");
    let agent_socket = identity_directory.join("1password agent.sock");
    let ssh_trace = root.join("ssh.trace");
    let bad_ssh_trace = root.join("bad-ssh.trace");
    let bad_credential_trace = root.join("bad-credential.trace");
    fs::write(&public_key, "ssh-ed25519 AAAATEST work\n").expect("public key");

    executable(
        &fake_bin.join("ssh-add"),
        r#"#!/bin/sh
[ "$SSH_AUTH_SOCK" = "$EXPECTED_AGENT_SOCKET" ] || exit 65
printf '%s\n' 'ssh-ed25519 AAAATEST work'
"#,
    );
    executable(
        &fake_bin.join("ssh-keygen"),
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '256 SHA256:work work (ED25519)'
"#,
    );
    executable(
        &fake_bin.join("ssh"),
        r#"#!/bin/sh
{
  printf 'variant=%s\n' "${GIT_SSH_VARIANT-}"
  for argument do
    printf 'arg=%s\n' "$argument"
  done
} > "$SSH_TRACE"
exit 73
"#,
    );
    let bad_ssh = fake_bin.join("bad-ssh");
    executable(
        &bad_ssh,
        r#"#!/bin/sh
printf 'inherited SSH command ran\n' > "$BAD_SSH_TRACE"
exit 74
"#,
    );
    executable(
        &fake_bin.join("gh"),
        r#"#!/bin/sh
if [ "${1:-} ${2:-}" = "auth token" ]; then
  printf '%s\n' 'selected-token'
  exit 0
fi
if [ "${1:-} ${2:-}" = "repo clone" ]; then
  exec git ls-remote backup
fi
exit 64
"#,
    );
    let bad_credential = fake_bin.join("bad-credential");
    executable(
        &bad_credential,
        r#"#!/bin/sh
printf 'wrong helper ran\n' > "$BAD_CREDENTIAL_TRACE"
printf '%s\n' 'username=wrong-account' 'password=wrong-token' ''
"#,
    );

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
                "-c",
                "user.name=Setup",
                "-c",
                "user.email=setup@example.test",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ])
            .current_dir(&repository)
            .status()
            .expect("initial commit")
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/example/project.git",
            ])
            .current_dir(&repository)
            .status()
            .expect("add primary remote")
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "remote",
                "add",
                "backup",
                "git@github.com:example/project.git",
            ])
            .current_dir(&repository)
            .status()
            .expect("add SSH backup remote")
            .success()
    );

    let config = root.join("config.toml");
    fs::write(
        &config,
        format!(
            r#"version = 1

[behavior]
ssh_unmanaged = "fail"

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
"#,
        ),
    )
    .expect("config");

    let path = prepend_path(&fake_bin);
    let mut bind = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    bind.args([
        "--config",
        config.to_str().expect("UTF-8 config path"),
        "use",
        "work",
        "--repo",
        repository.to_str().expect("UTF-8 repository path"),
    ])
    .env("PATH", &path)
    .env("GIT_CONFIG_GLOBAL", "/dev/null");
    apply_environment(&mut bind, root);
    bind.assert().success();

    let inherited_command = format!("{} --wrong-identity", bad_ssh.display());
    let mut push = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    push.current_dir(&repository)
        .args([
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "git",
            "--",
            "push",
            "backup",
            "HEAD:refs/heads/main",
        ])
        .env("PATH", &path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("EXPECTED_AGENT_SOCKET", &agent_socket)
        .env("SSH_TRACE", &ssh_trace)
        .env("BAD_SSH_TRACE", &bad_ssh_trace)
        .env("GIT_SSH", &bad_ssh)
        .env("GIT_SSH_COMMAND", &inherited_command)
        .env("GIT_SSH_VARIANT", "plink");
    apply_environment(&mut push, root);
    push.assert().failure();

    assert!(ssh_trace.is_file(), "managed ssh command did not run");
    assert!(
        !bad_ssh_trace.exists(),
        "the inherited GIT_SSH_COMMAND bypassed the selected Profile"
    );
    let trace = fs::read_to_string(&ssh_trace).expect("read ssh trace");
    assert!(trace.lines().any(|line| line == "variant=ssh"), "{trace}");
    assert!(
        trace.lines().any(|line| line == "arg=IdentitiesOnly=yes"),
        "{trace}"
    );
    for expected in [
        "arg=-F",
        "arg=/dev/null",
        "arg=BatchMode=yes",
        "arg=ControlMaster=no",
        "arg=ControlPath=none",
    ] {
        assert!(trace.lines().any(|line| line == expected), "{trace}");
    }
    assert!(
        trace
            .lines()
            .any(|line| line == format!("arg=IdentityAgent={}", agent_socket.display())),
        "{trace}"
    );
    assert!(
        trace
            .lines()
            .any(|line| line == format!("arg={}", public_key.display())),
        "{trace}"
    );
    assert!(!trace.contains("wrong-identity"), "{trace}");

    fs::remove_file(&ssh_trace).expect("clear push trace");
    let mut gh_clone = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    gh_clone
        .current_dir(&repository)
        .args([
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "gh",
            "--",
            "repo",
            "clone",
            "example/project",
        ])
        .env("PATH", &path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("SSH_TRACE", &ssh_trace)
        .env("BAD_SSH_TRACE", &bad_ssh_trace)
        .env("GIT_SSH", &bad_ssh)
        .env("GIT_SSH_COMMAND", inherited_command)
        .env("GIT_SSH_VARIANT", "plink");
    apply_environment(&mut gh_clone, root);
    gh_clone.assert().failure();

    assert!(
        ssh_trace.is_file(),
        "a Git command started by gh did not receive managed SSH"
    );
    assert!(
        !bad_ssh_trace.exists(),
        "gh inherited the caller's SSH identity override"
    );
    let trace = fs::read_to_string(&ssh_trace).expect("read gh SSH trace");
    assert!(trace.lines().any(|line| line == "variant=ssh"), "{trace}");
    assert!(
        trace.lines().any(|line| line == "arg=IdentitiesOnly=yes"),
        "{trace}"
    );

    assert!(
        Command::new("git")
            .args([
                "config",
                "--worktree",
                "--unset-all",
                "credential.https://github.com.helper",
            ])
            .current_dir(&repository)
            .status()
            .expect("remove repository helper")
            .success()
    );
    let global_config = root.join("global.gitconfig");
    assert!(
        Command::new("git")
            .args(["config", "--file"])
            .arg(&global_config)
            .arg("credential.helper")
            .arg(format!("!{}", bad_credential.display()))
            .status()
            .expect("install wrong global helper")
            .success()
    );
    let mut explicit_override = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    explicit_override
        .current_dir(&repository)
        .arg("--config")
        .arg(config.to_str().expect("UTF-8 config path"))
        .args(["git", "--", "-c"])
        .arg(format!("credential.helper=!{}", bad_credential.display()))
        .args(["credential", "fill"])
        .write_stdin("protocol=https\nhost=github.com\n\n")
        .env("PATH", &path)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("BAD_CREDENTIAL_TRACE", &bad_credential_trace);
    apply_environment(&mut explicit_override, root);
    explicit_override
        .assert()
        .failure()
        .stderr(predicates::str::contains("credential helper 覆盖"));
    assert!(
        !bad_credential_trace.exists(),
        "an explicit helper ran before ghis rejected it"
    );

    let mut credential = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    credential
        .current_dir(&repository)
        .arg("--config")
        .arg(config.to_str().expect("UTF-8 config path"))
        .args(["git", "--", "credential", "fill"])
        .write_stdin("protocol=https\nhost=github.com\n\n")
        .env("PATH", &path)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("BAD_CREDENTIAL_TRACE", &bad_credential_trace);
    apply_environment(&mut credential, root);
    credential
        .assert()
        .success()
        .stdout(predicates::str::contains("username=worker"))
        .stdout(predicates::str::contains("password=selected-token"))
        .stdout(predicates::str::contains("wrong-token").not());
    assert!(
        !bad_credential_trace.exists(),
        "a caller or global helper replaced the selected Profile"
    );

    fs::write(
        &config,
        format!(
            r#"version = 1

[behavior]
ssh_unmanaged = "fail"

[profiles.work]
host = "github.com"
login = "worker"
git_name = "Work Identity"
git_email = "work@example.test"

[profiles.work.ssh]
mode = "one-password"
public_key = {public_key:?}
fingerprint = "SHA256:work"
"#,
        ),
    )
    .expect("config without an available Agent socket");

    let mut remote_update = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    remote_update
        .current_dir(&repository)
        .args([
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "git",
            "--",
            "remote",
            "update",
            "backup",
        ])
        .env("PATH", &path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("BAD_SSH_TRACE", &bad_ssh_trace)
        .env("GIT_SSH", &bad_ssh)
        .env(
            "GIT_SSH_COMMAND",
            format!("{} --wrong-identity", bad_ssh.display()),
        )
        .env("GIT_SSH_VARIANT", "plink")
        .env_remove("SSH_AUTH_SOCK");
    apply_environment(&mut remote_update, root);
    remote_update.assert().failure();
    assert!(
        !bad_ssh_trace.exists(),
        "an unclassified remote command inherited the caller's SSH identity"
    );
}

fn forwarded_signing_config(
    public_key: &str,
    fingerprint: &str,
    program: &Path,
    other_program: &Path,
    authentication_key: &Path,
    local_agent_socket: &Path,
) -> String {
    format!(
        r#"version = 1

[profiles.work]
host = "github.com"
login = "worker"
git_name = "Work Identity"
git_email = "work@example.test"

[profiles.work.ssh]
mode = "managed"
public_key = {authentication_key:?}
agent_socket = {local_agent_socket:?}

[profiles.work.signing]
enabled = true
transport = "forwarded-agent"
signing_key = {public_key:?}
fingerprint = {fingerprint:?}
program = {program:?}

[profiles.other]
host = "github.com"
login = "other"
git_name = "Other Identity"
git_email = "other@example.test"

[profiles.other.signing]
enabled = true
signing_key = "ssh-ed25519 AAAAOTHER other"
program = {other_program:?}
"#,
    )
}

struct ForwardedSigningTest<'a> {
    proxy_ssh: &'a Path,
    config: &'a Path,
    repository: &'a Path,
    root: &'a Path,
    fake_path: &'a str,
    forwarded_socket: &'a Path,
    inaccessible_socket: &'a Path,
    proxy_trace: &'a Path,
    ssh_add_trace: &'a Path,
    other_signer_trace: &'a Path,
}

fn forwarded_commit_through_proxy(
    test: &ForwardedSigningTest<'_>,
    socket: Option<&Path>,
    agent_key: &str,
    agent_fingerprint: &str,
) -> std::process::Output {
    let mut command = Command::new(test.proxy_ssh);
    command
        .args(["-J", "first-hop,second-hop", "--"])
        .arg(cargo_bin("ghis"))
        .current_dir(test.repository)
        .args([
            "--config",
            test.config.to_str().expect("UTF-8 config path"),
            "git",
            "--",
            "commit",
            "--allow-empty",
            "-m",
            "must not commit",
        ])
        .env("PATH", test.fake_path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("FORWARDED_SOCKET", test.forwarded_socket)
        .env("INACCESSIBLE_SOCKET", test.inaccessible_socket)
        .env("AGENT_KEY", agent_key)
        .env("AGENT_FINGERPRINT", agent_fingerprint)
        .env("PROXY_TRACE", test.proxy_trace)
        .env("SSH_ADD_TRACE", test.ssh_add_trace)
        .env("OTHER_SIGNER_TRACE", test.other_signer_trace);
    if let Some(socket) = socket {
        command.env("SSH_AUTH_SOCK", socket);
    } else {
        command.env_remove("SSH_AUTH_SOCK");
    }
    for key in [
        "GHIS_CONFIG",
        "GHIS_PROFILE",
        "GHIS_BANNER_SHOWN",
        "GHIS_WRAPPER_ACTIVE",
        "GHIS_BYPASS",
        "GHIS_DISABLE_CHPWD",
    ] {
        command.env_remove(key);
    }
    for (key, value) in xdg_environment(test.root) {
        command.env(key, value);
    }
    command.output().expect("run commit through proxy")
}

fn commit_count(repository: &Path) -> String {
    let output = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .current_dir(repository)
        .output()
        .expect("count commits");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("commit count UTF-8")
        .trim()
        .to_owned()
}

#[test]
fn forwarded_signing_fails_closed_across_fake_proxy_hops() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let root = temporary.path();
    let fake_bin = root.join("fake-bin");
    let repository = root.join("repository");
    let config = root.join("config.toml");
    let authentication_key = root.join("authentication.pub");
    let local_agent_socket = root.join("local-agent.sock");
    let forwarded_socket = root.join("forwarded-agent.sock");
    let inaccessible_socket = root.join("inaccessible-agent.sock");
    let signing_program = fake_bin.join("signer");
    let other_signing_program = fake_bin.join("other-signer");
    let proxy_ssh = fake_bin.join("ssh");
    let missing_signer = root.join("missing-signer");
    let proxy_trace = root.join("proxy.trace");
    let ssh_add_trace = root.join("ssh-add.trace");
    let other_signer_trace = root.join("other-signer.trace");
    fs::create_dir_all(&fake_bin).expect("fake bin");
    fs::create_dir_all(&repository).expect("repository");
    fs::write(&authentication_key, "ssh-ed25519 AAAAAUTH authentication\n")
        .expect("authentication key");

    executable(
        &proxy_ssh,
        r#"#!/bin/sh
[ "$1" = "-J" ] || exit 80
printf 'proxyjump=%s\n' "$2" > "$PROXY_TRACE"
shift 2
[ "$1" = "--" ] || exit 81
shift
exec "$@"
"#,
    );
    executable(
        &fake_bin.join("ssh-add"),
        r#"#!/bin/sh
case "$SSH_AUTH_SOCK" in
  "$FORWARDED_SOCKET")
    printf '%s\n' forwarded >> "$SSH_ADD_TRACE"
    printf '%s\n' "$AGENT_KEY"
    ;;
  "$INACCESSIBLE_SOCKET")
    printf '%s\n' inaccessible >> "$SSH_ADD_TRACE"
    printf '%s\n' 'agent access denied' >&2
    exit 2
    ;;
  *)
    printf '%s\n' unexpected >> "$SSH_ADD_TRACE"
    exit 82
    ;;
esac
"#,
    );
    executable(
        &fake_bin.join("ssh-keygen"),
        r#"#!/bin/sh
key=$(cat)
printf '256 %s %s (ED25519)\n' "$AGENT_FINGERPRINT" "$key"
"#,
    );
    executable(&signing_program, "#!/bin/sh\nexit 0\n");
    executable(
        &other_signing_program,
        "#!/bin/sh\nprintf '%s\\n' invoked >> \"$OTHER_SIGNER_TRACE\"\n",
    );

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
                "-c",
                "user.name=Setup",
                "-c",
                "user.email=setup@example.test",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ])
            .current_dir(&repository)
            .status()
            .expect("initial commit")
            .success()
    );

    fs::write(
        &config,
        forwarded_signing_config(
            "ssh-ed25519 AAAAWORK work",
            "SHA256:work",
            &signing_program,
            &other_signing_program,
            &authentication_key,
            &local_agent_socket,
        ),
    )
    .expect("forwarded signing config");
    let fake_path = prepend_path(&fake_bin);
    let mut bind = AssertCommand::cargo_bin("ghis").expect("ghis binary");
    bind.args([
        "--config",
        config.to_str().expect("UTF-8 config path"),
        "use",
        "work",
        "--repo",
        repository.to_str().expect("UTF-8 repository path"),
    ])
    .env("PATH", &fake_path)
    .env("GIT_CONFIG_GLOBAL", "/dev/null");
    apply_environment(&mut bind, root);
    bind.assert().success();

    let forwarded_test = ForwardedSigningTest {
        proxy_ssh: &proxy_ssh,
        config: &config,
        repository: &repository,
        root,
        fake_path: &fake_path,
        forwarded_socket: &forwarded_socket,
        inaccessible_socket: &inaccessible_socket,
        proxy_trace: &proxy_trace,
        ssh_add_trace: &ssh_add_trace,
        other_signer_trace: &other_signer_trace,
    };
    let cases = [
        (
            "missing forwarded socket",
            None,
            "ssh-ed25519 AAAAWORK work",
            "SHA256:work",
            "SSH_AUTH_SOCK in the current process",
            "ssh-ed25519 AAAAWORK work",
            "SHA256:work",
            &signing_program,
            None,
        ),
        (
            "inaccessible forwarded socket",
            Some(inaccessible_socket.as_path()),
            "ssh-ed25519 AAAAWORK work",
            "SHA256:work",
            "SSH agent is unavailable",
            "ssh-ed25519 AAAAWORK work",
            "SHA256:work",
            &signing_program,
            Some("inaccessible\n"),
        ),
        (
            "missing signer",
            Some(forwarded_socket.as_path()),
            "ssh-ed25519 AAAAWORK work",
            "SHA256:work",
            "SSH signing program is unavailable",
            "ssh-ed25519 AAAAWORK work",
            "SHA256:work",
            &missing_signer,
            Some("forwarded\n"),
        ),
        (
            "public key mismatch",
            Some(forwarded_socket.as_path()),
            "ssh-ed25519 AAAADIFFERENT configured",
            "SHA256:work",
            "does not contain the explicitly configured signing key",
            "ssh-ed25519 AAAAWORK work",
            "SHA256:work",
            &signing_program,
            Some("forwarded\n"),
        ),
        (
            "fingerprint mismatch",
            Some(forwarded_socket.as_path()),
            "ssh-ed25519 AAAAWORK work",
            "SHA256:different",
            "does not contain the explicitly configured signing key",
            "ssh-ed25519 AAAAWORK work",
            "SHA256:work",
            &signing_program,
            Some("forwarded\n"),
        ),
    ];

    for (
        case,
        socket,
        configured_key,
        configured_fingerprint,
        expected_error,
        agent_key,
        agent_fingerprint,
        program,
        expected_ssh_add_trace,
    ) in cases
    {
        fs::write(
            &config,
            forwarded_signing_config(
                configured_key,
                configured_fingerprint,
                program,
                &other_signing_program,
                &authentication_key,
                &local_agent_socket,
            ),
        )
        .expect("rewrite forwarded signing config");
        for trace in [&proxy_trace, &ssh_add_trace, &other_signer_trace] {
            if trace.exists() {
                fs::remove_file(trace).expect("clear trace");
            }
        }

        let output =
            forwarded_commit_through_proxy(&forwarded_test, socket, agent_key, agent_fingerprint);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{case}: {stderr}");
        assert!(stderr.contains(expected_error), "{case}: {stderr}");
        assert_eq!(commit_count(&repository), "1", "{case}: {stderr}");
        assert!(
            !other_signer_trace.exists(),
            "{case}: signing fell back to another Profile"
        );
        assert_eq!(
            fs::read_to_string(&proxy_trace).expect("read proxy trace"),
            "proxyjump=first-hop,second-hop\n",
            "{case}"
        );
        match expected_ssh_add_trace {
            Some(expected) => assert_eq!(
                fs::read_to_string(&ssh_add_trace).expect("read ssh-add trace"),
                expected,
                "{case}: signing replaced the forwarded agent socket with the configured local agent"
            ),
            None => assert!(
                !ssh_add_trace.exists(),
                "{case}: signing queried an agent instead of failing before agent access"
            ),
        }
    }
}
