//! Git's HTTPS credential-helper protocol.
//!
//! The helper is intentionally narrow: it answers `get` only for the exact
//! HTTPS host and profile selected for the current repository.  `store` and
//! `erase` are no-ops so Git cannot persist a token outside gh's own keyring.

use crate::config;
use crate::github::{self, GhError, SecretToken};
use std::fmt;
use std::io::{self, BufRead, Write};
use zeroize::Zeroizing;

#[derive(Debug)]
pub enum CredentialError {
    Io(io::Error),
    Gh(GhError),
    Command(crate::credential_command::CommandError),
    InvalidRequest(String),
    InvalidAction(String),
    /// A lookup that matched but had no credential source to draw on.
    Lookup(String),
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "credential helper: {err}"),
            Self::Gh(err) => err.fmt(f),
            Self::Command(err) => err.fmt(f),
            Self::InvalidRequest(err) => write!(f, "invalid credential request: {err}"),
            Self::InvalidAction(action) => write!(f, "unknown credential action: {action}"),
            Self::Lookup(reason) => write!(f, "credential lookup: {reason}"),
        }
    }
}

impl From<crate::credential_command::CommandError> for CredentialError {
    fn from(error: crate::credential_command::CommandError) -> Self {
        Self::Command(error)
    }
}

impl std::error::Error for CredentialError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Gh(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for CredentialError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<GhError> for CredentialError {
    fn from(value: GhError) -> Self {
        Self::Gh(value)
    }
}

pub type Result<T> = std::result::Result<T, CredentialError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Get,
    Store,
    Erase,
}

impl Action {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "get" => Ok(Self::Get),
            "store" => Ok(Self::Store),
            "erase" => Ok(Self::Erase),
            other => Err(CredentialError::InvalidAction(other.to_owned())),
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct CredentialRequest {
    pub protocol: Option<String>,
    pub host: Option<String>,
    pub path: Option<String>,
    pub username: Option<String>,
    /// Incoming `store` requests can contain a password.  It is never used by
    /// ghis and is zeroized on drop.
    pub password: Option<Zeroizing<String>>,
    /// `url` is not needed for matching because Git normally sends protocol
    /// and host separately.  Keep it only for protocol compatibility and
    /// zeroize it because URLs are allowed to contain userinfo.
    pub url: Option<Zeroizing<String>>,
    /// Unknown fields are retained for round-tripping/debugging but never
    /// echoed by the helper.
    pub extra: Vec<(String, String)>,
}

impl fmt::Debug for CredentialRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("CredentialRequest");
        debug
            .field("protocol", &self.protocol)
            .field("host", &self.host)
            .field("path", &self.path)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "[redacted]"))
            .field("url", &self.url.as_ref().map(|_| "[redacted]"));
        let extras = self
            .extra
            .iter()
            .map(|(key, value)| {
                if is_sensitive_key(key) {
                    (key.as_str(), "[redacted]")
                } else {
                    (key.as_str(), value.as_str())
                }
            })
            .collect::<Vec<_>>();
        debug.field("extra", &extras).finish()
    }
}

impl CredentialRequest {
    pub fn is_https(&self) -> bool {
        self.protocol
            .as_deref()
            .map(|protocol| protocol.eq_ignore_ascii_case("https"))
            .unwrap_or(false)
    }

    pub fn normalized_host(&self) -> Option<String> {
        self.host.as_deref().map(github::normalize_host)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    pub username: String,
    pub password: SecretToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialProfile {
    pub host: String,
    pub login: String,
    /// Username emitted next to the token, and the only one a request that
    /// carries a username may claim: Git echoes back whatever we emitted, so
    /// `Command` profiles with a `credential_username` other than `login` would
    /// otherwise stop matching their own responses.
    pub username: String,
    pub mode: config::CredentialMode,
    /// argv used only in `Command` mode.
    pub command: Vec<String>,
}

impl CredentialProfile {
    /// The `gh`-backed profile every existing configuration describes.
    pub fn gh(host: String, login: String) -> Self {
        Self {
            username: login.clone(),
            host,
            login,
            mode: config::CredentialMode::Manage,
            command: Vec::new(),
        }
    }
}

/// Parse the line-oriented protocol.  Git terminates a request with an empty
/// line; accepting EOF as a terminator makes this function convenient for
/// tests and direct invocation.
pub fn parse_request(input: &str) -> Result<CredentialRequest> {
    let mut request = CredentialRequest::default();
    for raw in input.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() {
            break;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(CredentialError::InvalidRequest(
                "malformed key/value line".into(),
            ));
        };
        if key.is_empty() {
            return Err(CredentialError::InvalidRequest(
                "credential field name is empty".into(),
            ));
        }
        let key = key.to_owned();
        let value = value.to_owned();
        match key.as_str() {
            "protocol" => request.protocol = Some(value),
            "host" => request.host = Some(value),
            "path" => request.path = Some(value),
            "username" => request.username = Some(value),
            "password" => request.password = Some(Zeroizing::new(value)),
            "url" => request.url = Some(Zeroizing::new(value)),
            _ if is_sensitive_key(&key) => request.extra.push((key, "[redacted]".into())),
            _ => request.extra.push((key, value)),
        }
    }
    Ok(request)
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["password", "token", "secret", "authorization", "url"]
        .iter()
        .any(|needle| key.contains(needle))
}

/// Return the protocol response for a successful `get`.  Password bytes are
/// copied directly into the output and are not included in any error value.
pub fn format_response(request: &CredentialRequest, credential: &Credential) -> String {
    let protocol = request.protocol.as_deref().unwrap_or("https");
    let host = request.host.as_deref().unwrap_or_default();
    let mut output = String::new();
    output.push_str("protocol=");
    output.push_str(protocol);
    output.push('\n');
    if !host.is_empty() {
        output.push_str("host=");
        output.push_str(host);
        output.push('\n');
    }
    output.push_str("username=");
    output.push_str(&credential.username);
    output.push('\n');
    output.push_str("password=");
    output.push_str(credential.password.as_str().unwrap_or_default());
    output.push_str("\n\n");
    output
}

/// Match a request to a profile without resolving a token.  Matching is exact
/// on protocol and host; an explicit username must agree with the profile.
pub fn matches_profile(request: &CredentialRequest, profile: &CredentialProfile) -> bool {
    if !request.is_https() {
        return false;
    }
    let Some(host) = request.normalized_host() else {
        return false;
    };
    if host != github::normalize_host(&profile.host) {
        return false;
    }
    request
        .username
        .as_deref()
        .is_none_or(|username| username == profile.username)
}

/// Resolve a matching profile.  `Manage` reads gh's keyring, `Command` runs the
/// profile's argv.  No token is cached by this helper and `store`/`erase`
/// remain no-ops.
pub fn get_for_profile(
    request: &CredentialRequest,
    profile: &CredentialProfile,
) -> Result<Option<Credential>> {
    if !matches_profile(request, profile) {
        return Ok(None);
    }
    let password = match profile.mode {
        config::CredentialMode::Manage => github::token(&profile.host, &profile.login)?,
        config::CredentialMode::Command => crate::credential_command::run(&profile.command)?,
        // A passthrough profile never has this helper installed, so reaching
        // here would mean something installed it anyway.  Refuse rather than
        // resolve: falling through to gh would apply a GitHub token the profile
        // explicitly declined to use.
        config::CredentialMode::Passthrough => {
            return Err(CredentialError::Lookup(
                "credential helper ran for a passthrough profile".into(),
            ));
        }
    };
    Ok(Some(Credential {
        username: profile.username.clone(),
        password,
    }))
}

pub trait Lookup {
    fn get(&mut self, request: &CredentialRequest) -> Result<Option<Credential>>;
}

impl<F> Lookup for F
where
    F: FnMut(&CredentialRequest) -> Result<Option<Credential>>,
{
    fn get(&mut self, request: &CredentialRequest) -> Result<Option<Credential>> {
        self(request)
    }
}

/// Handle one helper action from an input reader.  `store` and `erase` consume
/// the request but emit no records.  This mirrors Git's helper protocol and
/// avoids accidentally persisting a selected account's token.
pub fn handle<R, W, L>(action: Action, mut reader: R, mut writer: W, mut lookup: L) -> Result<()>
where
    R: BufRead,
    W: Write,
    L: Lookup,
{
    let mut input = Zeroizing::new(String::new());
    reader.read_to_string(&mut input)?;
    let request = parse_request(&input)?;
    if action != Action::Get {
        writer.write_all(b"\n")?;
        writer.flush()?;
        return Ok(());
    }
    match lookup.get(&request) {
        Ok(Some(credential)) => {
            let response = Zeroizing::new(format_response(&request, &credential));
            writer.write_all(response.as_bytes())?;
        }
        // A selected Profile is authoritative. A protocol/host/login mismatch
        // must not fall through to another helper or an interactive prompt.
        Ok(None) => writer.write_all(b"quit=true\n\n")?,
        Err(error) => {
            // Stop Git from consulting another helper or prompting after a
            // bound profile's credential lookup failed.
            writer.write_all(b"quit=true\n\n")?;
            writer.flush()?;
            return Err(error);
        }
    }
    writer.flush()?;
    Ok(())
}

/// Convenience entry point for a process implementing Git's helper command.
/// `args` should contain exactly one action (`get`, `store`, or `erase`).
pub fn serve<R, W>(args: &[String], reader: R, writer: W, profile: CredentialProfile) -> Result<()>
where
    R: BufRead,
    W: Write,
{
    let action = args
        .first()
        .ok_or_else(|| CredentialError::InvalidAction("missing action".into()))
        .and_then(|value| Action::parse(value))?;
    handle(action, reader, writer, ProfileLookup { profile })
}

struct ProfileLookup {
    profile: CredentialProfile,
}

impl Lookup for ProfileLookup {
    fn get(&mut self, request: &CredentialRequest) -> Result<Option<Credential>> {
        get_for_profile(request, &self.profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Cursor;

    struct FakeLookup;

    impl Lookup for FakeLookup {
        fn get(&mut self, request: &CredentialRequest) -> Result<Option<Credential>> {
            Ok(matches_profile(
                request,
                &CredentialProfile::gh("github.com".into(), "alice".into()),
            )
            .then(|| Credential {
                username: "alice".into(),
                password: SecretToken::new(b"test-token".to_vec()),
            }))
        }
    }

    struct FailingLookup;

    impl Lookup for FailingLookup {
        fn get(&mut self, _request: &CredentialRequest) -> Result<Option<Credential>> {
            Err(CredentialError::InvalidRequest("lookup failed".into()))
        }
    }

    #[test]
    fn parses_git_protocol_and_preserves_unknown_fields() {
        let request =
            parse_request("protocol=https\nhost=GitHub.com\nusername=alice\nww=1\n\n").unwrap();
        assert!(request.is_https());
        assert_eq!(request.normalized_host().as_deref(), Some("github.com"));
        assert_eq!(request.extra, vec![("ww".into(), "1".into())]);
    }

    #[test]
    fn request_debug_redacts_store_passwords() {
        let request =
            parse_request("protocol=https\nhost=github.com\npassword=secret-token\n\n").unwrap();
        assert!(!format!("{request:?}").contains("secret-token"));
    }

    #[test]
    fn matching_requires_exact_https_host_and_login() {
        let profile = CredentialProfile::gh("github.com".into(), "alice".into());
        let request = parse_request("protocol=https\nhost=github.com\nusername=alice\n\n").unwrap();
        assert!(matches_profile(&request, &profile));
        let wrong = parse_request("protocol=https\nhost=github.com\nusername=bob\n\n").unwrap();
        assert!(!matches_profile(&wrong, &profile));
        let ssh = parse_request("protocol=ssh\nhost=github.com\n\n").unwrap();
        assert!(!matches_profile(&ssh, &profile));
    }

    #[test]
    fn matching_canonicalizes_default_https_port_and_preserves_other_ports() {
        let profile = CredentialProfile::gh("Git.Example.Test.".into(), "alice".into());
        let default_port =
            parse_request("protocol=HTTPS\nhost=git.example.test:443\nusername=alice\n\n").unwrap();
        assert!(matches_profile(&default_port, &profile));

        let enterprise_profile =
            CredentialProfile::gh("git.example.test:8443".into(), "alice".into());
        let enterprise = parse_request("protocol=https\nhost=GIT.EXAMPLE.TEST.:8443\n\n").unwrap();
        assert!(matches_profile(&enterprise, &enterprise_profile));
        assert!(!matches_profile(&default_port, &enterprise_profile));
    }

    #[test]
    fn matching_supports_bracketed_ipv6_hosts() {
        let profile = CredentialProfile::gh("[2001:db8::1]".into(), "alice".into());
        let request = parse_request("protocol=https\nhost=[2001:DB8::1]:443\n\n").unwrap();
        assert!(matches_profile(&request, &profile));
    }

    #[test]
    fn unmatched_request_stops_the_helper_chain() {
        let input = Cursor::new("protocol=https\nhost=other.test\n\n");
        let mut output = Vec::new();
        let profile = CredentialProfile::gh("github.com".into(), "alice".into());
        handle(Action::Get, input, &mut output, ProfileLookup { profile }).unwrap();
        assert_eq!(output, b"quit=true\n\n");
    }

    #[test]
    fn get_response_uses_git_credential_protocol() {
        let input = Cursor::new("protocol=https\nhost=github.com\n\n");
        let mut output = Vec::new();
        handle(Action::Get, input, &mut output, FakeLookup).unwrap();
        assert_eq!(
            output,
            b"protocol=https\nhost=github.com\nusername=alice\npassword=test-token\n\n"
        );
    }

    #[test]
    fn lookup_failure_stops_the_helper_chain() {
        let input = Cursor::new("protocol=https\nhost=github.com\n\n");
        let mut output = Vec::new();
        let error =
            handle(Action::Get, input, &mut output, FailingLookup).expect_err("lookup must fail");
        assert!(error.to_string().contains("lookup failed"));
        assert_eq!(output, b"quit=true\n\n");
    }

    /// Write an executable POSIX script and return its path as an argv element.
    #[cfg(unix)]
    fn script(dir: &std::path::Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(&path, body).expect("write script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        path.to_string_lossy().into_owned()
    }

    #[test]
    #[cfg(unix)]
    fn a_credential_username_other_than_login_still_matches_its_own_response() {
        let profile = CredentialProfile {
            username: "oauth2".into(),
            command: Vec::new(),
            mode: config::CredentialMode::Command,
            host: "git.example.test".into(),
            login: "alice".into(),
        };
        // Git echoes back whatever the helper emitted, so the second request
        // carries `oauth2` rather than `login`.
        let request =
            parse_request("protocol=https\nhost=git.example.test\nusername=oauth2\n\n").unwrap();
        assert!(matches_profile(&request, &profile));

        let other =
            parse_request("protocol=https\nhost=git.example.test\nusername=bob\n\n").unwrap();
        assert!(!matches_profile(&other, &profile));
    }

    #[test]
    #[cfg(unix)]
    fn command_mode_answers_with_the_command_output_and_the_declared_username() {
        let dir = tempfile::tempdir().expect("temp dir");
        let argv = vec![script(
            dir.path(),
            "token.sh",
            "#!/bin/sh\nprintf 'gitlab-token\\n'\n",
        )];
        let profile = CredentialProfile {
            username: "oauth2".into(),
            command: argv,
            mode: config::CredentialMode::Command,
            host: "git.example.test".into(),
            login: "alice".into(),
        };

        let credential = get_for_profile(
            &parse_request("protocol=https\nhost=git.example.test\n\n").unwrap(),
            &profile,
        )
        .expect("lookup succeeds")
        .expect("credential present");

        assert_eq!(credential.username, "oauth2");
        assert_eq!(credential.password.as_str(), Some("gitlab-token"));

        let input = Cursor::new("protocol=https\nhost=git.example.test\n\n");
        let mut output = Vec::new();
        handle(Action::Get, input, &mut output, ProfileLookup { profile }).expect("helper answers");
        assert_eq!(
            output,
            b"protocol=https\nhost=git.example.test\nusername=oauth2\npassword=gitlab-token\n\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_failing_command_stops_the_helper_chain_with_quit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let profile = CredentialProfile {
            username: "oauth2".into(),
            command: vec![script(
                dir.path(),
                "broken.sh",
                "#!/bin/sh\necho vault locked >&2\nexit 4\n",
            )],
            mode: config::CredentialMode::Command,
            host: "git.example.test".into(),
            login: "alice".into(),
        };

        let input = Cursor::new("protocol=https\nhost=git.example.test\n\n");
        let mut output = Vec::new();
        let error = handle(Action::Get, input, &mut output, ProfileLookup { profile })
            .expect_err("a failing command must fail the lookup");

        assert_eq!(output, b"quit=true\n\n");
        assert!(error.to_string().contains("vault locked"), "{error}");
    }

    #[test]
    fn a_passthrough_profile_refuses_to_resolve_anything() {
        let profile = CredentialProfile {
            username: "alice".into(),
            command: Vec::new(),
            mode: config::CredentialMode::Passthrough,
            host: "git.example.test".into(),
            login: "alice".into(),
        };
        let request = parse_request("protocol=https\nhost=git.example.test\n\n").unwrap();

        let error = get_for_profile(&request, &profile).expect_err("must not resolve");
        assert!(matches!(error, CredentialError::Lookup(_)), "{error:?}");

        let input = Cursor::new("protocol=https\nhost=git.example.test\n\n");
        let mut output = Vec::new();
        let error = handle(Action::Get, input, &mut output, ProfileLookup { profile })
            .expect_err("must not resolve");
        assert_eq!(output, b"quit=true\n\n");
        assert!(error.to_string().contains("passthrough"), "{error}");
    }
}
