//! AI coding-agent integration.
//!
//! The module is deliberately independent from the repository context renderer.
//! The pending `agent_context` module only needs to implement [`ContextProvider`].

pub mod claude;
pub mod codex;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::process::{CommandRunner, CommandSpec, ProcessError};

/// Context target understood by the future `agent_context::render_for(kind)` API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Codex,
}

/// Minimal compatibility boundary for the pending `agent_context` module.
///
/// `require_resolved` must fail closed when no unambiguous repository identity
/// can be selected. `render_for` must return context only, never a user prompt.
pub trait ContextProvider {
    type Error: std::error::Error + Send + Sync + 'static;

    fn require_resolved(&self) -> Result<(), Self::Error>;
    fn render_for(&self, kind: AgentKind) -> Result<String, Self::Error>;
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("agent context is unresolved: {0}")]
    Unresolved(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("could not render agent context: {0}")]
    Context(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error(transparent)]
    Process(#[from] ProcessError),
}

/// Build a transparent launcher command.
///
/// argv is preserved as independent OS strings, cwd and stdio are inherited by
/// the process runner, and GitHub token variables are removed from the child.
pub fn launcher_spec(
    program: impl Into<OsString>,
    args: impl IntoIterator<Item = impl Into<OsString>>,
    cwd: impl AsRef<Path>,
) -> CommandSpec {
    CommandSpec::new(program)
        .args(args)
        .current_dir(cwd.as_ref())
        .clear_github_auth_env()
}

/// Run an agent command with inherited stdio and return its exact exit code.
pub fn launch<R: CommandRunner>(runner: &R, spec: &CommandSpec) -> Result<i32, AgentError> {
    let status = runner.run_passthrough(spec)?;
    Ok(status.code().unwrap_or(1))
}

/// Create a PATH shim directory for one session without editing shell startup
/// files. This is intentionally small so the shell-agent implementation can
/// merge it into its own session environment mechanism.
#[cfg(unix)]
pub fn install_session_shim(
    directory: &Path,
    name: &OsStr,
    target: &Path,
) -> std::io::Result<std::path::PathBuf> {
    use std::os::unix::fs::symlink;

    std::fs::create_dir_all(directory)?;
    let shim = directory.join(name);
    match std::fs::remove_file(&shim) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    symlink(target, &shim)?;
    Ok(shim)
}

#[cfg(not(unix))]
pub fn install_session_shim(
    _directory: &Path,
    _name: &OsStr,
    _target: &Path,
) -> std::io::Result<std::path::PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "session shims are currently supported on Unix only",
    ))
}

pub fn prepare_session_shims(directory: &Path, ghis: &Path) -> std::io::Result<[PathBuf; 2]> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        std::fs::create_dir_all(directory)?;
        let ghis = shell_word(ghis.as_os_str());
        let mut written = Vec::new();
        for command in ["git", "gh"] {
            let path = directory.join(command);
            let temporary = directory.join(format!(".{command}.ghis-tmp-{}", std::process::id()));
            let contents = format!(
                "#!/bin/sh\nPATH=\"${{GHIS_AGENT_REAL_PATH:-$PATH}}\"; export PATH\nexec {ghis} {command} -- \"$@\"\n"
            );
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true).mode(0o700);
            let mut file = options.open(&temporary)?;
            std::io::Write::write_all(&mut file, contents.as_bytes())?;
            file.sync_all()?;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o700))?;
            std::fs::rename(&temporary, &path)?;
            written.push(path);
        }
        Ok([written.remove(0), written.remove(0)])
    }
    #[cfg(not(unix))]
    {
        let _ = (directory, ghis);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "session shims are currently supported on Unix only",
        ))
    }
}

/// Prepend one directory to PATH without changing any existing entries.
pub fn prepend_path(spec: CommandSpec, directory: &Path) -> std::io::Result<CommandSpec> {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let value = std::env::join_paths(
        std::iter::once(directory.to_path_buf()).chain(std::env::split_paths(&inherited)),
    )
    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    Ok(spec
        .env("GHIS_AGENT_REAL_PATH", inherited)
        .env("PATH", value))
}

fn shell_word(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn resolved_context<P: ContextProvider>(
    provider: &P,
    kind: AgentKind,
) -> Result<String, AgentError> {
    provider
        .require_resolved()
        .map_err(|error| AgentError::Unresolved(Box::new(error)))?;
    provider
        .render_for(kind)
        .map_err(|error| AgentError::Context(Box::new(error)))
}
