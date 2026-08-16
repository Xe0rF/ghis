use assert_cmd::cargo::cargo_bin;
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::process::Command;

fn shell_cli_output(command: &str, shell: &str) -> Vec<u8> {
    let output = Command::new(cargo_bin!("ghis"))
        .args([command, shell])
        .output()
        .expect("run ghis shell renderer");
    assert!(
        output.status.success(),
        "{command} {shell} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[test]
fn init_cli_output_bytes_match_goldens() {
    for (shell, expected) in [
        (
            "zsh",
            "89effa45f19ac819ab3f9c9f86440ba18bb2250bce043451fcf4e23109c8ce0f",
        ),
        (
            "bash",
            "60ecc5a06d6c29cbf236525790ca72365960b5d0e93c832c6dd8a197d5bdd2c1",
        ),
    ] {
        assert_eq!(
            sha256_hex(&shell_cli_output("init", shell)),
            expected,
            "init {shell} bytes changed"
        );
    }
}

#[test]
fn completion_cli_output_bytes_match_goldens() {
    for (shell, expected) in [
        (
            "zsh",
            "20bc1bd29193cfdb2d8bd68ffec2aa9e11e0639511b4f67eedf24f90033c3be3",
        ),
        (
            "bash",
            "91421862974eb22e163803518aae97b7bca1e46c97c36fff9f2e2a25e13ec300",
        ),
        (
            "fish",
            "5a904ccaf9b1f7d3f53155a022477d7ac2b3040e3aa9fd9d2fc7c432af672cb9",
        ),
        (
            "powershell",
            "a7e9dab6a101e2b6bdd9a6abaec3c9a0b2ac17074939c91ab33f713786bcd338",
        ),
    ] {
        assert_eq!(
            sha256_hex(&shell_cli_output("completion", shell)),
            expected,
            "completion {shell} bytes changed"
        );
    }
}

#[cfg(not(windows))]
const COMPLETION_SYNTAX_SHELLS: &[&str] = &["zsh", "bash", "fish"];

#[cfg(windows)]
const COMPLETION_SYNTAX_SHELLS: &[&str] = &["powershell"];

#[test]
fn completion_scripts_parse_when_their_shell_is_available() {
    let directory = tempfile::tempdir().expect("temporary completion directory");
    for shell in COMPLETION_SYNTAX_SHELLS {
        let path = directory.path().join(format!("ghis.{shell}"));
        fs::write(&path, shell_cli_output("completion", shell)).expect("write completion");

        let mut command = match *shell {
            "zsh" => {
                let mut command = Command::new("zsh");
                command.arg("-n");
                command
            }
            "bash" => {
                let mut command = Command::new("bash");
                command.arg("-n");
                command
            }
            "fish" => {
                let mut command = Command::new("fish");
                command.arg("--no-execute");
                command
            }
            "powershell" => {
                let executable = if Command::new("pwsh").arg("--version").output().is_ok() {
                    "pwsh"
                } else {
                    "powershell"
                };
                let mut command = Command::new(executable);
                command.args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "$tokens=$null; $errors=$null; [System.Management.Automation.Language.Parser]::ParseFile($env:GHIS_COMPLETION_PATH, [ref]$tokens, [ref]$errors) > $null; if ($errors.Count -gt 0) { exit 1 }",
                ]);
                command.env("GHIS_COMPLETION_PATH", &path);
                command
            }
            _ => unreachable!("known completion shell"),
        };
        command.arg(&path);

        match command.status() {
            Ok(status) => assert!(status.success(), "{shell} rejected generated completion"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                eprintln!("{shell} syntax check deferred: shell is not installed");
            }
            Err(error) => panic!("run {shell} completion syntax check: {error}"),
        }
    }
}
