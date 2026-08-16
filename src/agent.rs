//! AI coding-agent integration.
//!
//! The module is deliberately independent from the repository context renderer.
//! The pending `agent_context` module only needs to implement [`ContextProvider`].

pub mod claude;
pub mod codex;

#[cfg(unix)]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

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

/// Run a command with inherited stdio while retaining control of its lifecycle.
///
/// Unlike the Unix process runner's `exec` fast path, this waits for the child
/// so the private shim directory is removed before the caller returns.
pub fn launch_session(spec: &CommandSpec) -> Result<i32, AgentError> {
    let mut command = std::process::Command::new(spec.program());
    command.args(spec.arguments());
    for key in spec.removed_environment() {
        command.env_remove(key);
    }
    for (key, value) in spec.environment() {
        command.env(key, value);
    }
    if let Some(directory) = spec.current_directory() {
        command.current_dir(directory);
    }
    let status = command.status().map_err(|source| {
        AgentError::Process(ProcessError::Wait {
            command: spec.display(),
            source,
        })
    })?;
    Ok(status.code().unwrap_or(1))
}

/// A private PATH shim directory which remains available for the child process.
///
/// The directory is removed when this value is dropped. Callers must keep it
/// alive until the launched process has exited.
pub struct SessionShims {
    directory: tempfile::TempDir,
}

impl SessionShims {
    /// Create an isolated shim directory outside the workspace and user cache.
    ///
    /// On Unix, an absolute `TMPDIR` is preferred and `/tmp` is the fallback.
    /// Other platforms fail closed until an equivalent private launcher can be
    /// provided without relying on Unix shell scripts.
    pub fn create(ghis: &Path) -> std::io::Result<Self> {
        create_session_shims(ghis)
    }

    /// Directory prepended to the launched process's PATH.
    pub fn directory(&self) -> &Path {
        self.directory.path()
    }
}

#[cfg(unix)]
fn create_session_shims(ghis: &Path) -> std::io::Result<SessionShims> {
    use std::os::unix::fs::PermissionsExt;

    let directory = create_session_directory()?;
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    write_shim(directory.path(), "git", ghis)?;
    write_shim(directory.path(), "gh", ghis)?;
    Ok(SessionShims { directory })
}

#[cfg(unix)]
fn create_session_directory() -> std::io::Result<tempfile::TempDir> {
    let temporary = std::env::var_os("TMPDIR")
        .filter(|path| Path::new(path).is_absolute())
        .and_then(|path| {
            tempfile::Builder::new()
                .prefix("ghis-agent-")
                .tempdir_in(path)
                .ok()
        });

    match temporary {
        Some(directory) => Ok(directory),
        None => tempfile::Builder::new()
            .prefix("ghis-agent-")
            .tempdir_in("/tmp"),
    }
}

#[cfg(unix)]
fn write_shim(directory: &Path, command: &str, ghis: &Path) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let destination = directory.join(command);
    let contents = format!(
        "#!/bin/sh\nPATH=\"${{GHIS_AGENT_REAL_PATH:-$PATH}}\"; export PATH\nexec {} {} -- \"$@\"\n",
        shell_word(ghis.as_os_str()),
        command,
    );
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(contents.as_bytes())?;
    temporary.as_file_mut().sync_all()?;
    std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700))?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(&destination)
        .map_err(|error| error.error)?;
    std::fs::File::open(directory)?.sync_all()?;
    Ok(destination)
}

#[cfg(not(unix))]
fn create_session_shims(_ghis: &Path) -> std::io::Result<SessionShims> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "private launch shims are unavailable on this platform; refusing to bypass managed commands",
    ))
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

#[cfg(unix)]
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
