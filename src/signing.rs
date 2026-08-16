//! SSH Agent and 1Password signing detection.
//!
//! The adapter only handles public material.  It asks the agent for its public
//! key list (`ssh-add -L`) and asks `ssh-keygen` for fingerprints; private keys
//! are never opened or copied.  1Password is detected by its agent socket and
//! signing-program names, not through the optional `op` CLI.

use crate::config::SigningTransport;
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

/// A public-key file discovered under the user's SSH directory. Only `.pub`
/// files are opened; private-key filenames and contents are never inspected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKeyFile {
    pub path: PathBuf,
    pub public_key: String,
    pub fingerprint: Option<String>,
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
    pub transport: SigningTransport,
    pub agent_socket: Option<PathBuf>,
    pub public_key: Option<String>,
    pub fingerprint: Option<String>,
    pub signing_program: Option<PathBuf>,
}

/// Return the SSH agent socket inherited by the current process. This is used
/// for forwarded signing: unlike local-agent mode, it intentionally does not
/// inspect configured sockets or local 1Password locations.
pub fn forwarded_agent_socket() -> Option<PathBuf> {
    env::var_os("SSH_AUTH_SOCK")
        .filter(|socket| !socket.is_empty())
        .map(PathBuf::from)
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

/// Discover public key material from the conventional `~/.ssh` directory.
/// This read-only lookup never inspects private key material.
pub fn discover_public_key_files() -> Vec<PublicKeyFile> {
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    discover_public_key_files_in(&home.join(".ssh"))
}

pub fn discover_public_key_files_in(directory: &Path) -> Vec<PublicKeyFile> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut keys = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("pub")
                || !entry.file_type().ok()?.is_file()
            {
                return None;
            }
            load_public_key_file(&path).ok()
        })
        .collect::<Vec<_>>();
    keys.sort_by(|left, right| left.path.cmp(&right.path));
    keys
}

/// Load one explicitly selected public-key file.
///
/// Requiring a regular `.pub` file before opening it prevents the profile
/// editor from accidentally treating a conventional private-key path as
/// public material. `ssh-keygen` then performs the actual key validation and
/// fingerprint calculation.
pub fn load_public_key_file(path: &Path) -> Result<PublicKeyFile> {
    let path = expand_user(path);
    if path.extension().and_then(|value| value.to_str()) != Some("pub") {
        return Err(SigningError::InvalidKey(format!(
            "{} is not a .pub file",
            path.display()
        )));
    }
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_file() {
        return Err(SigningError::InvalidKey(format!(
            "{} is not a regular public-key file",
            path.display()
        )));
    }
    let text = fs::read_to_string(&path)?;
    let public_key = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .filter(|line| is_public_key_line(line))
        .ok_or_else(|| SigningError::InvalidKey(path.display().to_string()))?;
    let fingerprint = fingerprint(public_key)?;
    Ok(PublicKeyFile {
        path,
        public_key: public_key.to_owned(),
        fingerprint: Some(fingerprint),
    })
}

/// Whether a line starts with a supported SSH public-key algorithm. This is a
/// cheap shape check; callers that need validation must still use
/// [`fingerprint`], which delegates parsing to `ssh-keygen`.
pub fn is_public_key_line(value: &str) -> bool {
    let key_type = value.split_whitespace().next().unwrap_or_default();
    key_type.starts_with("ssh-")
        || key_type.starts_with("ecdsa-")
        || key_type.starts_with("sk-")
        || key_type.starts_with("rsa-sha2-")
}

/// Return the form Git expects for an inline SSH signing public key.
///
/// Git accepts paths directly, but literal public keys must use the `key::`
/// marker. Some Git versions happen to recognize a bare `ssh-*` key while
/// treating ECDSA, FIDO, and RSA-SHA2 keys as filenames, so always emit the
/// explicit form for every supported inline key shape.
pub fn git_signing_key_value(value: &str) -> String {
    let trimmed = value.trim_start();
    if trimmed.starts_with("key::") {
        return trimmed.to_owned();
    }
    if is_public_key_line(trimmed) {
        return format!("key::{trimmed}");
    }
    value.to_owned()
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
        let code = output
            .status
            .code()
            .map_or_else(|| "signal".to_owned(), |code| format!("exit {code}"));
        return Ok(AgentInfo {
            socket,
            source,
            available: false,
            keys: Vec::new(),
            error: Some(if message.is_empty() {
                format!("ssh-add -L failed ({code})")
            } else {
                format!("ssh-add -L failed ({code}): {message}")
            }),
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
    let socket = match config.transport {
        SigningTransport::LocalAgent => discover_agent_socket(config.agent_socket.as_deref()),
        SigningTransport::ForwardedAgent => forwarded_agent_socket(),
    };
    if let Some(socket) = socket {
        match inspect_agent(&socket) {
            Ok(agent) => {
                if config.enabled && !agent.available {
                    status.warnings.push("SSH agent is unavailable".to_owned());
                }
                status.selected_key = select_key(&agent.keys, config);
                if config.enabled && status.selected_key.is_none() {
                    status.warnings.push(match config.transport {
                        SigningTransport::LocalAgent => {
                            "configured signing key was not found in the agent".to_owned()
                        }
                        SigningTransport::ForwardedAgent => {
                            "forwarded SSH agent does not contain the explicitly configured signing key".to_owned()
                        }
                    });
                }
                status.agent = Some(agent);
            }
            Err(err) => status.warnings.push(err.to_string()),
        }
    } else if config.enabled {
        status.warnings.push(match config.transport {
            SigningTransport::LocalAgent => {
                "no SSH_AUTH_SOCK or 1Password Agent socket found".to_owned()
            }
            SigningTransport::ForwardedAgent => {
                "forwarded-agent signing requires SSH_AUTH_SOCK in the current process".to_owned()
            }
        });
    }
    status.program = match config.transport {
        SigningTransport::LocalAgent => discover_signing_program(config.signing_program.as_deref()),
        // A remote process must not infer a local 1Password signing executable.
        SigningTransport::ForwardedAgent => {
            config
                .signing_program
                .as_deref()
                .map(|path| SigningProgram {
                    path: expand_user(path),
                    onepassword: false,
                })
        }
    };
    if config.enabled
        && (matches!(config.transport, SigningTransport::LocalAgent) && status.program.is_none()
            || status
                .program
                .as_ref()
                .is_some_and(|program| !is_executable(&program.path)))
    {
        status
            .warnings
            .push("SSH signing program is unavailable or not executable".to_owned());
    }
    status
}

pub fn select_key(keys: &[AgentKey], config: &SigningProfile) -> Option<AgentKey> {
    let fingerprint = config.fingerprint.as_deref().map(str::trim);
    let public_key = config.public_key.as_deref().and_then(public_key_material);
    if fingerprint.is_some() || config.public_key.is_some() {
        return keys
            .iter()
            .find(|key| {
                fingerprint.is_none_or(|wanted| key.fingerprint.as_deref() == Some(wanted))
                    && (config.public_key.is_none()
                        || public_key.as_deref().is_some_and(|wanted| {
                            public_key_material(&key.public_key).as_deref() == Some(wanted)
                        }))
            })
            .cloned();
    }
    // A forwarded agent belongs to the caller's remote session. Never infer a
    // signing identity from its only key; callers must name public material or
    // a fingerprint explicitly.
    if matches!(config.transport, SigningTransport::ForwardedAgent) {
        return None;
    }
    (keys.len() == 1).then(|| keys[0].clone())
}

/// Normalize a public SSH key to the type and base64 fields GitHub stores.
/// Comments identify a local key for humans but are not part of its public
/// material, so they must not affect matching.
pub fn public_key_material(public_key: &str) -> Option<String> {
    let line = public_key
        .lines()
        .map(str::trim)
        .map(|line| line.strip_prefix("key::").unwrap_or(line))
        .find(|line| is_public_key_line(line))?;
    let mut fields = line.split_whitespace();
    let key_type = fields.next()?;
    let encoded = fields.next()?;
    Some(format!("{key_type} {encoded}"))
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
pub fn render_managed_ssh_config(socket: &Path, public_key: &Path) -> String {
    format!(
        "Host *\n  BatchMode yes\n  ControlMaster no\n  ControlPath none\n  IdentitiesOnly yes\n  IdentityAgent {}\n  IdentityFile {}\n  ForwardAgent no\n",
        ssh_config_quote_path(&expand_user(socket)),
        ssh_config_quote_path(&expand_user(public_key)),
    )
}

pub fn render_ssh_command(ssh_config: &Path, proxy_jump: &[String], forward_agent: bool) -> String {
    let mut command = format!("ssh -F {}", shell_quote(&ssh_config.to_string_lossy()),);
    if !proxy_jump.is_empty() {
        command.push_str(" -J ");
        command.push_str(&shell_quote(&proxy_jump.join(",")));
    }
    if forward_agent {
        command.push_str(" -o ForwardAgent=yes");
    }
    command
}

fn ssh_config_quote_path(path: &Path) -> String {
    let escaped = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
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
    fn forwarded_agent_never_selects_a_single_unconfigured_key() {
        let keys = vec![AgentKey {
            key_type: "ssh-ed25519".into(),
            public_key: "ssh-ed25519 AAAATEST agent".into(),
            comment: Some("agent".into()),
            fingerprint: Some("SHA256:agent".into()),
        }];
        let config = SigningProfile {
            transport: SigningTransport::ForwardedAgent,
            ..SigningProfile::default()
        };
        assert!(select_key(&keys, &config).is_none());

        let by_fingerprint = SigningProfile {
            transport: SigningTransport::ForwardedAgent,
            fingerprint: Some("SHA256:agent".into()),
            ..SigningProfile::default()
        };
        assert_eq!(
            select_key(&keys, &by_fingerprint)
                .and_then(|key| key.fingerprint)
                .as_deref(),
            Some("SHA256:agent")
        );
    }

    #[test]
    fn identifies_onepassword_paths() {
        assert!(is_onepassword_socket(Path::new(
            "/run/user/1000/1Password/agent.sock"
        )));
        assert!(!is_onepassword_socket(Path::new("/tmp/ssh-agent.sock")));
    }

    #[test]
    fn normalizes_public_key_material_without_its_comment() {
        assert_eq!(
            public_key_material("\n# local note\nssh-ed25519 AAAATEST user@example.test\n"),
            Some("ssh-ed25519 AAAATEST".into())
        );
        assert_eq!(public_key_material("not an SSH key"), None);
        assert_eq!(
            public_key_material("key::ecdsa-sha2-nistp256 AAAATEST local comment"),
            Some("ecdsa-sha2-nistp256 AAAATEST".into())
        );
    }

    #[test]
    fn git_signing_key_values_prefix_every_inline_key_shape() {
        for value in [
            "ssh-ed25519 AAAA inline",
            "ecdsa-sha2-nistp256 AAAA inline",
            "sk-ssh-ed25519@openssh.com AAAA inline",
            "rsa-sha2-512 AAAA inline",
        ] {
            assert_eq!(git_signing_key_value(value), format!("key::{value}"));
            assert_eq!(
                git_signing_key_value(&format!("key::{value}")),
                format!("key::{value}")
            );
        }
        assert_eq!(
            git_signing_key_value("/keys/signing key.pub"),
            "/keys/signing key.pub"
        );
    }

    #[test]
    fn invalid_configured_public_key_never_falls_back_to_a_single_agent_key() {
        let keys = vec![AgentKey {
            key_type: "ssh-ed25519".into(),
            public_key: "ssh-ed25519 AAAATEST agent".into(),
            comment: Some("agent".into()),
            fingerprint: Some("SHA256:agent".into()),
        }];
        let config = SigningProfile {
            public_key: Some("not a public key".into()),
            ..SigningProfile::default()
        };
        assert!(select_key(&keys, &config).is_none());
    }

    #[test]
    fn discovers_only_public_key_files_without_opening_private_keys() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("work.pub"),
            concat!(
                "ssh-ed25519 ",
                "AAAAC3NzaC1lZDI1NTE5AAAAIORNiyIAB0NMF693WD7pB4vSv8uiK6PwYOPNBow9qa7t ",
                "work\n"
            ),
        )
        .unwrap();
        fs::write(directory.path().join("work"), "private material\n").unwrap();
        fs::write(directory.path().join("notes.pub"), "not a key\n").unwrap();

        let keys = discover_public_key_files_in(directory.path());
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].path.file_name().unwrap(), "work.pub");
        assert!(keys[0].public_key.ends_with(" work"));
        assert!(
            keys[0]
                .fingerprint
                .as_deref()
                .is_some_and(|value| { value.starts_with("SHA256:") })
        );
    }

    #[test]
    fn explicit_public_key_loader_rejects_non_pub_and_invalid_files() {
        let directory = tempfile::tempdir().unwrap();
        let private = directory.path().join("signing-key");
        let invalid = directory.path().join("invalid.pub");
        fs::write(&private, "private material\n").unwrap();
        fs::write(&invalid, "not a public key\n").unwrap();

        assert!(load_public_key_file(&private).is_err());
        assert!(load_public_key_file(&invalid).is_err());
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
    fn managed_ssh_config_quotes_paths_and_restricts_identities() {
        let config = render_managed_ssh_config(
            Path::new("/tmp/a path/agent%.sock"),
            Path::new("/tmp/key\"s.pub"),
        );
        assert!(config.contains("BatchMode yes"));
        assert!(config.contains("ControlMaster no"));
        assert!(config.contains("ControlPath none"));
        assert!(config.contains("IdentitiesOnly yes"));
        assert!(config.contains("ForwardAgent no"));
        assert!(config.contains("agent%%.sock"));
        assert!(config.contains("key\\\"s.pub"));

        let command = render_ssh_command(Path::new("/tmp/a config/managed's.conf"), &[], false);
        assert!(command.contains("-F '/tmp/a config/managed'\\''s.conf'"));
    }

    #[test]
    fn ssh_command_renders_explicit_proxy_jump_and_forwarding() {
        let command = render_ssh_command(
            Path::new("/tmp/managed.conf"),
            &["deploy@bastion.example:2222".into(), "inner.example".into()],
            true,
        );
        assert!(command.starts_with("ssh -F '/tmp/managed.conf'"));
        assert!(command.contains("-J 'deploy@bastion.example:2222,inner.example'"));
        assert!(command.contains("-o ForwardAgent=yes"));
        assert!(
            Command::new("sh")
                .args(["-n", "-c", &format!("{command} target.example")])
                .status()
                .expect("check rendered shell syntax")
                .success()
        );
    }
}
