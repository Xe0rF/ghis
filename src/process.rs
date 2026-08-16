//! Safe, replaceable wrappers around local processes.
//!
//! Every command is assembled as an argv vector.  No shell is involved, which
//! keeps repository paths and user input from becoming shell syntax.  The
//! runner is a small trait so tests can use a fake `git`/`gh` without touching
//! a real credential store or network.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use thiserror::Error;
use zeroize::Zeroizing;

/// Return an explicitly supplied PATH from before an agent shim was installed.
pub(crate) fn configured_agent_real_path() -> Option<OsString> {
    std::env::var_os("GHIS_AGENT_REAL_PATH").filter(|path| !path.is_empty())
}

/// Return the PATH from before an agent shim was installed.
///
/// This falls back to the current PATH for commands outside an agent session.
/// Agent session setup additionally removes recognized stale shim directories
/// from that fallback before recording it for a nested session.
pub(crate) fn agent_real_path() -> Option<OsString> {
    configured_agent_real_path().or_else(|| std::env::var_os("PATH"))
}

/// Construct Git while bypassing an active agent shim when possible.
pub fn git_command() -> Command {
    let mut command = Command::new("git");
    if let Some(path) = agent_real_path() {
        command.env("PATH", path);
    }
    command
}

/// A command that is ready to run without an intervening shell.
#[derive(Clone)]
pub struct CommandSpec {
    program: OsString,
    args: Vec<OsString>,
    environment: BTreeMap<OsString, OsString>,
    removed_environment: Vec<OsString>,
    current_dir: Option<PathBuf>,
    /// Secrets are never included in display strings or error messages.
    secrets: Vec<String>,
}

impl fmt::Debug for CommandSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let environment_keys = self.environment.keys().collect::<Vec<_>>();
        f.debug_struct("CommandSpec")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("environment_keys", &environment_keys)
            .field("removed_environment", &self.removed_environment)
            .field("current_dir", &self.current_dir)
            .field("secret_count", &self.secrets.len())
            .finish()
    }
}

impl CommandSpec {
    /// Start a command with no arguments or environment changes.
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            environment: BTreeMap::new(),
            removed_environment: Vec::new(),
            current_dir: None,
            secrets: Vec::new(),
        }
    }

    /// Start the real Git executable, bypassing an active agent shim.
    pub fn git() -> Self {
        let mut command = Self::new("git");
        if let Some(path) = agent_real_path() {
            command.environment.insert(OsString::from("PATH"), path);
        }
        command
    }

    /// Start the real GitHub CLI, bypassing an active agent shim.
    pub fn gh() -> Self {
        let mut command = Self::new("gh");
        if let Some(path) = agent_real_path() {
            command.environment.insert(OsString::from("PATH"), path);
        }
        command
    }

    /// Add one argument, preserving it as an independent argv element.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Add several arguments.
    pub fn args<I, A>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = A>,
        A: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set a child environment variable.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.environment.insert(key.into(), value.into());
        self
    }

    /// Set an environment variable and mark its value as sensitive.
    pub fn env_secret(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        let value = value.into();
        if let Some(value_string) = value.to_str()
            && !value_string.is_empty()
        {
            self.secrets.push(value_string.to_owned());
        }
        self.environment.insert(key.into(), value);
        self
    }

    /// Remove a variable from the inherited environment.
    pub fn remove_env(mut self, key: impl Into<OsString>) -> Self {
        self.removed_environment.push(key.into());
        self
    }

    /// Remove all GitHub CLI token/host variables that could shadow gh's
    /// keyring account.  The caller can then add a per-process token via
    /// [`Self::env_secret`].
    pub fn clear_github_auth_env(mut self) -> Self {
        for key in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "GITHUB_ENTERPRISE_TOKEN",
            "GH_HOST",
        ] {
            self.removed_environment.push(OsString::from(key));
        }
        self
    }

    /// Set a child working directory.
    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    /// Program path (for diagnostics and tests).
    pub fn program(&self) -> &OsStr {
        &self.program
    }

    /// Arguments, excluding the program path.
    pub fn arguments(&self) -> &[OsString] {
        &self.args
    }

    /// Child environment overrides.
    pub fn environment(&self) -> &BTreeMap<OsString, OsString> {
        &self.environment
    }

    /// Child environment variables removed before launch.
    pub fn removed_environment(&self) -> &[OsString] {
        &self.removed_environment
    }

    /// Child working directory.
    pub fn current_directory(&self) -> Option<&std::path::Path> {
        self.current_dir.as_deref()
    }

    /// Return a redacted human-readable argv string.
    pub fn display(&self) -> String {
        display_argv(
            std::iter::once(self.program.as_os_str())
                .chain(self.args.iter().map(OsString::as_os_str)),
            &Redactor::new(self.secrets.iter().map(String::as_str)),
        )
    }

    fn build(&self, capture: bool) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        for key in &self.removed_environment {
            command.env_remove(key);
        }
        for (key, value) in &self.environment {
            command.env(key, value);
        }
        if let Some(path) = &self.current_dir {
            command.current_dir(path);
        }
        if capture {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        }
        command
    }

    fn secrets(&self) -> impl Iterator<Item = &str> {
        self.secrets.iter().map(String::as_str)
    }
}

/// Captured child-process output.
#[derive(Debug)]
pub struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub duration: Duration,
}

impl CommandOutput {
    /// Whether the process returned a zero exit status.
    pub fn success(&self) -> bool {
        self.status.success()
    }

    /// Numeric exit code, if the platform supplied one.
    pub fn code(&self) -> Option<i32> {
        self.status.code()
    }

    /// Decode stdout as UTF-8, replacing malformed bytes.
    pub fn stdout_string(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Decode stderr as UTF-8, replacing malformed bytes.
    pub fn stderr_string(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// Move stdout into a zeroizing buffer when it contained a token.
    pub fn into_zeroizing_stdout(self) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(self.stdout)
    }
}

/// Errors produced while spawning or checking a command.
#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("could not start `{command}`: {source}")]
    Spawn { command: String, source: io::Error },
    #[error("`{command}` exited with {status}: {stderr}")]
    Failed {
        command: String,
        status: String,
        stderr: String,
    },
    #[error("could not wait for `{command}`: {source}")]
    Wait { command: String, source: io::Error },
}

/// Abstraction used by app, GitHub and credential code.
pub trait CommandRunner: Send + Sync {
    /// Capture stdout/stderr and return even for non-zero statuses.
    fn run(&self, command: &CommandSpec) -> Result<CommandOutput, ProcessError>;

    /// Capture output and turn a non-zero status into [`ProcessError::Failed`].
    fn run_checked(&self, command: &CommandSpec) -> Result<CommandOutput, ProcessError> {
        let output = self.run(command)?;
        if output.success() {
            return Ok(output);
        }
        let redactor = Redactor::new(command.secrets());
        Err(ProcessError::Failed {
            command: redactor.redact(&command.display()),
            status: format_status(output.status),
            stderr: redactor.redact(&output.stderr_string()),
        })
    }

    /// Run with inherited stdio for transparent wrappers (editor, pager and
    /// TTY all remain attached to the user's terminal).
    fn run_passthrough(&self, command: &CommandSpec) -> Result<ExitStatus, ProcessError> {
        let mut child = command.build(false);
        let name = command.display();
        child.status().map_err(|source| ProcessError::Wait {
            command: name,
            source,
        })
    }
}

/// The production runner using `std::process::Command`.
#[derive(Debug, Clone, Default)]
pub struct SystemCommandRunner;

impl SystemCommandRunner {
    /// Construct a production runner.
    pub fn new() -> Self {
        Self
    }
}

impl CommandRunner for SystemCommandRunner {
    fn run(&self, command: &CommandSpec) -> Result<CommandOutput, ProcessError> {
        let display = command.display();
        let started = Instant::now();
        let output = command
            .build(true)
            .output()
            .map_err(|source| ProcessError::Spawn {
                command: display.clone(),
                source,
            })?;
        let redactor = Redactor::new(command.secrets());
        let output = CommandOutput {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
            duration: started.elapsed(),
        };
        // A child may echo an injected token.  Keep captured bytes useful for
        // callers, but make failures/logging safe through the redacted helper.
        if !output.success() {
            let _ = redactor.redact_bytes(&output.stderr);
        }
        // Keep the mutable binding to make it explicit that output ownership
        // belongs to the caller; no logging happens here.
        Ok(output)
    }

    #[cfg(unix)]
    fn run_passthrough(&self, command: &CommandSpec) -> Result<ExitStatus, ProcessError> {
        use std::os::unix::process::CommandExt;

        let mut child = command.build(false);
        let name = command.display();
        let source = child.exec();
        Err(ProcessError::Spawn {
            command: name,
            source,
        })
    }
}

/// Replace every registered secret in text and bytes.
#[derive(Clone, Default)]
pub struct Redactor {
    secrets: Vec<Zeroizing<String>>,
}

impl fmt::Debug for Redactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Redactor")
            .field("secret_count", &self.secrets.len())
            .finish()
    }
}

impl Redactor {
    /// Build a redactor from secret values; empty values are ignored.
    pub fn new<I, S>(secrets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut values = secrets
            .into_iter()
            .map(|secret| secret.as_ref().to_owned())
            .filter(|secret| !secret.is_empty())
            .collect::<Vec<_>>();
        values.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        values.dedup();
        Self {
            secrets: values.into_iter().map(Zeroizing::new).collect(),
        }
    }

    /// Build a redactor containing one secret.
    pub fn one(secret: impl AsRef<str>) -> Self {
        Self::new([secret.as_ref()])
    }

    /// Return text with all secrets replaced by `[REDACTED]`.
    pub fn redact(&self, text: &str) -> String {
        self.secrets.iter().fold(text.to_owned(), |value, secret| {
            value.replace(secret.as_str(), "[REDACTED]")
        })
    }

    /// Redact bytes after lossily decoding UTF-8.  gh/git diagnostics are text
    /// in practice; callers needing exact bytes should keep the raw value out
    /// of logs rather than serializing it.
    pub fn redact_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        self.redact(&String::from_utf8_lossy(bytes)).into_bytes()
    }

    /// Add a secret without exposing it through `Debug`.
    pub fn add(&mut self, secret: impl AsRef<str>) {
        let secret = secret.as_ref();
        if !secret.is_empty() && !self.secrets.iter().any(|item| item.as_str() == secret) {
            self.secrets.push(Zeroizing::new(secret.to_owned()));
            self.secrets
                .sort_by_key(|item| std::cmp::Reverse(item.len()));
        }
    }
}

/// Render argv for diagnostics without invoking a shell.
pub fn display_argv<'a, I>(args: I, redactor: &Redactor) -> String
where
    I: IntoIterator<Item = &'a OsStr>,
{
    args.into_iter()
        .map(|arg| shell_quote(arg, redactor))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(arg: &OsStr, redactor: &Redactor) -> String {
    let value = redactor.redact(&arg.to_string_lossy());
    if value.is_empty() {
        return "''".into();
    }
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_./:@%+-".contains(&byte))
    {
        value
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn format_status(status: ExitStatus) -> String {
    status
        .code()
        .map(|code| format!("exit status {code}"))
        .unwrap_or_else(|| "terminated by signal".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_argv_is_not_shell_expanded() {
        let command = CommandSpec::new("printf")
            .arg("%s")
            .arg("a b; $(touch nope)")
            .clear_github_auth_env();
        let output = SystemCommandRunner::new().run_checked(&command).unwrap();
        assert_eq!(output.stdout_string(), "a b; $(touch nope)");
        assert!(command.display().contains("'a b; $(touch nope)'"));
    }

    #[test]
    fn redaction_hides_token_in_text_and_argv() {
        let redactor = Redactor::one("super-secret-token");
        assert_eq!(
            redactor.redact("token=super-secret-token"),
            "token=[REDACTED]"
        );
        let rendered = display_argv(
            [OsStr::new("gh"), OsStr::new("super-secret-token")],
            &redactor,
        );
        assert!(!rendered.contains("super-secret-token"));
    }

    #[test]
    fn command_debug_never_includes_secret_environment_values() {
        let command = CommandSpec::new("gh")
            .arg("api")
            .env_secret("GH_TOKEN", "debug-secret-token");
        let debug = format!("{command:?}");
        assert!(debug.contains("GH_TOKEN"));
        assert!(debug.contains("secret_count"));
        assert!(!debug.contains("debug-secret-token"));
    }

    #[test]
    fn nonzero_is_checked_without_leaking_secret() {
        let command = CommandSpec::new("sh")
            .arg("-c")
            .arg("printf '%s' \"$GH_TOKEN\" >&2; exit 7")
            .env_secret("GH_TOKEN", "token-value");
        let error = SystemCommandRunner::new()
            .run_checked(&command)
            .unwrap_err();
        let message = error.to_string();
        assert!(!message.contains("token-value"));
        assert!(message.contains("REDACTED"));
    }
}
