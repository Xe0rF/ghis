//! SSH Agent and 1Password signing detection.
//!
//! The adapter only handles public material.  It asks the agent for its public
//! key list (`ssh-add -L`) and asks `ssh-keygen` for fingerprints; private keys
//! are never opened or copied.  1Password is detected by its agent socket and
//! signing-program names, not through the optional `op` CLI.

use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug)]
pub enum SigningError {
    Io(std::io::Error),
    Command {
        operation: String,
        code: Option<i32>,
        stderr: String,
    },
    InvalidKey(String),
}

impl fmt::Display for SigningError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "SSH signing: {err}"),
            Self::Command {
                operation,
                code,
                stderr,
            } => {
                write!(f, "{operation} failed")?;
                if let Some(code) = code {
                    write!(f, " (exit {code})")?;
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            Self::InvalidKey(value) => write!(f, "invalid SSH public key: {value}"),
        }
    }
}

impl std::error::Error for SigningError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for SigningError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, SigningError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSource {
    OnePassword,
    System,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentKey {
    pub key_type: String,
    /// The complete public key line, including its base64 body and optional
    /// comment.  It is safe to persist this value.
    pub public_key: String,
    pub comment: Option<String>,
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    pub socket: PathBuf,
    pub source: AgentSource,
    pub available: bool,
    pub keys: Vec<AgentKey>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningProgram {
    pub path: PathBuf,
    pub onepassword: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SigningStatus {
    pub enabled: bool,
    pub agent: Option<AgentInfo>,
    pub program: Option<SigningProgram>,
    pub selected_key: Option<AgentKey>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SigningProfile {
    pub enabled: bool,
    pub agent_socket: Option<PathBuf>,
    pub public_key: Option<String>,
    pub fingerprint: Option<String>,
    pub signing_program: Option<PathBuf>,
}

/// Return likely 1Password Agent socket paths that exist on this machine.
/// A manually configured socket is preferred and returned even before it
/// exists, allowing the caller to show a precise unavailable diagnostic.
pub fn discover_agent_socket(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return Some(expand_user(path));
    }
    if let Some(candidate) = onepassword_socket_candidates()
        .into_iter()
        .find(|candidate| candidate.exists())
    {
        return Some(candidate);
    }
    if let Ok(socket) = env::var("SSH_AUTH_SOCK")
        && !socket.is_empty()
    {
        return Some(PathBuf::from(socket));
    }
    None
}

pub fn onepassword_socket_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(home) = env::var("HOME") {
        let home = PathBuf::from(home);
        paths.push(home.join(".1password/agent.sock"));
        paths.push(home.join(".config/1Password/agent.sock"));
        paths.push(home.join(".var/app/com.onepassword.OnePassword/data/1password/agent.sock"));
    }
    if let Ok(runtime) = env::var("XDG_RUNTIME_DIR") {
        paths.push(PathBuf::from(&runtime).join("1password/agent.sock"));
        paths.push(PathBuf::from(runtime).join("1password/ssh-agent.sock"));
    }
    if let Ok(uid) = env::var("UID")
        && uid.bytes().all(|byte| byte.is_ascii_digit())
    {
        paths.push(PathBuf::from(format!(
            "/run/user/{uid}/1password/agent.sock"
        )));
    }
    paths
}

/// Query one SSH agent.  An unavailable agent is represented in `AgentInfo`
/// rather than returned as an error so `doctor` can report all diagnostics at
/// once.
pub fn inspect_agent(socket: impl AsRef<Path>) -> Result<AgentInfo> {
    let socket = expand_user(socket.as_ref());
    let source = if is_onepassword_socket(&socket) {
        AgentSource::OnePassword
    } else {
        AgentSource::System
    };
    let output = Command::new("ssh-add")
        .arg("-L")
        .env("SSH_AUTH_SOCK", &socket)
        .output()?;
    if !output.status.success() {
        let message = if output.stderr.is_empty() {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        } else {
            String::from_utf8_lossy(&output.stderr).trim().to_owned()
        };
        if message.contains("The agent has no identities") {
            return Ok(AgentInfo {
                socket,
                source,
                available: true,
                keys: Vec::new(),
                error: None,
            });
        }
        return Ok(AgentInfo {
            socket,
            source,
            available: false,
            keys: Vec::new(),
            error: Some(message),
        });
    }
    let mut keys = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if line.is_empty() || line == "The agent has no identities." {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(key_type) = parts.next() else {
            continue;
        };
        let Some(encoded) = parts.next() else {
            return Err(SigningError::InvalidKey(line.to_owned()));
        };
        let comment = parts.collect::<Vec<_>>().join(" ");
        let public_key = if comment.is_empty() {
            format!("{key_type} {encoded}")
        } else {
            format!("{key_type} {encoded} {comment}")
        };
        let fingerprint = fingerprint(&public_key).ok();
        keys.push(AgentKey {
            key_type: key_type.to_owned(),
            public_key,
            comment: (!comment.is_empty()).then_some(comment),
            fingerprint,
        });
    }
    Ok(AgentInfo {
        socket,
        source,
        available: true,
        keys,
        error: None,
    })
}

/// Ask `ssh-keygen` for the SHA-256 fingerprint of one public key line.
pub fn fingerprint(public_key: &str) -> Result<String> {
    let mut child = Command::new("ssh-keygen")
        .args(["-lf", "-", "-E", "sha256"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        use std::io::Write;
        stdin.write_all(public_key.as_bytes())?;
        stdin.write_all(b"\n")?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(SigningError::Command {
            operation: "ssh-keygen fingerprint".into(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let mut fields = line.split_whitespace();
    let _bits = fields.next();
    let Some(value) = fields.next() else {
        return Err(SigningError::InvalidKey(line.trim().to_owned()));
    };
    Ok(value.to_owned())
}

pub fn is_onepassword_socket(path: &Path) -> bool {
    path.to_string_lossy()
        .to_ascii_lowercase()
        .contains("1password")
}

/// Locate `op-ssh-sign` without invoking a shell or requiring the `op` CLI.
pub fn discover_signing_program(configured: Option<&Path>) -> Option<SigningProgram> {
    if let Some(path) = configured {
        let path = expand_user(path);
        return Some(SigningProgram {
            onepassword: path
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains("op-ssh-sign"),
            path,
        });
    }
    let names = ["op-ssh-sign", "op-ssh-sign.exe"];
    if let Ok(path_var) = env::var("PATH") {
        for directory in env::split_paths(&path_var) {
            for name in names {
                let candidate = directory.join(name);
                if is_executable(&candidate) {
                    return Some(SigningProgram {
                        path: candidate,
                        onepassword: true,
                    });
                }
            }
        }
    }
    for candidate in [
        "/opt/1Password/op-ssh-sign",
        "/usr/local/bin/op-ssh-sign",
        "/usr/bin/op-ssh-sign",
    ] {
        let candidate = PathBuf::from(candidate);
        if is_executable(&candidate) {
            return Some(SigningProgram {
                path: candidate,
                onepassword: true,
            });
        }
    }
    None
}

pub fn inspect(config: &SigningProfile) -> SigningStatus {
    let mut status = SigningStatus {
        enabled: config.enabled,
        ..SigningStatus::default()
    };
    let socket = discover_agent_socket(config.agent_socket.as_deref());
    if let Some(socket) = socket {
        match inspect_agent(&socket) {
            Ok(agent) => {
                if config.enabled && !agent.available {
                    status.warnings.push("SSH agent is unavailable".to_owned());
                }
                status.selected_key = select_key(&agent.keys, config);
                if config.enabled && status.selected_key.is_none() {
                    status
                        .warnings
                        .push("configured signing key was not found in the agent".to_owned());
                }
                status.agent = Some(agent);
            }
            Err(err) => status.warnings.push(err.to_string()),
        }
    } else if config.enabled {
        status
            .warnings
            .push("no SSH_AUTH_SOCK or 1Password Agent socket found".to_owned());
    }
    status.program = discover_signing_program(config.signing_program.as_deref());
    if config.enabled
        && status
            .program
            .as_ref()
            .is_none_or(|program| !is_executable(&program.path))
    {
        status
            .warnings
            .push("SSH signing program is unavailable or not executable".to_owned());
    }
    status
}

pub fn select_key(keys: &[AgentKey], config: &SigningProfile) -> Option<AgentKey> {
    let fingerprint = config.fingerprint.as_deref().map(str::trim);
    let public_key = config.public_key.as_deref().map(key_material);
    if fingerprint.is_some() || public_key.is_some() {
        return keys
            .iter()
            .find(|key| {
                fingerprint.is_none_or(|wanted| key.fingerprint.as_deref() == Some(wanted))
                    && public_key
                        .as_deref()
                        .is_none_or(|wanted| key_material(&key.public_key) == wanted)
            })
            .cloned();
    }
    (keys.len() == 1).then(|| keys[0].clone())
}

fn key_material(public_key: &str) -> String {
    public_key
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a discovered or explicitly configured signing program is an
/// executable file. Explicit paths are reported even when unavailable so
/// diagnostics can name the exact broken setting.
pub fn signing_program_available(program: &SigningProgram) -> bool {
    is_executable(&program.path)
}

/// Produce a `core.sshCommand` value that restricts OpenSSH to one agent/key.
/// Git executes this value through the user's shell, therefore every path is
/// POSIX-quoted here. User SSH config and connection sharing are disabled so
/// another configured identity or an existing multiplexed session cannot win.
pub fn render_ssh_command(socket: &Path, public_key: &Path) -> String {
    let mut command = format!(
        "ssh -F /dev/null -o BatchMode=yes -o ControlMaster=no \
         -o ControlPath=none -o IdentitiesOnly=yes -o IdentityAgent={} ",
        shell_quote(&expand_user(socket).to_string_lossy())
    );
    command.push_str("-i ");
    command.push_str(&shell_quote(&expand_user(public_key).to_string_lossy()));
    command.push(' ');
    command.trim_end().to_owned()
}

pub fn expand_user(path: &Path) -> PathBuf {
    let value = path.to_string_lossy();
    if value == "~" {
        return env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| path.to_owned());
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    path.to_owned()
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".into();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_onepassword_paths() {
        assert!(is_onepassword_socket(Path::new(
            "/run/user/1000/1Password/agent.sock"
        )));
        assert!(!is_onepassword_socket(Path::new("/tmp/ssh-agent.sock")));
    }

    #[test]
    fn selects_by_fingerprint_then_public_key() {
        let keys = vec![
            AgentKey {
                key_type: "ssh-ed25519".into(),
                public_key: "ssh-ed25519 AAAA first".into(),
                comment: Some("first".into()),
                fingerprint: Some("SHA256:first".into()),
            },
            AgentKey {
                key_type: "ssh-ed25519".into(),
                public_key: "ssh-ed25519 BBBB second".into(),
                comment: Some("second".into()),
                fingerprint: Some("SHA256:second".into()),
            },
        ];
        let profile = SigningProfile {
            fingerprint: Some("SHA256:second".into()),
            ..SigningProfile::default()
        };
        assert_eq!(
            select_key(&keys, &profile).unwrap().comment.as_deref(),
            Some("second")
        );

        let mismatched = SigningProfile {
            fingerprint: Some("SHA256:missing".into()),
            ..SigningProfile::default()
        };
        assert!(select_key(&keys[..1], &mismatched).is_none());

        let conflicting = SigningProfile {
            fingerprint: Some("SHA256:second".into()),
            public_key: Some("ssh-ed25519 AAAA first".into()),
            ..SigningProfile::default()
        };
        assert!(select_key(&keys, &conflicting).is_none());
    }

    #[test]
    fn ssh_command_quotes_paths_and_restricts_identities() {
        let command = render_ssh_command(
            Path::new("/tmp/a path/agent.sock"),
            Path::new("/tmp/key's.pub"),
        );
        assert!(command.contains("-F /dev/null"));
        assert!(command.contains("ControlMaster=no"));
        assert!(command.contains("ControlPath=none"));
        assert!(command.contains("IdentitiesOnly=yes"));
        assert!(command.contains("'\\''"));
    }
}
