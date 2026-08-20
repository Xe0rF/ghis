use assert_cmd::cargo::cargo_bin;
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::process::Command;

fn shell_cli_output(command: &str, shell: &str) -> Vec<u8> {
    let output = Command::new(cargo_bin!("ghis"))
        .args(["shell", command, shell])
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
            "d73ffaf3493e68561014a98e91e72765812d00a5a0d48cdc0a1569e18faebc1b",
        ),
        (
            "bash",
            "35aa42338773e44beb62ab4aed40ae674b096b591c702458211b0103fb9cc1c8",
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
            "f889b60e03d31f533b9f5d2c57107176bc14d0ab629a3f243d9d6dc506d1b816",
        ),
        (
            "bash",
            "0b3ce8b9d97601d28ffc44bd5c795ca6c6212f5b27278fab1a926617e2cd2b8c",
        ),
        (
            "fish",
            "b2416ed8ad316c457409eff2a131d49967b5e50a7eefeaabfaac557b68f71da6",
        ),
        (
            "powershell",
            "2505a888cb6670e8b857a6918d1b4c2c97760dc08bc2448cd52a08d26c976da4",
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
