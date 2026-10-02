//! Resolving a credential token from a profile-supplied command.
//!
//! `Command` mode lets a profile name any other forge while keeping the
//! fail-closed contract that `Manage` mode gets from `gh`.  The profile stores
//! an argv, never a shell string: `git.rs` does not feed profile data to
//! `sh -c`, and this module keeps that rule.
//!
//! The command's stdout is the token and nothing else.  ghis pairs it with the
//! profile's credential username, so the command cannot become a channel for
//! anything but the secret.  A command that fails, or that prints nothing,
//! produces an error; the caller turns that into `quit=true` so Git stops
//! asking rather than falling through to an interactive prompt.

use crate::process::{CommandRunner, CommandSpec, ProcessError, SystemCommandRunner};
use crate::secret::SecretToken;
use std::fmt;

#[derive(Debug)]
pub enum CommandError {
    /// The profile has no argv, which validation should already have rejected.
    Empty,
    Spawn {
        command: String,
        source: std::io::Error,
    },
    Failed {
        command: String,
        status: String,
        stderr: String,
    },
    /// The command succeeded but printed no token.
    EmptyOutput { command: String },
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "credential_command is not configured"),
            Self::Spawn { command, source } => {
                write!(f, "could not start `{command}`: {source}")
            }
            Self::Failed {
                command,
                status,
                stderr,
            } => write!(f, "`{command}` exited with {status}: {stderr}"),
            Self::EmptyOutput { command } => {
                write!(f, "`{command}` printed no token on stdout")
            }
        }
    }
}

impl std::error::Error for CommandError {}

impl From<ProcessError> for CommandError {
    fn from(error: ProcessError) -> Self {
        match error {
            ProcessError::Spawn { command, source } => Self::Spawn { command, source },
            ProcessError::Failed {
                command,
                status,
                stderr,
            } => Self::Failed {
                command,
                status,
                stderr,
            },
            ProcessError::Wait { command, source } => Self::Spawn { command, source },
        }
    }
}

/// Build the argv for a profile's `credential_command`.
///
/// The inherited GitHub CLI variables are removed first.  Without that a
/// `GH_TOKEN` in the user's shell would silently replace whatever the command
/// was asked to produce.
fn spec_for(argv: &[String]) -> Option<CommandSpec> {
    let (program, arguments) = argv.split_first()?;
    let mut command = CommandSpec::new(program).clear_github_auth_env();
    command = command.args(arguments.iter());
    Some(command)
}

/// Run `argv` and return its trimmed stdout as the token.
pub fn run(argv: &[String]) -> Result<SecretToken, CommandError> {
    run_with(argv, &SystemCommandRunner::new())
}

pub fn run_with<R: CommandRunner>(
    argv: &[String],
    runner: &R,
) -> Result<SecretToken, CommandError> {
    let Some(command) = spec_for(argv) else {
        return Err(CommandError::Empty);
    };
    let display = command.display();
    let output = runner.run(&command).map_err(CommandError::from)?;
    if !output.success() {
        return Err(CommandError::Failed {
            command: display,
            status: match output.code() {
                Some(code) => format!("exit code {code}"),
                None => output.status.to_string(),
            },
            stderr: output.stderr_string(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let token = stdout.trim();
    if token.is_empty() {
        return Err(CommandError::EmptyOutput { command: display });
    }
    Ok(SecretToken::new(token.as_bytes().to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_argv_is_an_error_rather_than_a_lookup() {
        assert!(matches!(
            run_with(&[], &crate::process::SystemCommandRunner::new()),
            Err(CommandError::Empty)
        ));
        assert!(spec_for(&[]).is_none());
    }

    #[test]
    fn spec_keeps_arguments_separate_and_clears_github_tokens() {
        let spec = spec_for(&[
            "op".into(),
            "read".into(),
            "op://Private/GitLab/work/credential".into(),
        ])
        .expect("program present");
        assert_eq!(
            spec.arguments(),
            [
                std::ffi::OsString::from("read"),
                std::ffi::OsString::from("op://Private/GitLab/work/credential")
            ]
        );
        assert!(
            spec.removed_environment()
                .iter()
                .any(|key| key == "GH_TOKEN"),
            "an inherited GH_TOKEN would replace the command's output"
        );
    }

    /// Write a stub that emits `stdout` and `stderr`, then exits with `status`,
    /// and return the argv that runs it.
    ///
    /// Windows cannot execute a shebang script, so the stub is a `.cmd` file
    /// launched through `cmd /c`; both spellings emit the same bytes. Callers
    /// keep the argv opaque so the credential path stays argv-only here too.
    /// Payloads must stay free of quotes and `%`, which neither shell reads
    /// literally out of the assignment.
    fn stub(dir: &std::path::Path, stdout: &str, stderr: &str, status: i32) -> Vec<String> {
        #[cfg(unix)]
        let (name, body) = (
            "stub.sh",
            format!(
                "#!/bin/sh\nprintf '%s\\n' '{stdout}'\nprintf '%s' '{stderr}' >&2\nexit {status}\n"
            ),
        );
        #[cfg(windows)]
        let (name, body) = (
            "stub.cmd",
            format!(
                "@echo off\r\n<nul set /p \"={stdout}\"\r\n<nul set /p \"={stderr}\" 1>&2\r\nexit /b {status}\r\n"
            ),
        );
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let program = path.to_string_lossy().into_owned();
        // Windows cannot run the stub as a bare program, so it goes through
        // the interpreter the way a user's own `credential_command` would.
        #[cfg(windows)]
        let argv = vec!["cmd".to_string(), "/c".to_string(), program];
        #[cfg(unix)]
        let argv = vec![program];
        argv
    }

    #[test]
    fn stdout_is_trimmed_before_becoming_the_token() {
        let dir = tempfile::tempdir().expect("temp dir");
        let token = run(&stub(dir.path(), "  secret-token", "", 0)).expect("token");
        assert_eq!(token.as_str(), Some("secret-token"));
    }

    #[test]
    fn a_failing_command_reports_its_status_and_stderr() {
        let dir = tempfile::tempdir().expect("temp dir");
        let error = run(&stub(dir.path(), "", "boom", 7)).expect_err("must fail");
        let rendered = error.to_string();
        assert!(rendered.contains("boom"), "{rendered}");
        assert!(!rendered.contains("secret"), "{rendered}");
    }

    #[test]
    fn silent_output_is_not_a_valid_token() {
        let dir = tempfile::tempdir().expect("temp dir");
        let error = run(&stub(dir.path(), "", "", 0)).expect_err("must fail");
        assert!(matches!(error, CommandError::EmptyOutput { .. }), "{error}");
    }
}
