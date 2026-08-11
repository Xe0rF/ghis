//! Git's HTTPS credential-helper protocol.
//!
//! The helper is intentionally narrow: it answers `get` only for the exact
//! HTTPS host and profile selected for the current repository.  `store` and
//! `erase` are no-ops so Git cannot persist a token outside gh's own keyring.

use crate::github::{self, GhError, SecretToken};
use std::fmt;
use std::io::{self, BufRead, Write};
use zeroize::Zeroizing;

#[derive(Debug)]
pub enum CredentialError {
    Io(io::Error),
    Gh(GhError),
    InvalidRequest(String),
    InvalidAction(String),
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "credential helper: {err}"),
            Self::Gh(err) => err.fmt(f),
            Self::InvalidRequest(err) => write!(f, "invalid credential request: {err}"),
            Self::InvalidAction(action) => write!(f, "unknown credential action: {action}"),
        }
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

/// Match a request to a profile without querying gh.  Matching is exact on
/// protocol and host; an explicit username must agree with the profile.
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
        .is_none_or(|username| username == profile.login)
}

/// Resolve a matching profile through gh's keyring.  No token is cached by
/// this helper and `store`/`erase` remain no-ops.
pub fn get_for_profile(
    request: &CredentialRequest,
    profile: &CredentialProfile,
) -> Result<Option<Credential>> {
    if !matches_profile(request, profile) {
        return Ok(None);
    }
    let password = github::token(&profile.host, &profile.login)?;
    Ok(Some(Credential {
        username: profile.login.clone(),
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
    use std::io::Cursor;

    struct FakeLookup;

    impl Lookup for FakeLookup {
        fn get(&mut self, request: &CredentialRequest) -> Result<Option<Credential>> {
            Ok(matches_profile(
                request,
                &CredentialProfile {
                    host: "github.com".into(),
                    login: "alice".into(),
                },
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
        let profile = CredentialProfile {
            host: "github.com".into(),
            login: "alice".into(),
        };
        let request = parse_request("protocol=https\nhost=github.com\nusername=alice\n\n").unwrap();
        assert!(matches_profile(&request, &profile));
        let wrong = parse_request("protocol=https\nhost=github.com\nusername=bob\n\n").unwrap();
        assert!(!matches_profile(&wrong, &profile));
        let ssh = parse_request("protocol=ssh\nhost=github.com\n\n").unwrap();
        assert!(!matches_profile(&ssh, &profile));
    }

    #[test]
    fn matching_canonicalizes_default_https_port_and_preserves_other_ports() {
        let profile = CredentialProfile {
            host: "Git.Example.Test.".into(),
            login: "alice".into(),
        };
        let default_port =
            parse_request("protocol=HTTPS\nhost=git.example.test:443\nusername=alice\n\n").unwrap();
        assert!(matches_profile(&default_port, &profile));

        let enterprise_profile = CredentialProfile {
            host: "git.example.test:8443".into(),
            login: "alice".into(),
        };
        let enterprise = parse_request("protocol=https\nhost=GIT.EXAMPLE.TEST.:8443\n\n").unwrap();
        assert!(matches_profile(&enterprise, &enterprise_profile));
        assert!(!matches_profile(&default_port, &enterprise_profile));
    }

    #[test]
    fn matching_supports_bracketed_ipv6_hosts() {
        let profile = CredentialProfile {
            host: "[2001:db8::1]".into(),
            login: "alice".into(),
        };
        let request = parse_request("protocol=https\nhost=[2001:DB8::1]:443\n\n").unwrap();
        assert!(matches_profile(&request, &profile));
    }

    #[test]
    fn unmatched_request_stops_the_helper_chain() {
        let input = Cursor::new("protocol=https\nhost=other.test\n\n");
        let mut output = Vec::new();
        let profile = CredentialProfile {
            host: "github.com".into(),
            login: "alice".into(),
        };
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
}
