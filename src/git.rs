//! Small git adapter used by the resolver and the shell wrapper.
//!
//! All arguments are passed as separate `Command` arguments.  In particular,
//! this module never feeds user-controlled repository paths or profile values
//! through `sh -c`.

use crate::repo::{self, RepoError, Repository};
use crate::signing;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

#[derive(Debug)]
pub enum GitError {
    Io(std::io::Error),
    Command {
        operation: String,
        code: Option<i32>,
        stderr: String,
    },
    InvalidIdentity(String),
    InvalidOutput {
        operation: String,
        output: String,
    },
    Repo(RepoError),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "git: {err}"),
            Self::Command {
                operation,
                code,
                stderr,
            } => {
                write!(f, "git {operation} failed")?;
                if let Some(code) = code {
                    write!(f, " (exit {code})")?;
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            Self::InvalidIdentity(value) => write!(f, "invalid git identity: {value}"),
            Self::InvalidOutput { operation, output } => {
                write!(f, "invalid git output for {operation}: {output}")
            }
            Self::Repo(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for GitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Repo(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for GitError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<RepoError> for GitError {
    fn from(value: RepoError) -> Self {
        Self::Repo(value)
    }
}

pub type Result<T> = std::result::Result<T, GitError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitIdentity {
    pub name: String,
    pub email: String,
    /// The original value from `git var`, useful when showing diagnostics.
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveIdentities {
    pub author: GitIdentity,
    pub committer: GitIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityExpectation<'a> {
    pub name: &'a str,
    pub email: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityCheck {
    pub author_matches: bool,
    pub committer_matches: bool,
}

impl EffectiveIdentities {
    pub fn check(&self, expected: IdentityExpectation<'_>) -> IdentityCheck {
        IdentityCheck {
            author_matches: self.author.name == expected.name
                && self.author.email == expected.email,
            committer_matches: self.committer.name == expected.name
                && self.committer.email == expected.email,
        }
    }
}

/// Read the identity Git will actually use, including environment and `-c`
/// overrides.  This is preferable to inspecting config files directly.
pub fn effective_identities(repository: &Repository) -> Result<EffectiveIdentities> {
    let author = identity_from_git_var(repository, "GIT_AUTHOR_IDENT")?;
    let committer = identity_from_git_var(repository, "GIT_COMMITTER_IDENT")?;
    Ok(EffectiveIdentities { author, committer })
}

pub fn identity_from_git_var(repository: &Repository, variable: &str) -> Result<GitIdentity> {
    let out = run_git(repository.command_dir(), ["var", variable])?;
    if !out.status.success() {
        return Err(command_error(format!("var {variable}"), &out));
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim_end().to_owned();
    parse_identity(&raw)
}

/// Parse the format emitted by `git var GIT_*_IDENT`:
/// `Name <email> timestamp timezone`.
pub fn parse_identity(value: &str) -> Result<GitIdentity> {
    let value = value.trim();
    let Some(end) = value.rfind('>') else {
        return Err(GitError::InvalidIdentity(value.to_owned()));
    };
    let Some(start) = value[..end].rfind('<') else {
        return Err(GitError::InvalidIdentity(value.to_owned()));
    };
    if start == 0 || end <= start + 1 {
        return Err(GitError::InvalidIdentity(value.to_owned()));
    }
    let name = value[..start].trim();
    let email = value[start + 1..end].trim();
    if name.is_empty() || email.is_empty() {
        return Err(GitError::InvalidIdentity(value.to_owned()));
    }
    Ok(GitIdentity {
        name: name.to_owned(),
        email: email.to_owned(),
        raw: value.to_owned(),
    })
}

pub fn git_config(repository: &Repository, key: &str) -> Result<Option<String>> {
    repo::local_config(repository, key).map_err(Into::into)
}

/// One value returned by Git's merged configuration view.
///
/// The scope and origin are supplied by Git itself.  Keeping them alongside
/// the value lets diagnostics explain include/includeIf precedence without
/// implementing a second Git config parser in ghis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEntry {
    pub scope: String,
    pub origin: String,
    pub key: String,
    pub value: String,
}

/// Read the effective Git configuration, including included files.
///
/// `--null` makes values containing spaces or newlines unambiguous while
/// `--show-origin` and `--show-scope` preserve the provenance needed by the
/// conflict scanner.  The command is deliberately read-only.
pub fn config_entries(dir: &Path) -> Result<Vec<ConfigEntry>> {
    let out = run_git(
        dir,
        [
            "config",
            "--null",
            "--show-origin",
            "--show-scope",
            "--includes",
            "--list",
        ],
    )?;
    if !out.status.success() {
        return Err(command_error(
            "config --show-origin --show-scope --list",
            &out,
        ));
    }

    parse_config_entries(&out.stdout)
}

/// Parse the NUL-delimited triples emitted by [`config_entries`].
pub fn parse_config_entries(bytes: &[u8]) -> Result<Vec<ConfigEntry>> {
    let fields = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    // Git terminates each record with NUL. A trailing split field is therefore
    // expected; any other remainder means the output format was not the one
    // requested and must not be silently ignored (a dropped record could hide
    // an authentication or identity override).
    let trailing_empty = fields.last().is_some_and(|field| field.is_empty());
    let record_fields = if trailing_empty {
        &fields[..fields.len().saturating_sub(1)]
    } else {
        fields.as_slice()
    };
    if !record_fields.len().is_multiple_of(3) {
        return Err(GitError::InvalidOutput {
            operation: "config --show-origin --show-scope --list".into(),
            output: "Git returned an incomplete NUL-delimited config record".into(),
        });
    }

    let mut entries = Vec::with_capacity(record_fields.len() / 3);
    for chunk in record_fields.chunks_exact(3) {
        if chunk.iter().all(|field| field.is_empty()) {
            continue;
        }
        let scope = String::from_utf8_lossy(chunk[0]).trim().to_owned();
        // Preserve the origin byte-for-byte. A filename may legitimately end
        // in whitespace, and trimming it would make the provenance
        // misleading in diagnostics.
        let origin = String::from_utf8_lossy(chunk[1]).into_owned();
        let Some(separator) = chunk[2].iter().position(|byte| *byte == b'\n') else {
            return Err(GitError::InvalidOutput {
                operation: "config --show-origin --show-scope --list".into(),
                output: "Git returned a config field without its key/value separator".into(),
            });
        };
        let (key, value) = chunk[2].split_at(separator);
        let value = &value[1..];
        entries.push(ConfigEntry {
            scope,
            origin,
            key: String::from_utf8_lossy(key).trim().to_ascii_lowercase(),
            value: String::from_utf8_lossy(value).into_owned(),
        });
    }
    Ok(entries)
}

pub fn set_git_config(repository: &Repository, key: &str, value: &str) -> Result<()> {
    repo::set_local_config(repository, key, value).map_err(Into::into)
}

pub fn unset_git_config(repository: &Repository, key: &str) -> Result<()> {
    repo::unset_local_config(repository, key).map_err(Into::into)
}

/// Git config key used for an exact HTTPS host helper.  Keeping this helper
/// URL-scoped prevents an unusable bound profile from falling back to the
/// user's global helper and another account.
pub fn credential_helper_key(host: &str) -> String {
    format!("credential.https://{}.helper", normalize_host(host))
}

/// Replace the URL-scoped helper with `helper`.  `helper` is a single Git
/// config value (usually `!ghis credential-helper`), not a shell command run
/// by this process.
pub fn install_credential_helper(repository: &Repository, host: &str, helper: &str) -> Result<()> {
    let key = credential_helper_key(host);
    repo::unset_local_config(repository, &key)?;
    // An empty helper value resets helpers inherited from global/system config
    // for this exact URL.  Without it Git could silently fall through to an
    // old account when ghis cannot obtain the bound profile's token.
    repo::add_local_config(repository, &key, "")?;
    repo::add_local_config(repository, &key, helper).map_err(Into::into)
}

/// Git 2.55's named-hook configuration lets ghis add its checks without
/// replacing an existing `.git/hooks/<event>` file or `core.hooksPath`.
pub const NAMED_HOOK_PREPARE: &str = "ghis-prepare-commit-msg";
pub const NAMED_HOOK_PUSH: &str = "ghis-pre-push";

/// Install the identity banner/check hook for commit and push events.  The
/// command is a fixed executable plus fixed arguments; profile data never gets
/// interpolated into the shell command stored in Git config.
pub fn install_named_hooks(repository: &Repository, binary: &str) -> Result<()> {
    install_named_hooks_with_config(repository, binary, None)
}

/// Install named hooks and pin the configuration file used by their internal
/// ghis invocation. This keeps `--config` bindings working outside the wrapper.
pub fn install_named_hooks_with_config(
    repository: &Repository,
    binary: &str,
    config_path: Option<&Path>,
) -> Result<()> {
    install_one_named_hook(
        repository,
        binary,
        config_path,
        NAMED_HOOK_PREPARE,
        "prepare-commit-msg",
    )?;
    install_one_named_hook(repository, binary, config_path, NAMED_HOOK_PUSH, "pre-push")
}

fn install_one_named_hook(
    repository: &Repository,
    binary: &str,
    config_path: Option<&Path>,
    name: &str,
    event: &str,
) -> Result<()> {
    let command = named_hook_command(binary, config_path, event);
    let command_key = format!("hook.{name}.command");
    let event_key = format!("hook.{name}.event");
    for key in [&command_key, &event_key] {
        repo::unset_local_config(repository, key)?;
    }
    repo::add_local_config(repository, &command_key, &command)?;
    repo::add_local_config(repository, &event_key, event).map_err(Into::into)
}

/// Render the exact command stored for one named hook.
pub fn named_hook_command(binary: &str, config_path: Option<&Path>, event: &str) -> String {
    let config_arg = config_path.map_or_else(String::new, |path| {
        format!(" --config {}", shell_quote(path.to_string_lossy().as_ref()))
    });
    format!(
        "{}{config_arg} hook --hook {}",
        shell_quote(binary),
        shell_quote(event)
    )
}

/// Check that both named hooks still point at the expected binary and config.
pub fn named_hooks_match(
    repository: &Repository,
    binary: &str,
    config_path: Option<&Path>,
) -> Result<bool> {
    for (name, event) in [
        (NAMED_HOOK_PREPARE, "prepare-commit-msg"),
        (NAMED_HOOK_PUSH, "pre-push"),
    ] {
        let command_key = format!("hook.{name}.command");
        let event_key = format!("hook.{name}.event");
        let expected = named_hook_command(binary, config_path, event);
        if git_config(repository, &command_key)?.as_deref() != Some(expected.as_str())
            || git_config(repository, &event_key)?.as_deref() != Some(event)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Remove only the named hook owned by ghis.  Existing traditional hooks and
/// other named hooks remain untouched.
pub fn remove_named_hooks(repository: &Repository) -> Result<()> {
    for key in [
        format!("hook.{NAMED_HOOK_PREPARE}.command"),
        format!("hook.{NAMED_HOOK_PREPARE}.event"),
        format!("hook.{NAMED_HOOK_PREPARE}.enabled"),
        format!("hook.{NAMED_HOOK_PREPARE}.parallel"),
        format!("hook.{NAMED_HOOK_PUSH}.command"),
        format!("hook.{NAMED_HOOK_PUSH}.event"),
        format!("hook.{NAMED_HOOK_PUSH}.enabled"),
        format!("hook.{NAMED_HOOK_PUSH}.parallel"),
        // Clean up the early development name if a user ran that build.
        "hook.ghis-identity.command".into(),
        "hook.ghis-identity.event".into(),
    ] {
        repo::unset_local_config(repository, &key)?;
    }
    Ok(())
}

/// Check whether this Git advertises the named-hook configuration introduced
/// in Git 2.55.  It is intentionally a capability probe rather than a
/// version-string comparison, which also works with vendor backports.
pub fn supports_named_hooks() -> bool {
    static SUPPORTS_NAMED_HOOKS: OnceLock<bool> = OnceLock::new();
    *SUPPORTS_NAMED_HOOKS.get_or_init(|| {
        Command::new("git")
            .args(["help", "--config"])
            .output()
            .map(|output| {
                String::from_utf8_lossy(&output.stdout).contains("hook.<friendly-name>.command")
            })
            .unwrap_or(false)
    })
}

pub fn remove_credential_helper(repository: &Repository, host: &str) -> Result<()> {
    let key = credential_helper_key(host);
    repo::unset_local_config(repository, &key).map_err(Into::into)
}

/// Return the configured `core.hooksPath`, if any.  Hooks managed by ghis
/// should use Git's named-hook facility where available and must not silently
/// replace a user hook or this setting.
pub fn hooks_path(repository: &Repository) -> Result<Option<PathBuf>> {
    let value = git_config(repository, "core.hooksPath")?;
    Ok(value.map(|path| {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            path
        } else {
            repository.command_dir().join(path)
        }
    }))
}

/// Build the body of a hook that delegates to `ghis hook`.  A caller can
/// install it through Git's named hooks API (or a dedicated hook directory)
/// without having to interpolate any profile data into shell.
pub fn hook_script(hook_name: &str) -> String {
    format!(
        "#!/bin/sh\nexec ghis hook --hook {} \"$@\"\n",
        shell_quote(hook_name)
    )
}

/// Install a hook in the repository's ordinary hooks directory only when the
/// target does not already exist.  This fallback is useful on Git versions
/// without named hooks; callers can elect to skip it when `core.hooksPath` is
/// set.  Returns `true` when a new file was written.
pub fn install_hook_if_missing(repository: &Repository, hook_name: &str) -> Result<bool> {
    let hooks = repository.git_dir.join("hooks");
    fs::create_dir_all(&hooks)?;
    let path = hooks.join(hook_name);
    if path.exists() {
        return Ok(false);
    }
    let temporary = path.with_extension("ghis.tmp");
    fs::write(&temporary, hook_script(hook_name))?;
    let mut permissions = fs::metadata(&temporary)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o755);
        fs::set_permissions(&temporary, permissions)?;
    }
    fs::rename(temporary, path)?;
    Ok(true)
}

/// Render a profile fragment containing only profile-scoped Git settings.
/// The caller owns the path and can atomically write it under XDG config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FragmentOptions {
    pub name: String,
    pub email: String,
    pub credential_helper: Option<String>,
    pub ssh_command: Option<String>,
    pub signing_key: Option<String>,
    pub signing_program: Option<String>,
    pub commit_gpg_sign: bool,
}

pub fn render_profile_fragment(options: &FragmentOptions) -> String {
    let mut output = String::new();
    output.push_str("# Generated by ghis; edits may be overwritten.\n");
    output.push_str("[user]\n");
    output.push_str("\tname = ");
    output.push_str(&git_config_quote(&options.name));
    output.push('\n');
    output.push_str("\temail = ");
    output.push_str(&git_config_quote(&options.email));
    output.push('\n');
    if let Some(value) = options.credential_helper.as_deref() {
        output.push_str("[credential]\n\thelper = ");
        output.push_str(&git_config_quote(value));
        output.push('\n');
    }
    if let Some(value) = options.ssh_command.as_deref() {
        output.push_str("[core]\n\tsshCommand = ");
        output.push_str(&git_config_quote(value));
        output.push('\n');
    }
    if options.signing_key.is_some() || options.signing_program.is_some() || options.commit_gpg_sign
    {
        output.push_str("[gpg]\n\tformat = ssh\n");
        if let Some(value) = options.signing_program.as_deref() {
            output.push_str("[gpg \"ssh\"]\n\tprogram = ");
            output.push_str(&git_config_quote(value));
            output.push('\n');
        }
        if let Some(value) = options.signing_key.as_deref() {
            output.push_str("[user]\n\tsigningKey = ");
            output.push_str(&git_config_quote(&signing::git_signing_key_value(value)));
            output.push('\n');
        }
        output.push_str("[commit]\n\tgpgSign = ");
        output.push_str(if options.commit_gpg_sign {
            "true\n"
        } else {
            "false\n"
        });
    }
    output
}

/// Quote a value for Git's config file syntax.  This is deliberately separate
/// from shell quoting: fragments are parsed by Git, not by a shell.
pub fn git_config_quote(value: &str) -> String {
    if value
        .bytes()
        .all(|byte| !byte.is_ascii_whitespace() && !matches!(byte, b'#' | b';' | b'"' | b'\\'))
    {
        return value.to_owned();
    }
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

/// POSIX shell single-quote escaping used only for generated hook text.
pub fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".into();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn normalize_host(host: &str) -> String {
    crate::github::normalize_host(host)
}

pub fn run_git<I, S>(dir: &Path, args: I) -> std::io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("git").args(args).current_dir(dir).output()
}

fn command_error(operation: impl Into<String>, output: &Output) -> GitError {
    GitError::Command {
        operation: operation.into(),
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn initialized_repository() -> (tempfile::TempDir, Repository) {
        let directory = tempfile::tempdir().unwrap();
        let output = Command::new("git")
            .args(["init", "-q"])
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        let repository = crate::repo::discover(directory.path()).unwrap();
        (directory, repository)
    }

    #[test]
    fn parses_git_var_identity_with_timestamp() {
        let identity =
            parse_identity("Alice Example <alice@example.test> 1710000000 +0000").unwrap();
        assert_eq!(identity.name, "Alice Example");
        assert_eq!(identity.email, "alice@example.test");
    }

    #[test]
    fn config_and_shell_quoting_are_distinct() {
        assert_eq!(git_config_quote("Alice"), "Alice");
        assert_eq!(git_config_quote("A Name"), "\"A Name\"");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn fragment_contains_expected_scoped_settings() {
        let fragment = render_profile_fragment(&FragmentOptions {
            name: "Alice".into(),
            email: "alice@example.test".into(),
            credential_helper: Some("!ghis credential-helper".into()),
            ssh_command: None,
            signing_key: None,
            signing_program: None,
            commit_gpg_sign: false,
        });
        assert!(fragment.contains("[user]"));
        assert!(fragment.contains("email = alice@example.test"));
        assert!(fragment.contains("ghis credential-helper"));
    }

    #[test]
    fn parses_git_config_entries_with_origin_scope_and_newlines() {
        let bytes = b"global\0file:/tmp/global\0user.name\nAlice Example\0global\0file:/tmp/global\0credential.helper\nstore\0";
        assert_eq!(
            parse_config_entries(bytes).unwrap(),
            vec![
                ConfigEntry {
                    scope: "global".into(),
                    origin: "file:/tmp/global".into(),
                    key: "user.name".into(),
                    value: "Alice Example".into(),
                },
                ConfigEntry {
                    scope: "global".into(),
                    origin: "file:/tmp/global".into(),
                    key: "credential.helper".into(),
                    value: "store".into(),
                },
            ]
        );
    }

    #[test]
    fn rejects_incomplete_or_unseparated_config_records() {
        let incomplete = parse_config_entries(b"global\0file:/tmp/config\0user.name\nAlice\0extra");
        assert!(matches!(incomplete, Err(GitError::InvalidOutput { .. })));

        let missing_separator = parse_config_entries(b"global\0file:/tmp/config\0user.name\0");
        assert!(matches!(
            missing_separator,
            Err(GitError::InvalidOutput { .. })
        ));
    }

    #[test]
    fn preserves_embedded_newlines_in_config_values() {
        let entries = parse_config_entries(
            b"global\0file:/tmp/config\0http.extraheader\nX-Test: one\nX-Test: two\0",
        )
        .unwrap();
        assert_eq!(entries[0].key, "http.extraheader");
        assert_eq!(entries[0].value, "X-Test: one\nX-Test: two");
    }

    #[test]
    fn signing_fragment_is_valid_git_config() {
        let file = tempfile::NamedTempFile::new().expect("temporary fragment");
        let fragment = render_profile_fragment(&FragmentOptions {
            name: "Alice".into(),
            email: "alice@example.test".into(),
            signing_key: Some("/tmp/alice signing.pub".into()),
            signing_program: Some("/opt/1Password/op-ssh-sign".into()),
            commit_gpg_sign: true,
            ..FragmentOptions::default()
        });
        std::fs::write(file.path(), fragment).expect("write fragment");

        for (key, expected) in [
            ("gpg.format", "ssh"),
            ("gpg.ssh.program", "/opt/1Password/op-ssh-sign"),
            ("user.signingKey", "/tmp/alice signing.pub"),
            ("commit.gpgSign", "true"),
        ] {
            let output = std::process::Command::new("git")
                .args(["config", "--file"])
                .arg(file.path())
                .args(["--get", key])
                .output()
                .expect("run git config");
            assert!(
                output.status.success(),
                "git rejected {key}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
        }
    }

    #[test]
    fn signing_fragment_marks_every_inline_ssh_key_as_literal() {
        for value in [
            "ssh-ed25519 AAAA inline",
            "ecdsa-sha2-nistp256 AAAA inline",
            "sk-ssh-ed25519@openssh.com AAAA inline",
            "rsa-sha2-512 AAAA inline",
        ] {
            let file = tempfile::NamedTempFile::new().expect("temporary fragment");
            let fragment = render_profile_fragment(&FragmentOptions {
                name: "Alice".into(),
                email: "alice@example.test".into(),
                signing_key: Some(value.into()),
                commit_gpg_sign: true,
                ..FragmentOptions::default()
            });
            std::fs::write(file.path(), fragment).expect("write fragment");
            let output = std::process::Command::new("git")
                .args(["config", "--file"])
                .arg(file.path())
                .args(["--get", "user.signingKey"])
                .output()
                .expect("run git config");
            assert!(output.status.success());
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).trim(),
                format!("key::{value}")
            );
        }
    }

    #[test]
    fn scoped_credential_helper_resets_inherited_helpers() {
        let (_directory, repository) = initialized_repository();
        install_credential_helper(&repository, "GitHub.com", "!ghis credential-helper").unwrap();
        let output = run_git(
            repository.command_dir(),
            [
                "config",
                "--worktree",
                "--get-all",
                "credential.https://github.com.helper",
            ],
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "\n!ghis credential-helper\n"
        );
    }

    #[test]
    fn credential_helper_keys_use_canonical_https_hosts() {
        assert_eq!(
            credential_helper_key("GitHub.com.:443"),
            "credential.https://github.com.helper"
        );
        assert_eq!(
            credential_helper_key("[2001:DB8::1]:443"),
            "credential.https://[2001:db8::1].helper"
        );
        assert_eq!(
            credential_helper_key("git.example:8443"),
            "credential.https://git.example:8443.helper"
        );
    }

    #[test]
    fn named_hooks_do_not_touch_traditional_hook_directory() {
        let (_directory, repository) = initialized_repository();
        let traditional = repository.git_dir.join("hooks/pre-push");
        fs::write(&traditional, "#!/bin/sh\nexit 0\n").unwrap();
        install_named_hooks(&repository, "/tmp/ghis test").unwrap();
        assert!(traditional.exists());
        let output = run_git(
            repository.command_dir(),
            ["config", "--worktree", "--get", "hook.ghis-pre-push.event"],
        )
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "pre-push");
        let command = run_git(
            repository.command_dir(),
            [
                "config",
                "--worktree",
                "--get",
                "hook.ghis-prepare-commit-msg.command",
            ],
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&command.stdout).contains("'/tmp/ghis test'"));
    }
}
