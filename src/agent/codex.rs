//! OpenAI Codex CLI integration.
//!
//! Context is supplied as developer instructions only. User argv remains argv;
//! ghis never concatenates a user prompt, MCP server, or skill invocation.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use super::{AgentError, AgentKind, ContextProvider, launcher_spec, resolved_context};
use crate::process::{CommandRunner, CommandSpec};

pub const DEVELOPER_INSTRUCTIONS_KEY: &str = "developer_instructions";
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
