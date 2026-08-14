//! OpenAI Codex CLI integration.
//!
//! Context is supplied as developer instructions only. User argv remains argv;
//! ghis never concatenates a user prompt, MCP server, or skill invocation.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use super::{AgentError, AgentKind, ContextProvider, launcher_spec, resolved_context};
use crate::process::{CommandRunner, CommandSpec};

pub const DEVELOPER_INSTRUCTIONS_KEY: &str = "developer_instructions";
pub const SHELL_PATH_KEY: &str = "shell_environment_policy.set.PATH";
pub const DEFAULT_MAX_CONTEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexCapabilities {
    pub available: bool,
    pub developer_instructions: bool,
    pub version: Option<String>,
}

/// Probe the installed Codex CLI. The configuration flag is a stable CLI
/// surface; a successful `--version` establishes that this binary accepts the
/// normal Codex argv parser used by `-c key=value`.
pub fn probe<R: CommandRunner>(runner: &R, program: &OsStr) -> CodexCapabilities {
    let spec = CommandSpec::new(program).arg("--version");
    match runner.run(&spec) {
        Ok(output) if output.success() => CodexCapabilities {
            available: true,
            developer_instructions: true,
            version: Some(output.stdout_string().trim().to_owned()),
        },
        _ => CodexCapabilities {
            available: false,
            developer_instructions: false,
            version: None,
        },
    }
}

/// Build `codex -c developer_instructions=<context> <original argv...>`.
///
/// No `--` separator is inserted: Codex must continue parsing original
/// command-line options such as `--model`, `-C`, and `exec` exactly as the user
/// supplied them.
pub fn run_spec<P: ContextProvider>(
    provider: &P,
    program: impl Into<OsString>,
    user_args: impl IntoIterator<Item = impl Into<OsString>>,
    cwd: &Path,
) -> Result<CommandSpec, AgentError> {
    let context = resolved_context(provider, AgentKind::Codex)?;
    let setting = OsString::from(format!("{DEVELOPER_INSTRUCTIONS_KEY}={context}"));
    let mut args = vec![OsString::from("-c"), setting];
    args.extend(user_args.into_iter().map(Into::into));
    Ok(launcher_spec(program, args, cwd))
}

/// Build a Codex launcher that also pins the shell-tool PATH.
///
/// Codex reconstructs shell environments from configuration before each tool
/// call. Keeping the ghis shim only in the launch process environment is not
/// sufficient when that environment is isolated or rebuilt, so the same PATH
/// is passed as a per-session Codex configuration override.
pub fn run_spec_with_shell_path<P: ContextProvider>(
    provider: &P,
    program: impl Into<OsString>,
    user_args: impl IntoIterator<Item = impl Into<OsString>>,
    cwd: &Path,
    shell_path: &OsStr,
) -> Result<CommandSpec, AgentError> {
    let context = resolved_context(provider, AgentKind::Codex)?;
    let developer_instructions = OsString::from(format!("{DEVELOPER_INSTRUCTIONS_KEY}={context}"));
    let shell_path = toml_edit::Value::from(shell_path.to_string_lossy().as_ref()).to_string();
    let shell_environment_path = OsString::from(format!("{SHELL_PATH_KEY}={shell_path}"));
    let mut args = vec![
        OsString::from("-c"),
        developer_instructions,
        OsString::from("-c"),
        shell_environment_path,
    ];
    args.extend(user_args.into_iter().map(Into::into));
    Ok(launcher_spec(program, args, cwd))
}
