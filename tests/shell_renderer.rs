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
            "48254df969bcc44b4d0ab20577122b4849a50809da28860a32d473ebc6b639ef",
        ),
        (
            "bash",
            "f2ae4c46987f89c228aa6d89abdc1c0b43343b1636c85b3bd0a71af08f13542f",
        ),
        (
            "fish",
            "50c57aea39fca2a1dc2a7d8dfca355ecafc304000723a640aa20d024f81cf4b9",
        ),
        (
            "powershell",
            "b160cc926b0a9a596fc82b037288a9d0c034f8317f45c1c6afdc45f25e7ade6b",
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
