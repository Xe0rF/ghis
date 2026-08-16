#![cfg(unix)]

use ghis::agent;
use ghis::process::{CommandRunner, SystemCommandRunner};
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::tempdir;

#[test]
fn launcher_preserves_argv_cwd_stdio_exit_and_clears_github_tokens() {
    let dir = tempdir().unwrap();
    let script = dir.path().join("probe.sh");
    fs::write(
        &script,
        r#"#!/bin/sh
printf '%s\n' "$PWD" "$#" "$1" "$2" "${GH_TOKEN-unset}" "${GITHUB_TOKEN-unset}" > result
exit 37
"#,
    )
    .unwrap();
    let mut mode = fs::metadata(&script).unwrap().permissions();
    mode.set_mode(0o755);
    fs::set_permissions(&script, mode).unwrap();

    unsafe {
        std::env::set_var("GH_TOKEN", "parent-gh");
        std::env::set_var("GITHUB_TOKEN", "parent-github");
    }
    let spec = agent::launcher_spec(&script, ["hello world", "semi;colon"], dir.path());
    assert_eq!(spec.current_directory(), Some(dir.path()));
    for key in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "GH_HOST",
    ] {
        assert!(
            spec.removed_environment()
                .iter()
                .any(|removed| removed == key)
        );
    }
    let status = SystemCommandRunner::new().run(&spec).unwrap();
    assert_eq!(status.code(), Some(37));
    let result = fs::read_to_string(dir.path().join("result")).unwrap();
    let lines: Vec<_> = result.lines().collect();
    let directory = fs::canonicalize(dir.path()).unwrap();
    assert_eq!(Path::new(lines[0]), directory);
    assert_eq!(
        &lines[1..],
        &["2", "hello world", "semi;colon", "unset", "unset"]
    );
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn prepend_path(directory: &Path) -> OsString {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(
        std::iter::once(directory.to_path_buf()).chain(std::env::split_paths(&inherited)),
    )
    .unwrap()
}

fn write_config(path: &Path) {
    fs::write(
        path,
        r#"version = 1

[behavior]
default_profile = "work"

[profiles.work]
host = "github.com"
login = "worker"
git_name = "Worker"
git_email = "worker@example.test"
"#,
    )
    .unwrap();
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {path:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn internal_git_uses_the_pre_shim_path() {
    let root = tempdir().unwrap();
    let real_bin = root.path().join("real-bin");
    let shim_bin = root.path().join("shim-bin");
    fs::create_dir(&real_bin).unwrap();
    fs::create_dir(&shim_bin).unwrap();
    write_executable(
        &real_bin.join("git"),
        "#!/bin/sh\nprintf 'real-git:%s\\n' \"$*\"\n",
    );
    write_executable(
        &shim_bin.join("git"),
        "#!/bin/sh\nprintf 'shim-reentered\\n' >&2\nexit 91\n",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_ghis"))
        .args(["git", "--", "status", "--short"])
        .current_dir(root.path())
        .env("PATH", &shim_bin)
        .env("GHIS_AGENT_REAL_PATH", &real_bin)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "ghis git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"real-git:status --short\n");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("shim-reentered"));
}

#[test]
fn session_shims_are_private_and_removed_after_the_child_exits() {
    let root = tempdir().unwrap();
    let temporary = root.path().join("temporary");
    let bin = root.path().join("bin");
    let readonly = root.path().join("readonly");
    fs::create_dir_all(&temporary).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&readonly).unwrap();
    for name in ["home", "config", "cache", "state"] {
        let path = readonly.join(name);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();
    }
    let config = root.path().join("config.toml");
    write_config(&config);
    let release = root.path().join("release");
    let launcher = bin.join("codex");
    write_executable(
        &launcher,
        r#"#!/bin/sh
set -eu
directory=${PATH%%:*}
printf '%s\n' "$directory" > "$OBSERVATION"
while [ ! -f "$RELEASE" ]; do sleep 0.01; done
exit 37
"#,
    );

    let binary = PathBuf::from(env!("CARGO_BIN_EXE_ghis"));
    let mut children = Vec::new();
    let mut observations = Vec::new();
    for index in 0..2 {
        let observation = root.path().join(format!("observation-{index}"));
        let mut command = Command::new(&binary);
        command
            .args([
                "--config",
                config.to_str().unwrap(),
                "agent",
                "run",
                "codex",
            ])
            .arg("--")
            .arg("inspect")
            .current_dir(root.path())
            .env("HOME", readonly.join("home"))
            .env("XDG_CONFIG_HOME", readonly.join("config"))
            .env("XDG_CACHE_HOME", readonly.join("cache"))
            .env("XDG_STATE_HOME", readonly.join("state"))
            .env("TMPDIR", &temporary)
            .env("PATH", prepend_path(&bin))
            .env("OBSERVATION", &observation)
            .env("RELEASE", &release)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        children.push(command.spawn().unwrap());
        observations.push(observation);
    }

    for observation in &observations {
        wait_for(observation);
    }
    let directories = observations
        .iter()
        .map(|observation| PathBuf::from(fs::read_to_string(observation).unwrap().trim()))
        .collect::<Vec<_>>();
    assert_ne!(directories[0], directories[1]);
    for directory in &directories {
        assert!(directory.starts_with(&temporary));
        assert_eq!(
            fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for command in ["git", "gh"] {
            let shim = directory.join(command);
            assert_eq!(
                fs::metadata(shim).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    fs::write(&release, "go\n").unwrap();
    for mut child in children {
        assert_eq!(child.wait().unwrap().code(), Some(37));
    }
    let remaining = fs::read_dir(&temporary)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert!(
        remaining.is_empty(),
        "temporary entries remain: {remaining:?}"
    );
}
