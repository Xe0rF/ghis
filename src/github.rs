//! GitHub CLI adapter.
//!
//! `ghis` deliberately does not call `gh auth switch`: that command mutates a
//! process-global active account and makes two terminals race.  Instead, the
//! selected account's token is requested for one child process and injected
//! only into that child.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Output};
use tempfile::NamedTempFile;
use zeroize::Zeroizing;

const DISCOVERY_CACHE_VERSION: u32 = 1;
const MAX_COMMAND_ERROR_CHARS: usize = 2_048;
pub const DISCOVERY_CACHE_FILENAME: &str = "accounts.json";

#[derive(Debug)]
pub enum GhError {
    Io(std::io::Error),
    Command {
        operation: String,
        code: Option<i32>,
        stderr: String,
    },
    Json(String),
    MissingToken {
        host: String,
        login: String,
    },
}

impl fmt::Display for GhError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "gh: {err}"),
            Self::Command {
                operation,
                code,
                stderr,
            } => {
                write!(f, "gh {operation} failed")?;
                if let Some(code) = code {
                    write!(f, " (exit {code})")?;
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            Self::Json(err) => write!(f, "invalid gh JSON: {err}"),
            Self::MissingToken { host, login } => {
                write!(f, "no token available for {login} on {host}")
            }
        }
    }
}

impl std::error::Error for GhError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for GhError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, GhError>;

#[derive(Clone, PartialEq, Eq)]
pub struct SecretToken(Zeroizing<Vec<u8>>);

impl SecretToken {
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_slice()
    }

    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(self.0.as_slice()).ok()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn into_bytes(mut self) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(std::mem::take(&mut *self.0))
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretToken([redacted])")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GhAccount {
    pub host: String,
    pub login: String,
    pub active: bool,
    /// gh reports `state = "error"` when an account cannot currently be
    /// verified.  Such an account remains discoverable but is not trusted for
    /// automatic selection.
    pub state: Option<String>,
    pub verified: bool,
    pub error: Option<String>,
    pub token_source: Option<String>,
    pub git_protocol: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GhDiscovery {
    pub accounts: Vec<GhAccount>,
    pub command_succeeded: bool,
    pub offline: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct DiscoveryCache {
    version: u32,
    discovery: GhDiscovery,
}

/// Read cached account metadata without invoking `gh` or touching its keyring.
/// A missing cache is normal; corrupt or newer cache formats are ignored by
/// callers in the same way as a cache miss.
pub fn load_discovery_cache(path: &Path) -> Result<Option<GhDiscovery>> {
    let input = match fs::read(path) {
        Ok(input) => input,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let cache: DiscoveryCache =
        serde_json::from_slice(&input).map_err(|error| GhError::Json(error.to_string()))?;
    if cache.version != DISCOVERY_CACHE_VERSION {
        return Ok(None);
    }
    Ok(Some(cache.discovery))
}

/// Atomically cache only non-secret account discovery metadata. GitHub tokens
/// remain exclusively in gh's credential store and cannot be represented by
/// [`GhDiscovery`].
pub fn save_discovery_cache(path: &Path, discovery: &GhDiscovery) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    serde_json::to_writer(
        &mut temporary,
        &DiscoveryCache {
            version: DISCOVERY_CACHE_VERSION,
            discovery: discovery.clone(),
        },
    )
    .map_err(|error| GhError::Json(error.to_string()))?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o600))?;
    }
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// Parse the stable `gh auth status --json hosts` object.  We intentionally do
/// not deserialize a fixed struct because gh has added fields across releases
/// and currently returns an account even when the network check fails.
pub fn parse_auth_status_json(input: &str) -> Result<Vec<GhAccount>> {
    let value: Value = serde_json::from_str(input).map_err(|err| GhError::Json(err.to_string()))?;
    let hosts = value
        .get("hosts")
        .ok_or_else(|| GhError::Json("missing hosts field".into()))?;
    let object = hosts
        .as_object()
        .ok_or_else(|| GhError::Json("hosts is not an object".into()))?;

    let mut accounts = Vec::new();
    for (host_key, entries) in object {
        // gh currently emits arrays.  Accept a single object as well so old
        // enterprise builds remain useful.
        let values: Vec<&Value> = entries
            .as_array()
            .map(|array| array.iter().collect())
            .or_else(|| entries.as_object().map(|_| vec![entries]))
            .unwrap_or_default();
        for entry in values {
            let host = string_field(entry, "host").unwrap_or_else(|| host_key.clone());
            let Some(login) = string_field(entry, "login") else {
                continue;
            };
            let state = string_field(entry, "state");
            let error = string_field(entry, "error");
            let verified = error.is_none()
                && matches!(
                    state.as_deref(),
                    None | Some("success") | Some("authenticated") | Some("ok")
                );
            accounts.push(GhAccount {
                host,
                login,
                active: bool_field(entry, "active").unwrap_or(false),
                state,
                verified,
                error,
                token_source: string_field(entry, "tokenSource"),
                git_protocol: string_field(entry, "gitProtocol"),
            });
        }
    }
    Ok(accounts)
}

/// Discover all accounts known by gh.  A failed status command is not itself
/// fatal if gh returned JSON with `state = "error"`; callers can show those
/// accounts as unverified while remaining offline.
pub fn discover_accounts(host: Option<&str>) -> Result<GhDiscovery> {
    let mut args = vec!["auth", "status", "--json", "hosts"];
    if let Some(host) = host {
        args.push("--hostname");
        args.push(host);
    }
    let output = sanitized_command("gh", &args, None, None)?;
    let text = String::from_utf8_lossy(&output.stdout);
    if text.trim().is_empty() {
        return Err(command_error("auth status", &output));
    }
    let accounts = parse_auth_status_json(&text)?;
    Ok(GhDiscovery {
        offline: accounts.iter().any(|account| !account.verified),
        accounts,
        command_succeeded: output.status.success(),
    })
}

/// Ask gh's credential store for one exact account.  The token is never put in
/// argv, logs, or a parent-shell environment.
pub fn token(host: &str, login: &str) -> Result<SecretToken> {
    let host = normalize_host(host);
    let output = sanitized_command(
        "gh",
        ["auth", "token", "--hostname", &host, "--user", login],
        None,
        None,
    )?;
    if !output.status.success() {
        return Err(command_error("auth token", &output));
    }
    let token = output.stdout;
    let token = trim_ascii_newline(token);
    if token.is_empty() {
        return Err(GhError::MissingToken {
            host,
            login: login.to_owned(),
        });
    }
    Ok(SecretToken::new(token))
}

/// Run one gh invocation under a selected account.  Existing token/host
/// environment variables are removed first; only the child receives the
/// selected account's token and host.
pub fn run_for_account<I, S>(host: &str, login: &str, args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let token = token(host, login)?;
    let token_text = token.as_str().ok_or_else(|| GhError::MissingToken {
        host: host.to_owned(),
        login: login.to_owned(),
    })?;
    let output = sanitized_command(
        "gh",
        args,
        Some((token_environment_variable(host), token_text)),
        Some(host),
    )?;
    drop(token);
    Ok(output)
}

/// Fetch GitHub's email candidates through the selected account.  The result
/// is sorted primary-first and then alphabetically, making first-run profile
/// confirmation deterministic.
pub fn email_candidates(host: &str, login: &str) -> Result<Vec<EmailCandidate>> {
    let value = api_json(host, login, "user/emails")?;
    let array = value
        .as_array()
        .ok_or_else(|| GhError::Json("user/emails response is not an array".into()))?;
    let mut result = array
        .iter()
        .filter_map(|item| {
            let email = string_field(item, "email")?;
            Some(EmailCandidate {
                email,
                primary: bool_field(item, "primary").unwrap_or(false),
                verified: bool_field(item, "verified").unwrap_or(false),
                visibility: string_field(item, "visibility"),
                noreply: false,
            })
        })
        .collect::<Vec<_>>();
    result.sort_by_key(|candidate| {
        (
            !candidate.primary,
            !candidate.verified,
            candidate.email.clone(),
        )
    });
    Ok(result)
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EmailCandidate {
    pub email: String,
    pub primary: bool,
    pub verified: bool,
    pub visibility: Option<String>,
    pub noreply: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhUser {
    pub id: u64,
    pub login: String,
}

/// One public SSH signing key registered for the selected GitHub account.
/// Only public material is returned by GitHub's endpoint; titles and other
/// account metadata are intentionally not needed by callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhSshSigningKey {
    pub key: String,
}

/// Read the canonical login and numeric account id from GitHub's low-scope
/// `/user` endpoint. The endpoint does not require the `user:email` scope.
pub fn user_identity(host: &str, login: &str) -> Result<GhUser> {
    let value = api_json(host, login, "user")?;
    let id = value
        .get("id")
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
        .ok_or_else(|| GhError::Json("user response has no numeric id".into()))?;
    let login = string_field(&value, "login")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| GhError::Json("user response has no login".into()))?;
    Ok(GhUser { id, login })
}

/// Generate GitHub.com's ID-based noreply address. GitHub Enterprise hosts
/// are deliberately left to their server-provided email candidates because
/// they do not share github.com's address rule.
pub fn github_noreply_email(host: &str, login: &str) -> Result<Option<String>> {
    if normalize_host(host) != "github.com" {
        return Ok(None);
    }
    let user = user_identity(host, login)?;
    Ok(
        (user.id > 0)
            .then(|| format!("{}+{}@users.noreply.github.com", user.id, user.login.trim())),
    )
}

/// Build the candidates used by profile creation. The noreply candidate is
/// generated first without requiring email scope; `/user/emails` is optional
/// and quietly degrades when the selected token cannot read it.
pub fn profile_email_candidates(host: &str, login: &str) -> Result<Vec<EmailCandidate>> {
    let user = user_identity(host, login)?;
    let mut result = Vec::new();
    if normalize_host(host) == "github.com" && user.id > 0 {
        result.push(EmailCandidate {
            email: format!("{}+{}@users.noreply.github.com", user.id, user.login.trim()),
            primary: false,
            verified: true,
            visibility: Some("noreply".into()),
            noreply: true,
        });
    }
    if let Ok(mut emails) = email_candidates(host, login) {
        result.append(&mut emails);
    }
    result.sort_by_key(|candidate| {
        (
            !candidate.noreply,
            !candidate.primary,
            !candidate.verified,
            candidate.email.to_ascii_lowercase(),
        )
    });
    result.dedup_by(|left, right| left.email.eq_ignore_ascii_case(&right.email));
    Ok(result)
}

/// Read the selected account's public SSH signing keys. `/user` first binds
/// the lookup to the selected gh account and supplies its canonical login;
/// the public user endpoint then avoids requiring a signing-key read scope.
/// Neither request uploads local key material or changes token scopes.
pub fn ssh_signing_keys(host: &str, login: &str) -> Result<Vec<GhSshSigningKey>> {
    let user = user_identity(host, login)?;
    let endpoint = format!(
        "users/{}/ssh_signing_keys",
        encode_path_segment(&user.login)
    );
    let value = api_json_with_options(host, login, &endpoint, true)?;
    parse_ssh_signing_keys(&value)
}

fn encode_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

fn parse_ssh_signing_keys(value: &Value) -> Result<Vec<GhSshSigningKey>> {
    let top_level = value
        .as_array()
        .ok_or_else(|| GhError::Json("GitHub SSH signing keys response is not an array".into()))?;
    let items = if top_level.first().is_some_and(Value::is_array) {
        top_level
            .iter()
            .map(|page| {
                page.as_array().ok_or_else(|| {
                    GhError::Json("GitHub SSH signing keys pagination is malformed".into())
                })
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
    } else {
        top_level.iter().collect::<Vec<_>>()
    };
    let mut keys = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        let key = string_field(item, "key")
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| {
                GhError::Json(format!(
                    "GitHub SSH signing keys[{index}] has no public key"
                ))
            })?;
        keys.push(GhSshSigningKey { key });
    }
    Ok(keys)
}

fn api_json(host: &str, login: &str, endpoint: &str) -> Result<Value> {
    api_json_with_options(host, login, endpoint, false)
}

fn api_json_with_options(host: &str, login: &str, endpoint: &str, paginate: bool) -> Result<Value> {
    let selected = token(host, login)?;
    let token_text = selected.as_str().ok_or_else(|| GhError::MissingToken {
        host: host.to_owned(),
        login: login.to_owned(),
    })?;
    let mut args = vec!["api", endpoint];
    if paginate {
        args.extend(["--paginate", "--slurp"]);
    }
    let output = sanitized_command(
        "gh",
        args,
        Some((token_environment_variable(host), token_text)),
        Some(host),
    )?;
    if !output.status.success() {
        let error = command_error_with_secrets(format!("api {endpoint}"), &output, &[token_text]);
        drop(selected);
        return Err(error);
    }
    drop(selected);
    serde_json::from_slice(&output.stdout).map_err(|err| GhError::Json(err.to_string()))
}

pub fn normalize_host(host: &str) -> String {
    let host = host.trim();
    if let Some(address) = host.strip_prefix('[')
        && let Some((address, suffix)) = address.split_once(']')
    {
        let address = address.to_ascii_lowercase();
        return match normalized_port(suffix.strip_prefix(':')) {
            Some(None) => format!("[{address}]"),
            Some(Some(port)) => format!("[{address}]:{port}"),
            None if suffix.is_empty() => format!("[{address}]"),
            None => host.to_ascii_lowercase(),
        };
    }

    // A colon inside the left side denotes an unbracketed IPv6 literal, not a
    // hostname/port separator. URL authorities use brackets for IPv6 ports.
    if let Some((hostname, port)) = host.rsplit_once(':')
        && !hostname.contains(':')
        && let Some(port) = normalized_port(Some(port))
    {
        let hostname = hostname.trim_end_matches('.').to_ascii_lowercase();
        return port.map_or(hostname.clone(), |port| format!("{hostname}:{port}"));
    }

    if host.contains(':') {
        host.to_ascii_lowercase()
    } else {
        host.trim_end_matches('.').to_ascii_lowercase()
    }
}

/// `None` means the suffix is not a valid numeric port. `Some(None)` is the
/// canonical HTTPS port and is therefore omitted from host identity keys.
fn normalized_port(port: Option<&str>) -> Option<Option<u16>> {
    let port = port?;
    let port = port.parse::<u16>().ok()?;
    Some((port != 443).then_some(port))
}

/// Environment variable expected by gh for the selected GitHub host.
/// GitHub.com and Enterprise Cloud use GH_TOKEN; a self-hosted GHES instance
/// uses GH_ENTERPRISE_TOKEN.
pub fn token_environment_variable(host: &str) -> &'static str {
    let host = normalize_host(host);
    let host = host.strip_suffix(":443").unwrap_or(&host);
    if host == "github.com" || host.ends_with(".ghe.com") {
        "GH_TOKEN"
    } else {
        "GH_ENTERPRISE_TOKEN"
    }
}

fn sanitized_command<I, S>(
    program: &str,
    args: I,
    selected_token: Option<(&str, &str)>,
    selected_host: Option<&str>,
) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new(program);
    command.args(args);
    // GH_TOKEN/GITHUB_TOKEN can silently override the keyring account.  GH_HOST
    // can redirect a command to an enterprise host.  Clear all variants first.
    for key in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "GH_HOST",
    ] {
        command.env_remove(key);
    }
    if let Some((key, value)) = selected_token {
        command.env(key, value);
    }
    if let Some(host) = selected_host {
        command.env("GH_HOST", normalize_host(host));
    }
    command.output().map_err(Into::into)
}

fn trim_ascii_newline(mut bytes: Vec<u8>) -> Vec<u8> {
    while matches!(bytes.last(), Some(b'\n' | b'\r' | b' ' | b'\t')) {
        bytes.pop();
    }
    bytes
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn bool_field(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

fn command_error(operation: impl Into<String>, output: &Output) -> GhError {
    command_error_with_secrets(operation, output, &[])
}

fn command_error_with_secrets(
    operation: impl Into<String>,
    output: &Output,
    secrets: &[&str],
) -> GhError {
    GhError::Command {
        operation: operation.into(),
        code: output.status.code(),
        stderr: sanitize_command_stderr_with_secrets(&output.stderr, secrets),
    }
}

fn sanitize_command_stderr_with_secrets(stderr: &[u8], secrets: &[&str]) -> String {
    let printable = String::from_utf8_lossy(stderr)
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let compact = printable.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut redacted = redact_github_tokens(&compact);
    for secret in secrets.iter().copied().filter(|secret| !secret.is_empty()) {
        redacted = redacted.replace(secret, "[REDACTED]");
    }

    // Error output is untrusted. If it labels any remaining value as a
    // credential, discard the rest of the diagnostic instead of guessing
    // where an arbitrary enterprise token ends.
    let lower = redacted.to_ascii_lowercase();
    if let Some((position, marker_len)) = [
        "authorization:",
        "authorization=",
        "access_token:",
        "access_token=",
        "token:",
        "token=",
        "password:",
        "password=",
    ]
    .iter()
    .filter_map(|marker| lower.find(marker).map(|position| (position, marker.len())))
    .min_by_key(|(position, _)| *position)
    {
        redacted.truncate(position + marker_len);
        redacted.push_str("[REDACTED]");
    }

    let mut chars = redacted.chars();
    let limited = chars
        .by_ref()
        .take(MAX_COMMAND_ERROR_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        format!("{limited}...")
    } else {
        limited
    }
}

fn redact_github_tokens(input: &str) -> String {
    const PREFIXES: [&str; 7] = [
        "github_pat_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "ghv_",
    ];

    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    while cursor < input.len() {
        let next = PREFIXES
            .iter()
            .filter_map(|prefix| {
                input[cursor..]
                    .find(prefix)
                    .map(|offset| (cursor + offset, *prefix))
            })
            .min_by_key(|(position, _)| *position);
        let Some((start, prefix)) = next else {
            output.push_str(&input[cursor..]);
            break;
        };
        output.push_str(&input[cursor..start]);
        let mut end = start + prefix.len();
        for (offset, character) in input[end..].char_indices() {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                end = start + prefix.len() + offset + character.len_utf8();
            } else {
                break;
            }
        }
        output.push_str("[REDACTED]");
        cursor = end;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_verified_and_offline_accounts() {
        let json = r#"{"hosts":{"github.com":[{"host":"github.com","login":"alice","active":true,"tokenSource":"keyring","gitProtocol":"https"},{"host":"github.com","login":"bob","active":false,"state":"error","error":"offline"},{"host":"github.com","login":"carol","state":"unknown"}]}}"#;
        let accounts = parse_auth_status_json(json).unwrap();
        assert_eq!(accounts.len(), 3);
        assert!(accounts[0].verified);
        assert!(!accounts[1].verified);
        assert_eq!(accounts[1].error.as_deref(), Some("offline"));
        assert!(!accounts[2].verified);
    }

    #[test]
    fn failure_state_is_kept_as_an_offline_account() {
        let accounts = parse_auth_status_json(
            r#"{"hosts":{"git.example.com":[{"login":"alice","state":"failure"}]}}"#,
        )
        .unwrap();

        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].host, "git.example.com");
        assert_eq!(accounts[0].state.as_deref(), Some("failure"));
        assert!(!accounts[0].verified);
    }

    #[test]
    fn token_environment_matches_the_gh_host_class() {
        assert_eq!(token_environment_variable("GitHub.com"), "GH_TOKEN");
        assert_eq!(token_environment_variable("github.com:443"), "GH_TOKEN");
        assert_eq!(token_environment_variable("acme.ghe.com"), "GH_TOKEN");
        assert_eq!(
            token_environment_variable("github.company.test"),
            "GH_ENTERPRISE_TOKEN"
        );
    }

    #[test]
    fn known_enterprise_token_is_redacted_without_a_standard_prefix() {
        let sanitized = sanitize_command_stderr_with_secrets(
            b"server echoed opaque-enterprise-secret while failing",
            &["opaque-enterprise-secret"],
        );
        assert!(!sanitized.contains("opaque-enterprise-secret"));
        assert!(sanitized.contains("[REDACTED]"));
    }

    #[test]
    fn discovery_cache_roundtrips_without_secrets() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(DISCOVERY_CACHE_FILENAME);
        let discovery = GhDiscovery {
            accounts: vec![GhAccount {
                host: "github.com".into(),
                login: "alice".into(),
                verified: true,
                token_source: Some("keyring".into()),
                ..GhAccount::default()
            }],
            command_succeeded: true,
            offline: false,
        };

        save_discovery_cache(&path, &discovery).unwrap();
        assert_eq!(load_discovery_cache(&path).unwrap(), Some(discovery));
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.to_ascii_lowercase().contains("token\""));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            let cached = load_discovery_cache(&path).unwrap().unwrap();
            save_discovery_cache(&path, &cached).unwrap();
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn token_debug_is_redacted_and_trimmed() {
        let token = SecretToken::new(b"secret\n".to_vec());
        assert_eq!(token.as_str(), Some("secret\n"));
        assert!(!format!("{token:?}").contains("secret"));
        assert_eq!(
            trim_ascii_newline(token.into_bytes().to_vec()),
            b"secret".to_vec()
        );
    }

    #[test]
    fn command_stderr_is_compact_redacted_and_bounded() {
        let stderr = format!(
            "\u{1b}[31mrequest failed\u{1b}[0m for ghp_{}\nAuthorization: Bearer plain-secret {}",
            "a".repeat(80),
            "x".repeat(MAX_COMMAND_ERROR_CHARS * 2)
        );
        let sanitized = sanitize_command_stderr_with_secrets(stderr.as_bytes(), &[]);

        assert!(!sanitized.contains("ghp_"));
        assert!(!sanitized.contains("plain-secret"));
        assert!(!sanitized.contains('\u{1b}'));
        assert!(sanitized.contains("[REDACTED]"));
        assert!(sanitized.chars().count() <= MAX_COMMAND_ERROR_CHARS + 3);
    }

    #[test]
    fn normalizes_host() {
        assert_eq!(normalize_host(" GitHub.com. "), "github.com");
        assert_eq!(normalize_host(" GitHub.com.:443 "), "github.com");
        assert_eq!(normalize_host("Git.Example.:8443"), "git.example:8443");
        assert_eq!(normalize_host("[2001:DB8::1]:443"), "[2001:db8::1]");
        assert_eq!(normalize_host("[2001:DB8::1]:8443"), "[2001:db8::1]:8443");
        assert_eq!(normalize_host("2001:DB8::1"), "2001:db8::1");
    }

    #[test]
    fn signing_key_api_response_requires_public_key_material() {
        let valid = serde_json::json!([
            {"id": 1, "key": "ssh-ed25519 AAAATEST signing"},
            {"id": 2, "key": "ssh-rsa AAAAOTHER another"}
        ]);
        let keys = parse_ssh_signing_keys(&valid).unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].key, "ssh-ed25519 AAAATEST signing");

        let paginated = serde_json::json!([
            [{"id": 1, "key": "ssh-ed25519 AAAAONE first"}],
            [{"id": 2, "key": "ssh-ed25519 AAAATWO second"}]
        ]);
        assert_eq!(parse_ssh_signing_keys(&paginated).unwrap().len(), 2);

        assert!(parse_ssh_signing_keys(&serde_json::json!([{"id": 1}])).is_err());
        assert_eq!(
            encode_path_segment("Alice Example/管理员"),
            "Alice%20Example%2F%E7%AE%A1%E7%90%86%E5%91%98"
        );
    }
}
