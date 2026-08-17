//! AI coding-agent integration.
//!
//! The module is deliberately independent from the repository context renderer.
//! The pending `agent_context` module only needs to implement [`ContextProvider`].

pub mod claude;
pub mod codex;

use std::ffi::{OsStr, OsString};
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
    /// On Windows, native copies of `ghis.exe` are materialized as `git.exe`
    /// and `gh.exe`; the binary dispatches them without a command shell.
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
    let real_path = session_real_path()?;
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    write_shim(directory.path(), "git", ghis, &real_path)?;
    write_shim(directory.path(), "gh", ghis, &real_path)?;
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
fn write_shim(
    directory: &Path,
    command: &str,
    ghis: &Path,
    real_path: &OsStr,
) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let destination = directory.join(command);
    let mut contents = b"#!/bin/sh\n# ghis-agent-shim\nPATH=".to_vec();
    push_shell_word(&mut contents, real_path);
    contents.extend_from_slice(b"; export PATH\nexec ");
    push_shell_word(&mut contents, ghis.as_os_str());
    contents.push(b' ');
    contents.extend_from_slice(command.as_bytes());
    contents.extend_from_slice(b" -- \"$@\"\n");
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(&contents)?;
    temporary.as_file_mut().sync_all()?;
    std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700))?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(&destination)
        .map_err(|error| error.error)?;
    std::fs::File::open(directory)?.sync_all()?;
    Ok(destination)
}

#[cfg(windows)]
fn create_session_shims(ghis: &Path) -> std::io::Result<SessionShims> {
    let ghis = ghis.canonicalize()?;
    if !ghis.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Windows agent launcher executable does not exist",
        ));
    }

    let directory = tempfile::Builder::new().prefix("ghis-agent-").tempdir()?;
    for command in ["git.exe", "gh.exe"] {
        let destination = directory.path().join(command);
        std::fs::copy(&ghis, &destination)?;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&destination)?
            .sync_all()?;
    }
    Ok(SessionShims { directory })
}

/// Return the managed command selected by a native Windows agent shim.
///
/// The control environment is mandatory so merely renaming `ghis.exe` cannot
/// accidentally turn a normal invocation into an agent shim. Arguments are
/// still collected with `args_os` by the entrypoint and never joined into a
/// command string.
#[cfg(windows)]
pub fn windows_native_shim_command() -> Option<&'static str> {
    crate::process::configured_agent_real_path()?;
    let executable = std::env::current_exe().ok()?;
    let file_name = executable.file_name()?.to_string_lossy();
    if file_name.eq_ignore_ascii_case("git.exe") {
        Some("git")
    } else if file_name.eq_ignore_ascii_case("gh.exe") {
        Some("gh")
    } else {
        None
    }
}

#[cfg(not(any(unix, windows)))]
fn create_session_shims(_ghis: &Path) -> std::io::Result<SessionShims> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "private launch shims require an argv-transparent native launcher on this platform; refusing to bypass managed commands",
    ))
}

/// Prepend one directory to PATH without changing any existing entries.
pub fn prepend_path(spec: CommandSpec, directory: &Path) -> std::io::Result<CommandSpec> {
    let inherited = session_real_path()?;
    let value = session_path_from(directory, &inherited)?;
    Ok(spec
        .env("GHIS_AGENT_REAL_PATH", inherited)
        .env("PATH", value))
}

/// Return the session PATH with the ghis shim directory ahead of real entries.
pub fn session_path(directory: &Path) -> std::io::Result<OsString> {
    let inherited = session_real_path()?;
    session_path_from(directory, &inherited)
}

fn session_path_from(directory: &Path, inherited: &OsStr) -> std::io::Result<OsString> {
    std::env::join_paths(
        std::iter::once(directory.to_path_buf()).chain(std::env::split_paths(inherited)),
    )
    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
}

#[cfg(unix)]
fn session_real_path() -> std::io::Result<OsString> {
    session_real_path_from(
        crate::process::configured_agent_real_path(),
        std::env::var_os("PATH"),
    )
}

#[cfg(unix)]
fn session_real_path_from(
    configured: Option<OsString>,
    inherited: Option<OsString>,
) -> std::io::Result<OsString> {
    let mut paths = Vec::new();
    for path in inherited
        .iter()
        .flat_map(|path| std::env::split_paths(path))
        .filter(|path| !is_agent_shim_directory(path))
        .chain(
            configured
                .iter()
                .flat_map(|path| std::env::split_paths(path)),
        )
    {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    std::env::join_paths(paths)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
}

#[cfg(unix)]
fn is_agent_shim_directory(path: &Path) -> bool {
    ["git", "gh"].iter().all(|command| {
        let Ok(contents) = std::fs::read(path.join(command)) else {
            return false;
        };
        contents.starts_with(b"#!/bin/sh\n# ghis-agent-shim\n")
            || (contents.starts_with(b"#!/bin/sh\nPATH=\"${GHIS_AGENT_REAL_PATH:-$PATH}\"")
                && contents
                    .windows(b"\nexec ".len())
                    .any(|part| part == b"\nexec "))
    })
}

#[cfg(not(unix))]
fn session_real_path() -> std::io::Result<OsString> {
    Ok(crate::process::agent_real_path().unwrap_or_default())
}

#[cfg(unix)]
fn push_shell_word(output: &mut Vec<u8>, value: &OsStr) {
    use std::os::unix::ffi::OsStrExt;

    output.push(b'\'');
    for byte in value.as_bytes() {
        if *byte == b'\'' {
            output.extend_from_slice(b"'\\''");
        } else {
            output.push(*byte);
        }
    }
    output.push(b'\'');
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[cfg(not(target_os = "macos"))]
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    fn executable(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn nested_session_path_removes_recognized_old_and_new_shims() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("legacy");
        let current = root.path().join("current");
        let real = root.path().join("real");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::create_dir(&current).unwrap();
        std::fs::create_dir(&real).unwrap();

        for command in ["git", "gh"] {
            std::fs::write(
                legacy.join(command),
                format!(
                    "#!/bin/sh\nPATH=\"${{GHIS_AGENT_REAL_PATH:-$PATH}}\"; export PATH\nexec ghis {command} -- \"$@\"\n"
                ),
            )
            .unwrap();
        }
        let ghis = root.path().join("ghis");
        executable(&ghis, "#!/bin/sh\nexit 0\n");
        for command in ["git", "gh"] {
            write_shim(&current, command, &ghis, real.as_os_str()).unwrap();
        }

        let inherited = std::env::join_paths([legacy, current, real.clone()]).unwrap();
        assert_eq!(
            session_real_path_from(None, Some(inherited)).unwrap(),
            std::env::join_paths([real]).unwrap()
        );
    }

    #[test]
    fn session_path_preserves_entries_added_after_real_path_was_recorded() {
        let root = tempfile::tempdir().unwrap();
        let codex = root.path().join("codex-path");
        let shim = root.path().join("shim");
        let real = root.path().join("real");
        std::fs::create_dir(&codex).unwrap();
        std::fs::create_dir(&shim).unwrap();
        std::fs::create_dir(&real).unwrap();

        for command in ["git", "gh"] {
            std::fs::write(
                shim.join(command),
                format!(
                    "#!/bin/sh\nPATH=\"${{GHIS_AGENT_REAL_PATH:-$PATH}}\"; export PATH\nexec ghis {command} -- \"$@\"\n"
                ),
            )
            .unwrap();
        }

        let configured = std::env::join_paths([real.clone()]).unwrap();
        let inherited = std::env::join_paths([codex.clone(), shim, real.clone()]).unwrap();
        assert_eq!(
            session_real_path_from(Some(configured), Some(inherited)).unwrap(),
            std::env::join_paths([codex, real]).unwrap()
        );
    }

    #[test]
    fn shim_uses_embedded_real_path_when_control_environment_is_cleared() {
        let root = tempfile::tempdir().unwrap();
        #[cfg(target_os = "macos")]
        let real_bin = root.path().join("real'bin");
        #[cfg(not(target_os = "macos"))]
        let real_bin = root
            .path()
            .join(std::ffi::OsStr::from_bytes(b"real'\xffbin"));
        let shim_bin = root.path().join("shims");
        let trace = root.path().join("trace");
        std::fs::create_dir(&real_bin).unwrap();
        std::fs::create_dir(&shim_bin).unwrap();

        let ghis = root.path().join("ghis");
        executable(
            &ghis,
            r#"#!/bin/sh
set -eu
depth=${GHIS_TEST_DEPTH:-0}
if [ "$depth" -ge 1 ]; then
    printf 'recursed\n' >> "$TRACE"
    exit 90
fi
GHIS_TEST_DEPTH=$((depth + 1)); export GHIS_TEST_DEPTH
printf 'ghis\n' >> "$TRACE"
[ "$1" = git ]; shift
[ "$1" = -- ]; shift
exec git "$@"
"#,
        );
        executable(
            &real_bin.join("git"),
            r#"#!/bin/sh
printf 'git:%s\n' "$*" >> "$TRACE"
"#,
        );

        let shim = write_shim(&shim_bin, "git", &ghis, real_bin.as_os_str()).unwrap();
        let output = Command::new(shim)
            .args(["rev-parse", "--git-dir", "--git-common-dir"])
            .env("PATH", &shim_bin)
            .env("TRACE", &trace)
            .env_remove("GHIS_AGENT_REAL_PATH")
            .env_remove("GHIS_TEST_DEPTH")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "shim failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(trace).unwrap(),
            "ghis\ngit:rev-parse --git-dir --git-common-dir\n"
        );
    }
}
