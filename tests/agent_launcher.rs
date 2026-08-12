#![cfg(unix)]

use ghis::agent;
use ghis::process::{CommandRunner, SystemCommandRunner};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
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
    assert_eq!(Path::new(lines[0]), dir.path());
    assert_eq!(
        &lines[1..],
        &["2", "hello world", "semi;colon", "unset", "unset"]
    );
}

#[test]
fn session_shim_is_replaceable() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("target");
    fs::write(&target, "binary").unwrap();
    let shim_dir = dir.path().join("shim");
    let first =
        agent::install_session_shim(&shim_dir, std::ffi::OsStr::new("claude"), &target).unwrap();
    let second =
        agent::install_session_shim(&shim_dir, std::ffi::OsStr::new("claude"), &target).unwrap();
    assert_eq!(first, second);
    assert_eq!(fs::read_link(second).unwrap(), target);
}
