//! Configuration model, persistence and deterministic rule resolution.
//!
//! The configuration is intentionally ordinary data.  Git repository state is
//! kept in the repository's `.git/config`; this module only owns user-level
//! data and the cache/state locations derived from XDG variables.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use fd_lock::RwLock;
use globset::{Glob, GlobBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use toml_edit::{ArrayOfTables, DocumentMut, Item};

/// Current on-disk schema version.
pub const CONFIG_VERSION: u32 = 1;

/// User-facing configuration paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    /// `${XDG_CONFIG_HOME:-$HOME/.config}/ghis`.
    pub config_dir: PathBuf,
    /// The TOML configuration file.
    pub config_file: PathBuf,
    /// `${XDG_CACHE_HOME:-$HOME/.cache}/ghis` (or a config-specific namespace
    /// below it when `--config` points at a custom file).
    pub cache_dir: PathBuf,
    /// `${XDG_STATE_HOME:-$HOME/.local/state}/ghis` (or a config-specific
    /// namespace below it for a custom `--config`).
    pub state_dir: PathBuf,
    /// Generated Git config fragments.
    pub fragments_dir: PathBuf,
    /// Redacted diagnostic log.
    pub log_file: PathBuf,
    /// Disposable index of repositories previously bound by ghis.
    pub repositories_file: PathBuf,
}

impl ConfigPaths {
    /// Resolve paths from the process environment.
    pub fn discover() -> Result<Self, ConfigError> {
        let directories = crate::platform::UserDirectories::discover()
            .map_err(|error| ConfigError::Paths(error.to_string()))?;
        Ok(Self::from_bases(
            directories.config_base,
            directories.cache_base,
            directories.state_base,
        ))
    }

    /// Build paths under explicit XDG base directories.  This is useful for
    /// tests and callers that already resolved their environment.
    pub fn from_bases(
        config_base: impl Into<PathBuf>,
        cache_base: impl Into<PathBuf>,
        state_base: impl Into<PathBuf>,
    ) -> Self {
        let config_dir = config_base.into().join("ghis");
        let cache_dir = cache_base.into().join("ghis");
        let state_dir = state_base.into().join("ghis");
        Self {
            config_file: config_dir.join("config.toml"),
            fragments_dir: config_dir.join("fragments"),
            log_file: state_dir.join("ghis.log"),
            repositories_file: state_dir.join("repositories.json"),
            config_dir,
            cache_dir,
            state_dir,
        }
    }

    /// Create directories used by ghis.  Cache and state are deliberately
    /// optional at runtime, so callers may choose to create only `config_dir`.
    pub fn create_dirs(&self) -> io::Result<()> {
        fs::create_dir_all(&self.config_dir)?;
        fs::create_dir_all(&self.fragments_dir)?;
        fs::create_dir_all(&self.cache_dir)?;
        fs::create_dir_all(&self.state_dir)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            for directory in [
                &self.config_dir,
                &self.fragments_dir,
                &self.cache_dir,
                &self.state_dir,
            ] {
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
            }
        }

        Ok(())
    }

    /// Select an explicit config file and make its path independent of later
    /// working-directory changes by generated Git helpers and hooks. Custom
    /// files also receive one stable namespace for fragments, cache, and
    /// state, so multiple config files cannot share side-channel data.
    pub fn set_config_file(&mut self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let path = path.as_ref();
        self.config_file = std::path::absolute(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let default_file =
            std::path::absolute(self.config_dir.join("config.toml")).map_err(|source| {
                ConfigError::Io {
                    path: self.config_dir.clone(),
                    source,
                }
            })?;
        let fragments = self.config_dir.join("fragments");
        let cache_base = config_namespace_base(&self.cache_dir);
        let state_base = config_namespace_base(&self.state_dir);
        if self.config_file == default_file {
            self.fragments_dir = fragments;
            self.cache_dir = cache_base;
            self.state_dir = state_base;
        } else {
            let namespace = config_namespace(&self.config_file);
            self.fragments_dir = fragments.join(&namespace);
            self.cache_dir = cache_base.join(&namespace);
            self.state_dir = state_base.join(&namespace);
        }
        self.log_file = self.state_dir.join("ghis.log");
        self.repositories_file = self.state_dir.join("repositories.json");
        Ok(())
    }
}

/// Return the stable directory name used for an explicit configuration file.
///
/// A custom `--config` must isolate every ghis-owned side channel: generated
/// Git fragments, account discovery cache, and repository/audit state. Keep
/// the namespace derived from the absolute path so two files with the same
/// basename cannot share data.
fn config_namespace(path: &Path) -> String {
    let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
    let mut namespace = String::with_capacity(7 + 64);
    namespace.push_str("config-");
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut namespace, "{byte:02x}").expect("writing a digest to a string cannot fail");
    }
    namespace
}

/// Strip a namespace previously added by [`ConfigPaths::set_config_file`].
/// This keeps repeated calls on one `ConfigPaths` value idempotent and lets a
/// caller switch back from a custom file to the default XDG configuration.
fn config_namespace_base(path: &Path) -> PathBuf {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return path.to_path_buf();
    };
    let Some(digest) = name.strip_prefix("config-") else {
        return path.to_path_buf();
    };
    if digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    }
}

/// Policy for a repository whose profile cannot be resolved uniquely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum UnresolvedPolicy {
    /// Print a warning and run the underlying command unchanged.
    #[serde(alias = "warn_and_continue")]
    #[default]
    WarnAndContinue,
    /// Stop before a command that would mutate repository state.
    Fail,
}

/// Policy for a selected profile whose credential is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialFailurePolicy {
    /// Never silently use another account.
    #[serde(alias = "fail")]
    #[default]
    Fail,
}

/// Policy for SSH remotes that have no ghis-managed key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SshUnmanagedPolicy {
    #[default]
    #[serde(alias = "warn_and_continue")]
    WarnAndContinue,
    Fail,
}

/// When an operation banner should be displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DisplayIdentity {
    Always,
    #[serde(alias = "sensitive_commands")]
    #[default]
    SensitiveCommands,
    Never,
}

/// Behavior settings that do not belong to a particular profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Behavior {
    pub default_profile: Option<String>,
    pub auto_bind: bool,
    pub unresolved: UnresolvedPolicy,
    pub credential_failure: CredentialFailurePolicy,
    pub ssh_unmanaged: SshUnmanagedPolicy,
    pub display_identity: DisplayIdentity,
    pub display_profile_on_chpwd: bool,
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            default_profile: None,
            auto_bind: true,
            unresolved: UnresolvedPolicy::default(),
            credential_failure: CredentialFailurePolicy::default(),
            ssh_unmanaged: SshUnmanagedPolicy::default(),
            display_identity: DisplayIdentity::default(),
            display_profile_on_chpwd: false,
        }
    }
}

/// SSH material associated with a profile.  Private keys are intentionally
/// not representable in this struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SshProfile {
    pub mode: SshMode,
    pub public_key: Option<PathBuf>,
    pub fingerprint: Option<String>,
    pub agent_socket: Option<PathBuf>,
    /// Explicit jump hosts used only by managed SSH transport.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub proxy_jump: Vec<String>,
    /// Explicitly forward the selected agent through managed SSH transport.
    #[serde(skip_serializing_if = "bool_is_false")]
    pub forward_agent: bool,
}

impl Default for SshProfile {
    fn default() -> Self {
        Self {
            mode: SshMode::External,
            public_key: None,
            fingerprint: None,
            agent_socket: None,
            proxy_jump: Vec::new(),
            forward_agent: false,
        }
    }
}

fn bool_is_false(value: &bool) -> bool {
    !*value
}

fn managed_proxy_jump_is_valid(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value.len() <= 255
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'-' | b'_' | b'@' | b':' | b'[' | b']')
        })
}

/// How a profile's SSH connection is supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SshMode {
    #[default]
    External,
    OnePassword,
    Managed,
}

/// SSH commit-signing agent source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SigningTransport {
    /// Discover a local 1Password/system agent and signing program as before.
    #[default]
    LocalAgent,
    /// Use only the SSH agent forwarded into this process.
    ForwardedAgent,
}

/// Optional SSH commit-signing settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SigningProfile {
    pub enabled: bool,
    pub transport: SigningTransport,
    pub signing_key: Option<String>,
    pub fingerprint: Option<String>,
    pub program: Option<PathBuf>,
}

/// A GitHub/Git commit identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub host: String,
    pub login: String,
    pub git_name: String,
    pub git_email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub ssh: Option<SshProfile>,
    pub signing: SigningProfile,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            host: "github.com".into(),
            login: String::new(),
            git_name: String::new(),
            git_email: String::new(),
            description: None,
            ssh: None,
            signing: SigningProfile::default(),
        }
    }
}

/// A deterministic repository matching rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Rule {
    pub id: String,
    pub profile: String,
    pub priority: i32,
    pub host: Option<String>,
    pub owner: Option<String>,
    pub repo: Option<String>,
    pub remote: Option<String>,
    pub gitdir: Option<String>,
    pub cwd: Option<String>,
}

/// Top-level ghis configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub behavior: Behavior,
    pub profiles: BTreeMap<String, Profile>,
    pub rules: Vec<Rule>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            behavior: Behavior::default(),
            profiles: BTreeMap::new(),
            rules: Vec::new(),
        }
    }
}

/// Configuration persistence and validation errors.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration path error: {0}")]
    Paths(String),
    /// An explicitly requested configuration file does not exist.  Unlike the
    /// default XDG path, this is an operator mistake and must not silently
    /// become an empty configuration.
    #[error("指定的配置文件不存在：{path}")]
    Missing { path: PathBuf },
    #[error("I/O error while accessing {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("invalid TOML in {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml_edit::de::Error,
    },
    #[error("could not serialize configuration: {0}")]
    Serialize(#[from] toml_edit::ser::Error),
    #[error("configuration validation failed: {0}")]
    Validation(String),
    #[error("could not acquire configuration lock: {0}")]
    Lock(#[source] io::Error),
}

impl Config {
    /// Load configuration from a TOML file.  A missing file produces defaults.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Self::load_with_mode(path, LoadMode::AllowMissing)
    }

    /// Load configuration from a TOML file, requiring the file to exist.
    ///
    /// Use this for explicitly selected configuration files (`--config` or
    /// `GHIS_CONFIG`): a missing file is an operator mistake there and must
    /// not silently become an empty default configuration.
    pub fn load_required(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Self::load_with_mode(path, LoadMode::RequireExisting)
    }

    fn load_with_mode(path: impl AsRef<Path>, mode: LoadMode) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let mut input = String::new();
        match File::open(path) {
            Ok(mut file) => {
                file.read_to_string(&mut input)
                    .map_err(|source| ConfigError::Io {
                        path: path.to_path_buf(),
                        source,
                    })?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => match mode {
                LoadMode::AllowMissing => return Ok(Self::default()),
                LoadMode::RequireExisting => {
                    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
                    return Err(ConfigError::Missing { path: resolved });
                }
            },
            Err(source) => {
                return Err(ConfigError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
        let config: Self =
            toml_edit::de::from_str(&input).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        config.validate()?;
        Ok(config)
    }

    /// Convenience wrapper loading the standard XDG configuration file.
    pub fn load_default() -> Result<Self, ConfigError> {
        Self::load(ConfigPaths::discover()?.config_file)
    }

    /// Save configuration with a lock and an atomic same-directory rename.
    /// Existing comments and unknown keys are preserved where possible.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        self.validate()?;
        let path = path.as_ref();
        with_config_write_lock(path, || {
            let document = load_document(path)?;
            write_document(path, document, self)
        })
    }

    /// Update the latest on-disk configuration while holding its write lock.
    ///
    /// Callers should use this for read-modify-write operations. Loading a
    /// [`Config`] first and later calling [`Self::save`] is a full replacement
    /// and can intentionally overwrite changes made in between.
    pub fn update<T, E, F>(path: impl AsRef<Path>, update: F) -> std::result::Result<(Self, T), E>
    where
        E: From<ConfigError>,
        F: FnOnce(&mut Self) -> std::result::Result<T, E>,
    {
        let path = path.as_ref();
        with_config_write_lock(path, || {
            let (document, mut config) = load_document_and_config(path).map_err(E::from)?;
            let result = update(&mut config)?;
            config.validate().map_err(E::from)?;
            write_document(path, document, &config).map_err(E::from)?;
            Ok((config, result))
        })
    }

    /// Convenience wrapper saving the standard XDG configuration file.
    pub fn save_default(&self) -> Result<(), ConfigError> {
        Self::save(self, ConfigPaths::discover()?.config_file)
    }

    /// Validate references and invariants before writing or using the config.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version == 0 || self.version > CONFIG_VERSION {
            return Err(ConfigError::Validation(format!(
                "unsupported version {}; expected <= {}",
                self.version, CONFIG_VERSION
            )));
        }
        if let Some(default) = &self.behavior.default_profile
            && !self.profiles.contains_key(default)
        {
            return Err(ConfigError::Validation(format!(
                "default_profile refers to missing profile `{default}`"
            )));
        }
        let mut ids = BTreeSet::new();
        for (id, profile) in &self.profiles {
            if id.trim().is_empty() {
                return Err(ConfigError::Validation("profile id cannot be empty".into()));
            }
            if profile.host.trim().is_empty() || profile.login.trim().is_empty() {
                return Err(ConfigError::Validation(format!(
                    "profile `{id}` needs host and login"
                )));
            }
            if profile.git_name.trim().is_empty() || profile.git_email.trim().is_empty() {
                return Err(ConfigError::Validation(format!(
                    "profile `{id}` needs git_name and git_email"
                )));
            }
            if let Some(description) = profile.description.as_deref() {
                if description.trim().is_empty() {
                    return Err(ConfigError::Validation(format!(
                        "profile `{id}` description cannot be empty"
                    )));
                }
                if description.chars().any(char::is_control) {
                    return Err(ConfigError::Validation(format!(
                        "profile `{id}` description cannot contain control characters"
                    )));
                }
                if description.chars().count() > 200 {
                    return Err(ConfigError::Validation(format!(
                        "profile `{id}` description cannot exceed 200 characters"
                    )));
                }
            }
            if profile.ssh.as_ref().is_some_and(|ssh| {
                matches!(ssh.mode, SshMode::OnePassword | SshMode::Managed)
                    && ssh
                        .public_key
                        .as_ref()
                        .is_none_or(|path| path.as_os_str().is_empty())
            }) {
                return Err(ConfigError::Validation(format!(
                    "profile `{id}` with managed SSH needs public_key"
                )));
            }
            if let Some(ssh) = profile.ssh.as_ref() {
                if matches!(ssh.mode, SshMode::External)
                    && (!ssh.proxy_jump.is_empty() || ssh.forward_agent)
                {
                    return Err(ConfigError::Validation(format!(
                        "profile `{id}` can use proxy_jump/forward_agent only with managed or one-password SSH"
                    )));
                }
                if ssh.proxy_jump.len() > 8
                    || ssh
                        .proxy_jump
                        .iter()
                        .any(|jump| !managed_proxy_jump_is_valid(jump))
                {
                    return Err(ConfigError::Validation(format!(
                        "profile `{id}` has an invalid managed SSH proxy_jump"
                    )));
                }
            }
            if profile
                .signing
                .fingerprint
                .as_deref()
                .is_some_and(|fingerprint| fingerprint.trim().is_empty())
            {
                return Err(ConfigError::Validation(format!(
                    "profile `{id}` signing fingerprint cannot be empty"
                )));
            }
            if matches!(profile.signing.transport, SigningTransport::ForwardedAgent)
                && profile.signing.enabled
                && profile
                    .signing
                    .signing_key
                    .as_deref()
                    .is_none_or(str::is_empty)
                && profile
                    .signing
                    .fingerprint
                    .as_deref()
                    .is_none_or(str::is_empty)
                && profile
                    .ssh
                    .as_ref()
                    .and_then(|ssh| ssh.public_key.as_ref())
                    .is_none_or(|path| path.as_os_str().is_empty())
            {
                return Err(ConfigError::Validation(format!(
                    "profile `{id}` with forwarded-agent signing needs signing_key, signing fingerprint, or SSH public_key"
                )));
            }
        }
        for rule in &self.rules {
            if rule.id.trim().is_empty() {
                return Err(ConfigError::Validation("rule id cannot be empty".into()));
            }
            if !ids.insert(rule.id.clone()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate rule id `{}`",
                    rule.id
                )));
            }
            if !self.profiles.contains_key(&rule.profile) {
                return Err(ConfigError::Validation(format!(
                    "rule `{}` refers to missing profile `{}`",
                    rule.id, rule.profile
                )));
            }
            if rule
                .remote
                .as_deref()
                .is_some_and(|pattern| build_rule_glob(pattern).is_err())
            {
                return Err(ConfigError::Validation(format!(
                    "规则 `{}` 的 `remote` glob 模式无效",
                    rule.id
                )));
            }
            if rule
                .gitdir
                .as_deref()
                .is_some_and(|pattern| build_rule_glob(&expand_home(pattern)).is_err())
            {
                return Err(ConfigError::Validation(format!(
                    "规则 `{}` 的 `gitdir` glob 模式无效",
                    rule.id
                )));
            }
            if rule
                .cwd
                .as_deref()
                .is_some_and(|pattern| build_rule_glob(&expand_home(pattern)).is_err())
            {
                return Err(ConfigError::Validation(format!(
                    "规则 `{}` 的 `cwd` glob 模式无效",
                    rule.id
                )));
            }
        }
        Ok(())
    }
}

/// How [`Config`] treats a configuration file that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadMode {
    /// The default XDG path may be missing on first use: treat it as empty.
    AllowMissing,
    /// An explicitly selected path must exist; report the resolved absolute
    /// location instead of falling back to defaults.
    RequireExisting,
}

const KNOWN_ROOT_KEYS: &[&str] = &["version", "behavior", "profiles", "rules"];
const KNOWN_BEHAVIOR_TABLE_KEYS: &[&str] = &[
    "default_profile",
    "auto_bind",
    "unresolved",
    "credential_failure",
    "ssh_unmanaged",
    "display_identity",
    "display_profile_on_chpwd",
];
const KNOWN_PROFILE_KEYS: &[&str] = &[
    "host",
    "login",
    "git_name",
    "git_email",
    "description",
    "ssh",
    "signing",
];
const KNOWN_SSH_KEYS: &[&str] = &[
    "mode",
    "public_key",
    "fingerprint",
    "agent_socket",
    "proxy_jump",
    "forward_agent",
];
const KNOWN_SIGNING_KEYS: &[&str] = &[
    "enabled",
    "transport",
    "signing_key",
    "fingerprint",
    "program",
];
const KNOWN_RULE_KEYS: &[&str] = &[
    "id", "profile", "priority", "host", "owner", "repo", "remote", "gitdir", "cwd",
];

/// Report dotted paths of keys that [`Config`] does not model.
///
/// Unknown keys stay legal: loading ignores them and saving preserves them
/// for forward compatibility.  This scan exists so `ghis doctor` can point
/// at likely typos instead of silently dropping user intent.  A nested table
/// under an unknown parent is reported once at the parent path.
pub fn unknown_config_keys(document: &DocumentMut) -> Vec<String> {
    let mut unknown = Vec::new();
    let root = document.as_table();
    for (key, _) in root.iter() {
        if KNOWN_ROOT_KEYS.contains(&key) {
            continue;
        }
        unknown.push(key.to_owned());
    }
    if let Some(behavior) = root.get("behavior").and_then(Item::as_table) {
        for key in behavior.iter().map(|(key, _)| key) {
            if !KNOWN_BEHAVIOR_TABLE_KEYS.contains(&key) {
                unknown.push(format!("behavior.{key}"));
            }
        }
    }
    if let Some(profiles) = root.get("profiles").and_then(Item::as_table) {
        for (id, profile_item) in profiles.iter() {
            let Some(profile) = profile_item.as_table() else {
                continue;
            };
            for key in profile.iter().map(|(key, _)| key) {
                if !KNOWN_PROFILE_KEYS.contains(&key) {
                    unknown.push(format!("profiles.{id}.{key}"));
                }
            }
            if let Some(ssh) = profile.get("ssh").and_then(Item::as_table) {
                for key in ssh.iter().map(|(key, _)| key) {
                    if !KNOWN_SSH_KEYS.contains(&key) {
                        unknown.push(format!("profiles.{id}.ssh.{key}"));
                    }
                }
            }
            if let Some(signing) = profile.get("signing").and_then(Item::as_table) {
                for key in signing.iter().map(|(key, _)| key) {
                    if !KNOWN_SIGNING_KEYS.contains(&key) {
                        unknown.push(format!("profiles.{id}.signing.{key}"));
                    }
                }
            }
        }
    }
    if let Some(rules) = root.get("rules").and_then(Item::as_array_of_tables) {
        for (index, rule) in rules.iter().enumerate() {
            for key in rule.iter().map(|(key, _)| key) {
                if !KNOWN_RULE_KEYS.contains(&key) {
                    unknown.push(format!("rules[{index}].{key}"));
                }
            }
        }
    }
    unknown.sort();
    unknown.dedup();
    unknown
}

/// Scan the configuration file on disk for unknown keys.
///
/// A missing or unparseable file yields no findings here; both conditions are
/// reported by the normal load path with clearer messages.
pub fn scan_unknown_config_keys(path: impl AsRef<Path>) -> Vec<String> {
    let Ok(input) = fs::read_to_string(path.as_ref()) else {
        return Vec::new();
    };
    match input.parse::<DocumentMut>() {
        Ok(document) => unknown_config_keys(&document),
        Err(_) => Vec::new(),
    }
}

fn config_lock_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "{}lock",
        path.extension()
            .and_then(|value| value.to_str())
            .map(|value| format!("{value}."))
            .unwrap_or_default()
    ))
}

fn with_config_write_lock<T, E, F>(path: &Path, operation: F) -> std::result::Result<T, E>
where
    E: From<ConfigError>,
    F: FnOnce() -> std::result::Result<T, E>,
{
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|source| ConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })
        .map_err(E::from)?;
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(config_lock_path(path))
        .map_err(ConfigError::Lock)
        .map_err(E::from)?;
    let mut lock = RwLock::new(lock_file);
    let _guard = lock.write().map_err(ConfigError::Lock).map_err(E::from)?;
    operation()
}

fn load_document(path: &Path) -> Result<DocumentMut, ConfigError> {
    match fs::read_to_string(path) {
        Ok(input) => input
            .parse::<DocumentMut>()
            .map_err(|error| ConfigError::Validation(format!("invalid TOML: {error}"))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(source) => Err(ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn load_document_and_config(path: &Path) -> Result<(DocumentMut, Config), ConfigError> {
    let input = match fs::read_to_string(path) {
        Ok(input) => input,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((DocumentMut::new(), Config::default()));
        }
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let config: Config = toml_edit::de::from_str(&input).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    config.validate()?;
    let document = input
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Validation(format!("invalid TOML: {error}")))?;
    Ok((document, config))
}

fn write_document(
    path: &Path,
    mut document: DocumentMut,
    config: &Config,
) -> Result<(), ConfigError> {
    let replacement_text = toml_edit::ser::to_string_pretty(config)?;
    let replacement = replacement_text
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Validation(format!("invalid generated TOML: {error}")))?;
    merge_document(&mut document, &replacement);
    crate::platform::atomic_write(path, document.to_string().as_bytes()).map_err(|source| {
        ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

fn merge_document(dst: &mut DocumentMut, src: &DocumentMut) {
    for (key, source_item) in src.iter() {
        if let Some(destination_item) = dst.get_mut(key) {
            match key {
                "profiles" => merge_profiles(destination_item, source_item),
                "rules" => merge_rules(destination_item, source_item),
                "behavior" => merge_known_table(
                    destination_item,
                    source_item,
                    &[
                        "default_profile",
                        "auto_bind",
                        "unresolved",
                        "credential_failure",
                        "ssh_unmanaged",
                        "display_identity",
                        "display_profile_on_chpwd",
                    ],
                ),
                _ => merge_item(destination_item, source_item),
            }
        } else {
            dst[key] = source_item.clone();
        }
    }
}

fn merge_rules(destination: &mut Item, source: &Item) {
    let Item::ArrayOfTables(destination_tables) = destination else {
        return merge_item(destination, source);
    };
    let Item::ArrayOfTables(source_tables) = source else {
        return merge_item(destination, source);
    };

    let mut merged = ArrayOfTables::new();
    for source_table in source_tables {
        let source_id = source_table.get("id").and_then(Item::as_str);
        let existing = source_id.and_then(|id| {
            destination_tables
                .iter()
                .find(|table| table.get("id").and_then(Item::as_str) == Some(id))
        });
        let mut table = existing.cloned().unwrap_or_else(|| source_table.clone());
        if existing.is_some() {
            let mut destination_item = Item::Table(table);
            let source_item = Item::Table(source_table.clone());
            merge_known_table(
                &mut destination_item,
                &source_item,
                &[
                    "id", "profile", "priority", "host", "owner", "repo", "remote", "gitdir", "cwd",
                ],
            );
            table = destination_item
                .into_table()
                .expect("merging two rule tables keeps a table");
        }
        merged.push(table);
    }
    *destination_tables = merged;
}

fn merge_profiles(destination: &mut Item, source: &Item) {
    let Item::Table(destination_table) = destination else {
        return merge_item(destination, source);
    };
    let Item::Table(source_table) = source else {
        return merge_item(destination, source);
    };
    let stale = destination_table
        .iter()
        .filter(|(key, _)| !source_table.contains_key(key))
        .map(|(key, _)| key.to_owned())
        .collect::<Vec<_>>();
    for key in stale {
        destination_table.remove(&key);
    }
    for (key, source_item) in source_table.iter() {
        if let Some(destination_item) = destination_table.get_mut(key) {
            merge_profile(destination_item, source_item);
        } else {
            destination_table.insert(key, source_item.clone());
        }
    }
}

fn merge_profile(destination: &mut Item, source: &Item) {
    merge_known_table(
        destination,
        source,
        &[
            "host",
            "login",
            "git_name",
            "git_email",
            "description",
            "ssh",
            "signing",
        ],
    );
    let Item::Table(destination) = destination else {
        return;
    };
    let Item::Table(source) = source else {
        return;
    };
    if let (Some(destination), Some(source)) = (destination.get_mut("ssh"), source.get("ssh")) {
        merge_known_table(
            destination,
            source,
            &[
                "mode",
                "public_key",
                "fingerprint",
                "agent_socket",
                "proxy_jump",
                "forward_agent",
            ],
        );
    }
    if let (Some(destination), Some(source)) =
        (destination.get_mut("signing"), source.get("signing"))
    {
        merge_known_table(
            destination,
            source,
            &[
                "enabled",
                "transport",
                "signing_key",
                "fingerprint",
                "program",
            ],
        );
    }
}

fn merge_known_table(destination: &mut Item, source: &Item, known_keys: &[&str]) {
    let Item::Table(destination_table) = destination else {
        return merge_item(destination, source);
    };
    let Item::Table(source_table) = source else {
        return merge_item(destination, source);
    };
    for key in known_keys {
        if !source_table.contains_key(key) {
            destination_table.remove(key);
        }
    }
    for (key, source_item) in source_table.iter() {
        if let Some(destination_item) = destination_table.get_mut(key) {
            merge_item(destination_item, source_item);
        } else {
            destination_table.insert(key, source_item.clone());
        }
    }
}

fn merge_item(destination: &mut Item, source: &Item) {
    match (destination, source) {
        (Item::Table(destination), Item::Table(source)) => {
            for (key, source_item) in source.iter() {
                if let Some(destination_item) = destination.get_mut(key) {
                    merge_item(destination_item, source_item);
                } else {
                    destination.insert(key, source_item.clone());
                }
            }
        }
        (Item::Value(destination), Item::Value(source)) => {
            let decor = destination.decor().clone();
            let mut replacement = source.clone();
            *replacement.decor_mut() = decor;
            *destination = replacement;
        }
        (destination, source) => *destination = source.clone(),
    }
}

/// Values available to a rule matcher for one operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleContext {
    pub host: Option<String>,
    pub owner: Option<String>,
    pub repo: Option<String>,
    pub remote: Option<String>,
    pub gitdir: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
}

impl RuleContext {
    /// Construct a context from string-like values.
    pub fn new(
        host: Option<impl Into<String>>,
        owner: Option<impl Into<String>>,
        repo: Option<impl Into<String>>,
        remote: Option<impl Into<String>>,
        gitdir: Option<impl Into<PathBuf>>,
    ) -> Self {
        Self {
            host: host.map(Into::into),
            owner: owner.map(Into::into),
            repo: repo.map(Into::into),
            remote: remote.map(Into::into),
            gitdir: gitdir.map(Into::into),
            cwd: None,
        }
    }
}

/// Result of applying the highest-priority rule level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleResolution {
    None,
    Match {
        rule_id: String,
        profile: String,
        priority: i32,
    },
    Ambiguous {
        priority: i32,
        rule_ids: Vec<String>,
        profiles: Vec<String>,
    },
}

/// Match rules and resolve a profile.  Every condition written on a rule must
/// match; only the highest priority is considered.
pub fn resolve_rule(rules: &[Rule], context: &RuleContext) -> RuleResolution {
    let mut matches: Vec<&Rule> = rules
        .iter()
        .filter(|rule| rule_matches(rule, context))
        .collect();
    let Some(highest) = matches.iter().map(|rule| rule.priority).max() else {
        return RuleResolution::None;
    };
    matches.retain(|rule| rule.priority == highest);
    let mut profiles = matches
        .iter()
        .map(|r| r.profile.clone())
        .collect::<Vec<_>>();
    profiles.sort();
    profiles.dedup();
    let mut rule_ids = matches.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
    rule_ids.sort();
    if profiles.len() == 1 {
        RuleResolution::Match {
            rule_id: rule_ids[0].clone(),
            profile: profiles[0].clone(),
            priority: highest,
        }
    } else {
        RuleResolution::Ambiguous {
            priority: highest,
            rule_ids,
            profiles,
        }
    }
}

/// Return whether one rule matches a repository context.
pub fn rule_matches(rule: &Rule, context: &RuleContext) -> bool {
    rule.host.as_deref().is_none_or(|wanted| {
        context.host.as_deref().is_some_and(|actual| {
            crate::github::normalize_host(wanted) == crate::github::normalize_host(actual)
        })
    }) && rule.owner.as_deref().is_none_or(|wanted| {
        context
            .owner
            .as_deref()
            .is_some_and(|actual| wanted.eq_ignore_ascii_case(actual))
    }) && rule.repo.as_deref().is_none_or(|wanted| {
        context
            .repo
            .as_deref()
            .is_some_and(|actual| wanted.eq_ignore_ascii_case(actual))
    }) && rule.remote.as_deref().is_none_or(|pattern| {
        context
            .remote
            .as_deref()
            .is_some_and(|actual| glob_matches(pattern, actual))
    }) && rule.gitdir.as_deref().is_none_or(|pattern| {
        context.gitdir.as_deref().is_some_and(|actual| {
            let expanded = expand_home(pattern);
            glob_matches(&expanded, &actual.to_string_lossy())
        })
    }) && rule.cwd.as_deref().is_none_or(|pattern| {
        context.cwd.as_deref().is_some_and(|actual| {
            let expanded = expand_home(pattern);
            glob_matches(&expanded, &actual.to_string_lossy())
        })
    })
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    build_rule_glob(pattern).is_ok_and(|glob| glob.compile_matcher().is_match(value))
}

fn build_rule_glob(pattern: &str) -> Result<Glob, globset::Error> {
    GlobBuilder::new(pattern)
        .literal_separator(false)
        .backslash_escape(false)
        .build()
}

fn expand_home(value: &str) -> String {
    if value == "~" {
        return env::var("HOME").unwrap_or_else(|_| value.into());
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Ok(home) = env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    value.to_owned()
}

/// Source used by the complete profile-resolution chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionSource {
    Explicit,
    InvalidExplicit,
    RepositoryBinding,
    InvalidRepositoryBinding,
    Rule { id: String, priority: i32 },
    GithubLogin,
    Default,
    Unresolved,
    Ambiguous,
}

/// Deterministic output of profile resolution.  `profile` is `None` for an
/// unresolved or ambiguous repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileResolution {
    pub profile: Option<String>,
    pub source: ResolutionSource,
    pub candidates: Vec<String>,
    pub warnings: Vec<String>,
}

/// Apply the documented precedence: explicit, repository binding, rule,
/// unique GitHub login, then default profile.
pub fn resolve_profile(
    config: &Config,
    context: &RuleContext,
    explicit: Option<&str>,
    repository_binding: Option<&str>,
    github_login_match: Option<&str>,
) -> ProfileResolution {
    let mut warnings = Vec::new();
    if let Some(id) = explicit {
        if config.profiles.contains_key(id) {
            return ProfileResolution {
                profile: Some(id.to_owned()),
                source: ResolutionSource::Explicit,
                candidates: vec![id.to_owned()],
                warnings,
            };
        }
        warnings.push(format!("profile `{id}` does not exist"));
        return ProfileResolution {
            profile: None,
            source: ResolutionSource::InvalidExplicit,
            candidates: vec![id.to_owned()],
            warnings,
        };
    }
    if let Some(id) = repository_binding {
        if config.profiles.contains_key(id) {
            return ProfileResolution {
                profile: Some(id.to_owned()),
                source: ResolutionSource::RepositoryBinding,
                candidates: vec![id.to_owned()],
                warnings,
            };
        }
        warnings.push(format!("repository binding `{id}` does not exist"));
        return ProfileResolution {
            profile: None,
            source: ResolutionSource::InvalidRepositoryBinding,
            candidates: vec![id.to_owned()],
            warnings,
        };
    }
    match resolve_rule(&config.rules, context) {
        RuleResolution::Match {
            rule_id,
            profile,
            priority,
        } if config.profiles.contains_key(&profile) => {
            return ProfileResolution {
                profile: Some(profile.clone()),
                source: ResolutionSource::Rule {
                    id: rule_id,
                    priority,
                },
                candidates: vec![profile],
                warnings,
            };
        }
        RuleResolution::Match { profile, .. } => {
            warnings.push(format!("rule selected missing profile `{profile}`"))
        }
        RuleResolution::Ambiguous { profiles, .. } => {
            return ProfileResolution {
                profile: None,
                source: ResolutionSource::Ambiguous,
                candidates: profiles,
                warnings,
            };
        }
        RuleResolution::None => {}
    }
    if let Some(id) = github_login_match
        && config.profiles.contains_key(id)
    {
        return ProfileResolution {
            profile: Some(id.to_owned()),
            source: ResolutionSource::GithubLogin,
            candidates: vec![id.to_owned()],
            warnings,
        };
    }
    if let Some(id) = config.behavior.default_profile.as_deref() {
        if config.profiles.contains_key(id) {
            return ProfileResolution {
                profile: Some(id.to_owned()),
                source: ResolutionSource::Default,
                candidates: vec![id.to_owned()],
                warnings,
            };
        }
        warnings.push(format!("default profile `{id}` does not exist"));
    }
    ProfileResolution {
        profile: None,
        source: ResolutionSource::Unresolved,
        candidates: Vec::new(),
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use tempfile::tempdir;

    fn profile() -> Profile {
        Profile {
            host: "github.com".into(),
            login: "alice".into(),
            git_name: "Alice".into(),
            git_email: "alice@example.com".into(),
            ..Profile::default()
        }
    }

    #[test]
    fn profile_description_must_be_safe_single_line_text() {
        for description in ["", "   ", "line one\nline two", "alert\u{1b}"] {
            let mut config = Config::default();
            let mut invalid = profile();
            invalid.description = Some(description.into());
            config.profiles.insert("work".into(), invalid);
            assert!(config.validate().is_err(), "accepted {description:?}");
        }

        let mut config = Config::default();
        let mut invalid = profile();
        invalid.description = Some("字".repeat(201));
        config.profiles.insert("work".into(), invalid);
        assert!(config.validate().is_err());
    }

    #[test]
    fn forwarded_agent_signing_requires_explicit_public_material() {
        let mut config = Config::default();
        let mut forwarded = profile();
        forwarded.signing.enabled = true;
        forwarded.signing.transport = SigningTransport::ForwardedAgent;
        config.profiles.insert("work".into(), forwarded.clone());
        assert!(config.validate().is_err());

        forwarded.ssh = Some(SshProfile {
            public_key: Some("/keys/work.pub".into()),
            ..SshProfile::default()
        });
        config.profiles.insert("work".into(), forwarded);
        config
            .validate()
            .expect("configured public key is explicit material");
    }

    #[test]
    fn managed_ssh_requires_a_public_key_path() {
        let mut config = Config::default();
        let mut managed = profile();
        managed.ssh = Some(SshProfile {
            mode: SshMode::OnePassword,
            ..SshProfile::default()
        });
        config.profiles.insert("work".into(), managed);

        let error = config.validate().expect_err("missing public key must fail");
        assert!(error.to_string().contains("public_key"));
    }

    #[test]
    fn invalid_rule_globs_are_rejected_without_exposing_the_pattern() {
        for (field, remote, gitdir, cwd) in [
            (
                "remote",
                Some("https://credential-marker@example.test/["),
                None,
                None,
            ),
            ("gitdir", None, Some("/private/credential-marker/["), None),
            ("cwd", None, None, Some("/private/credential-marker/[")),
        ] {
            let mut config = Config::default();
            config.profiles.insert("work".into(), profile());
            config.rules.push(Rule {
                id: "invalid-pattern".into(),
                profile: "work".into(),
                remote: remote.map(str::to_owned),
                gitdir: gitdir.map(str::to_owned),
                cwd: cwd.map(str::to_owned),
                ..Rule::default()
            });

            let error = config.validate().expect_err("invalid glob must fail");
            let message = error.to_string();
            assert!(message.contains("规则 `invalid-pattern`"));
            assert!(message.contains(field));
            assert!(message.contains("glob 模式无效"));
            assert!(!message.contains("credential-marker"));
        }
    }

    #[test]
    fn loading_rejects_invalid_rule_globs_without_exposing_the_pattern() {
        let dir = tempdir().unwrap();
        for (field, pattern) in [
            ("remote", "https://credential-marker@example.test/["),
            ("gitdir", "/private/credential-marker/["),
            ("cwd", "/private/credential-marker/["),
        ] {
            let path = dir.path().join(format!("invalid-{field}.toml"));
            fs::write(
                &path,
                format!(
                    r#"version = 1

[profiles.work]
host = "github.com"
login = "alice"
git_name = "Alice"
git_email = "alice@example.com"

[[rules]]
id = "loaded-invalid-pattern"
profile = "work"
{field} = "{pattern}"
"#
                ),
            )
            .unwrap();

            let error = Config::load(&path).expect_err("invalid loaded glob must fail");
            let message = error.to_string();
            assert!(message.contains("规则 `loaded-invalid-pattern`"));
            assert!(message.contains(field));
            assert!(message.contains("glob 模式无效"));
            assert!(!message.contains("credential-marker"));
        }
    }

    #[test]
    fn validation_keeps_existing_rule_pattern_semantics() {
        let mut config = Config::default();
        config.profiles.insert("work".into(), profile());
        config.rules.push(Rule {
            id: "compatible-patterns".into(),
            profile: "work".into(),
            repo: Some("literal[repo".into()),
            remote: Some("https://github.com/**".into()),
            gitdir: Some(r"C:\work\".into()),
            cwd: Some("~/discussions/**".into()),
            ..Rule::default()
        });

        config
            .validate()
            .expect("repo remains exact and backslashes remain literal in globs");
    }

    #[test]
    fn cwd_rules_match_the_working_directory_and_combine_with_targets() {
        let rule = Rule {
            id: "discussion-project".into(),
            profile: "work".into(),
            host: Some("github.com".into()),
            repo: Some("project".into()),
            cwd: Some("/workspace/discussions/**".into()),
            ..Rule::default()
        };
        let matching = RuleContext {
            host: Some("github.com".into()),
            repo: Some("project".into()),
            cwd: Some("/workspace/discussions/topic".into()),
            ..RuleContext::default()
        };
        assert!(rule_matches(&rule, &matching));

        let outside = RuleContext {
            cwd: Some("/workspace/projects/topic".into()),
            ..matching.clone()
        };
        assert!(!rule_matches(&rule, &outside));

        let wrong_target = RuleContext {
            repo: Some("other".into()),
            ..matching
        };
        assert!(!rule_matches(&rule, &wrong_target));
    }

    #[test]
    fn rules_use_priority_and_report_ambiguity() {
        let rules = vec![
            Rule {
                id: "a".into(),
                profile: "personal".into(),
                priority: 10,
                owner: Some("acme".into()),
                ..Rule::default()
            },
            Rule {
                id: "b".into(),
                profile: "work".into(),
                priority: 10,
                owner: Some("acme".into()),
                ..Rule::default()
            },
            Rule {
                id: "c".into(),
                profile: "personal".into(),
                priority: 1,
                ..Rule::default()
            },
        ];
        let ctx = RuleContext::new(
            Some("github.com"),
            Some("acme"),
            Some("repo"),
            None::<String>,
            None::<PathBuf>,
        );
        assert!(matches!(
            resolve_rule(&rules, &ctx),
            RuleResolution::Ambiguous { priority: 10, .. }
        ));
    }

    #[test]
    fn invalid_explicit_or_repository_binding_never_falls_back() {
        let mut config = Config::default();
        config.profiles.insert("fallback".into(), profile());
        config.behavior.default_profile = Some("fallback".into());
        let context = RuleContext::default();

        let explicit = resolve_profile(&config, &context, Some("missing"), None, None);
        assert_eq!(explicit.profile, None);
        assert_eq!(explicit.source, ResolutionSource::InvalidExplicit);

        let binding = resolve_profile(&config, &context, None, Some("deleted"), None);
        assert_eq!(binding.profile, None);
        assert_eq!(binding.source, ResolutionSource::InvalidRepositoryBinding);
    }

    #[test]
    fn toml_save_is_atomic_and_keeps_comments() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "# keep this comment\nversion = 1\nunknown = true\n").unwrap();
        let mut config = Config::load(&path).unwrap();
        config.profiles.insert("personal".into(), profile());
        config.save(&path).unwrap();
        let saved = fs::read_to_string(path).unwrap();
        assert!(saved.contains("# keep this comment"));
        assert!(saved.contains("unknown = true"));
        assert!(saved.contains("[profiles.personal]"));
    }

    #[test]
    fn transactional_updates_keep_changes_from_concurrent_writers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let mut writers = Vec::new();

        for index in 0..8 {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            writers.push(thread::spawn(move || {
                barrier.wait();
                let id = format!("profile-{index}");
                Config::update(&path, |config| {
                    config.profiles.insert(
                        id,
                        Profile {
                            login: format!("user-{index}"),
                            git_name: format!("User {index}"),
                            git_email: format!("user-{index}@example.test"),
                            ..Profile::default()
                        },
                    );
                    Ok::<_, ConfigError>(())
                })
                .unwrap();
            }));
        }
        for writer in writers {
            writer.join().unwrap();
        }

        let config = Config::load(&path).unwrap();
        assert_eq!(config.profiles.len(), 8);
        for index in 0..8 {
            assert!(config.profiles.contains_key(&format!("profile-{index}")));
        }
    }

    #[test]
    fn saving_removes_deleted_profiles() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::default();
        config.profiles.insert("personal".into(), profile());
        config.profiles.insert("obsolete".into(), profile());
        config.save(&path).unwrap();
        config.profiles.remove("obsolete");
        config.save(&path).unwrap();

        let reloaded = Config::load(&path).unwrap();
        assert!(reloaded.profiles.contains_key("personal"));
        assert!(!reloaded.profiles.contains_key("obsolete"));
        assert!(
            !fs::read_to_string(path)
                .unwrap()
                .contains("profiles.obsolete")
        );
    }

    #[test]
    fn saving_clears_optional_known_fields_and_keeps_unknown_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"version = 1
future_root = "keep"

[behavior]
default_profile = "personal"
auto_bind = true
future_behavior = "keep"

[profiles.personal]
host = "github.com"
login = "alice"
git_name = "Alice"
git_email = "alice@example.com"
description = "Personal projects"
future_profile = "keep"

[profiles.personal.ssh]
mode = "one-password"
public_key = "/tmp/alice.pub"
fingerprint = "SHA256:old"
agent_socket = "/tmp/agent.sock"
proxy_jump = ["bastion.example"]
forward_agent = true
future_ssh = "keep"

[profiles.personal.signing]
enabled = true
signing_key = "SHA256:old"
program = "/tmp/op-ssh-sign"
future_signing = "keep"
"#,
        )
        .unwrap();

        let mut config = Config::load(&path).unwrap();
        config.behavior.default_profile = None;
        let personal = config.profiles.get_mut("personal").unwrap();
        personal.description = None;
        let ssh = personal.ssh.as_mut().unwrap();
        ssh.mode = SshMode::External;
        ssh.public_key = None;
        ssh.fingerprint = None;
        ssh.agent_socket = None;
        ssh.proxy_jump.clear();
        ssh.forward_agent = false;
        personal.signing.signing_key = None;
        personal.signing.program = None;
        config.save(&path).unwrap();

        let saved = fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("default_profile"));
        assert!(!saved.contains("description"));
        assert!(!saved.contains("public_key"));
        assert!(!saved.contains("fingerprint"));
        assert!(!saved.contains("agent_socket"));
        assert!(!saved.contains("proxy_jump"));
        assert!(!saved.contains("forward_agent"));
        assert!(!saved.contains("signing_key"));
        assert!(!saved.contains("program ="));
        assert!(saved.contains("future_root = \"keep\""));
        assert!(saved.contains("future_behavior = \"keep\""));
        assert!(saved.contains("future_profile = \"keep\""));
        assert!(saved.contains("future_ssh = \"keep\""));
        assert!(saved.contains("future_signing = \"keep\""));
        assert_eq!(Config::load(path).unwrap(), config);
    }

    #[test]
    fn saving_rules_keeps_comments_and_unknown_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"version = 1

[profiles.work]
host = "github.com"
login = "alice"
git_name = "Alice"
git_email = "alice@example.com"

[[rules]]
# keep this rule comment
id = "work-rule"
profile = "work"
priority = 10
remote = "https://github.com/**"
future_rule = "keep"

[[rules]]
id = "obsolete-rule"
profile = "work"
priority = 1
"#,
        )
        .unwrap();

        let mut config = Config::load(&path).unwrap();
        config.rules[0].priority = 50;
        config.rules[0].remote = None;
        config.rules[0].cwd = Some("~/discussions/**".into());
        config.rules.remove(1);
        config.save(&path).unwrap();

        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# keep this rule comment"));
        assert!(saved.contains("future_rule = \"keep\""));
        assert!(saved.contains("priority = 50"));
        assert!(saved.contains("cwd = \"~/discussions/**\""));
        assert!(!saved.contains("remote ="));
        assert!(!saved.contains("obsolete-rule"));
        assert_eq!(Config::load(path).unwrap().rules.len(), 1);
    }

    #[test]
    fn xdg_paths_are_predictable() {
        let paths = ConfigPaths::from_bases("/tmp/config", "/tmp/cache", "/tmp/state");
        assert_eq!(
            paths.config_file,
            PathBuf::from("/tmp/config/ghis/config.toml")
        );
        assert_eq!(paths.log_file, PathBuf::from("/tmp/state/ghis/ghis.log"));
        assert_eq!(
            paths.repositories_file,
            PathBuf::from("/tmp/state/ghis/repositories.json")
        );
    }

    #[test]
    fn custom_config_files_have_stable_isolated_xdg_directories() {
        let mut first = ConfigPaths::from_bases("/tmp/config", "/tmp/cache", "/tmp/state");
        let default_fragments = first.fragments_dir.clone();
        let default_cache = first.cache_dir.clone();
        let default_state = first.state_dir.clone();
        first.set_config_file("/tmp/identity-a.toml").unwrap();
        assert!(first.fragments_dir.starts_with(&default_fragments));
        assert_ne!(first.fragments_dir, default_fragments);
        assert!(first.cache_dir.starts_with(&default_cache));
        assert_ne!(first.cache_dir, default_cache);
        assert!(first.state_dir.starts_with(&default_state));
        assert_ne!(first.state_dir, default_state);
        assert_eq!(first.fragments_dir.file_name(), first.cache_dir.file_name());
        assert_eq!(first.fragments_dir.file_name(), first.state_dir.file_name());
        assert_eq!(
            first.log_file,
            first.state_dir.join("ghis.log"),
            "state files must follow the custom namespace"
        );
        assert_eq!(
            first.repositories_file,
            first.state_dir.join("repositories.json"),
            "state files must follow the custom namespace"
        );

        let mut same = ConfigPaths::from_bases("/tmp/config", "/tmp/cache", "/tmp/state");
        same.set_config_file("/tmp/identity-a.toml").unwrap();
        assert_eq!(same.fragments_dir, first.fragments_dir);
        assert_eq!(same.cache_dir, first.cache_dir);
        assert_eq!(same.state_dir, first.state_dir);

        let mut other = ConfigPaths::from_bases("/tmp/config", "/tmp/cache", "/tmp/state");
        other.set_config_file("/tmp/identity-b.toml").unwrap();
        assert_ne!(other.fragments_dir, first.fragments_dir);
        assert_ne!(other.cache_dir, first.cache_dir);
        assert_ne!(other.state_dir, first.state_dir);

        let mut default = ConfigPaths::from_bases("/tmp/config", "/tmp/cache", "/tmp/state");
        default
            .set_config_file("/tmp/config/ghis/config.toml")
            .unwrap();
        assert_eq!(default.fragments_dir, default_fragments);
        assert_eq!(default.cache_dir, default_cache);
        assert_eq!(default.state_dir, default_state);
    }

    #[test]
    fn switching_config_files_on_one_paths_value_does_not_nest_namespaces() {
        let mut paths = ConfigPaths::from_bases("/tmp/config", "/tmp/cache", "/tmp/state");
        let base_cache = paths.cache_dir.clone();
        let base_state = paths.state_dir.clone();

        paths.set_config_file("/tmp/identity-a.toml").unwrap();
        let first_cache = paths.cache_dir.clone();
        let first_state = paths.state_dir.clone();
        paths.set_config_file("/tmp/identity-b.toml").unwrap();
        assert!(paths.cache_dir.starts_with(&base_cache));
        assert!(paths.state_dir.starts_with(&base_state));
        assert!(!paths.cache_dir.starts_with(&first_cache));
        assert!(!paths.state_dir.starts_with(&first_state));

        paths
            .set_config_file("/tmp/config/ghis/config.toml")
            .unwrap();
        assert_eq!(paths.cache_dir, base_cache);
        assert_eq!(paths.state_dir, base_state);
    }

    #[test]
    fn managed_ssh_accepts_explicit_safe_proxy_jump_chain() {
        let mut config = Config::default();
        let mut profile = profile();
        profile.ssh = Some(SshProfile {
            mode: SshMode::Managed,
            public_key: Some("/keys/work.pub".into()),
            proxy_jump: vec!["deploy@bastion.example:2222".into(), "inner.example".into()],
            forward_agent: true,
            ..SshProfile::default()
        });
        config.profiles.insert("work".into(), profile);
        config.validate().unwrap();
    }

    #[test]
    fn managed_ssh_rejects_unsafe_or_external_proxy_jump() {
        let mut config = Config::default();
        let mut profile = profile();
        profile.ssh = Some(SshProfile {
            mode: SshMode::Managed,
            public_key: Some("/keys/work.pub".into()),
            proxy_jump: vec!["-F/etc/ssh/evil.conf".into()],
            ..SshProfile::default()
        });
        config.profiles.insert("work".into(), profile);
        assert!(config.validate().is_err());

        config.profiles.get_mut("work").unwrap().ssh = Some(SshProfile {
            mode: SshMode::External,
            proxy_jump: vec!["bastion.example".into()],
            ..SshProfile::default()
        });
        assert!(config.validate().is_err());
    }

    #[test]
    fn unknown_keys_are_reported_per_section_and_sorted() {
        let document = r#"version = 1
future_root = "keep"

[behavior]
auto_bind = true
future_behavior = "keep"

[profiles.work]
host = "github.com"
login = "alice"
git_name = "Alice"
git_email = "alice@example.test"
future_profile = "keep"

[profiles.work.ssh]
mode = "external"
future_ssh = "keep"

[profiles.work.signing]
enabled = false
future_signing = "keep"

[[rules]]
id = "rule"
profile = "work"
future_rule = "keep"
"#
        .parse::<DocumentMut>()
        .expect("document");
        assert_eq!(
            unknown_config_keys(&document),
            vec![
                "behavior.future_behavior",
                "future_root",
                "profiles.work.future_profile",
                "profiles.work.signing.future_signing",
                "profiles.work.ssh.future_ssh",
                "rules[0].future_rule",
            ]
        );
    }

    #[test]
    fn known_documents_report_no_unknown_keys() {
        let mut config = Config::default();
        config.profiles.insert("work".into(), profile());
        config.rules.push(Rule {
            id: "all".into(),
            profile: "work".into(),
            ..Rule::default()
        });
        let text = toml_edit::ser::to_string_pretty(&config).expect("serialize");
        let document = text.parse::<DocumentMut>().expect("parse back");
        assert!(unknown_config_keys(&document).is_empty());
    }

    #[test]
    fn scanning_a_missing_or_invalid_file_reports_nothing() {
        let dir = tempdir().unwrap();
        assert!(scan_unknown_config_keys(dir.path().join("absent.toml")).is_empty());
        let broken = dir.path().join("broken.toml");
        fs::write(&broken, "not valid = [toml\n").unwrap();
        assert!(scan_unknown_config_keys(&broken).is_empty());
    }

    #[test]
    fn saving_preserves_unknown_keys_after_scanning() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "version = 1\nfuture_root = \"keep\"\n\n[behavior]\nfuture_behavior = \"keep\"\n",
        )
        .unwrap();
        assert_eq!(
            scan_unknown_config_keys(&path),
            vec!["behavior.future_behavior", "future_root"]
        );
        let mut config = Config::load(&path).unwrap();
        config.behavior.auto_bind = false;
        config.save(&path).unwrap();
        assert_eq!(
            scan_unknown_config_keys(&path),
            vec!["behavior.future_behavior", "future_root"]
        );
        assert_eq!(Config::load(path).unwrap(), config);
    }
}
