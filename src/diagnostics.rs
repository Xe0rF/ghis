//! Read-only diagnostics for Git configuration that can alter identity,
//! authentication, signing, or the actual remote used by an operation.

use crate::config::{Profile, SshMode};
use crate::git::{self, ConfigEntry, EffectiveIdentities};
use crate::repo::Remote;
use crate::signing;
use serde::Serialize;
use std::path::Path;

/// Diagnostic severity shared by CLI JSON/text output and the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "信息",
            Self::Warning => "警告",
            Self::Error => "错误",
        }
    }
}

/// Stable machine-readable identifier for a diagnostic finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    GitIdentityConflict,
    SigningConflict,
    SshCommand,
    CredentialHelper,
    AuthorizationHeader,
    UrlRewrite,
    EffectiveIdentityMismatch,
    InvalidBinding,
    ShellIntegration,
    GhAuth,
    GlobalGitConfig,
    SshKey,
}

/// Safety class for a proposed repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairKind {
    Automatic,
    Confirmation,
    Manual,
}

impl RepairKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Automatic => "自动",
            Self::Confirmation => "需确认",
            Self::Manual => "手动",
        }
    }
}

/// A safe, displayable repair description. Commands are suggestions only;
/// execution is always performed by the owning application layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepairAction {
    pub kind: RepairKind,
    pub command: String,
    pub confirmation: bool,
}

impl RepairAction {
    pub fn automatic(command: impl Into<String>) -> Self {
        Self {
            kind: RepairKind::Automatic,
            command: command.into(),
            confirmation: false,
        }
    }

    pub fn confirmation(command: impl Into<String>) -> Self {
        Self {
            kind: RepairKind::Confirmation,
            command: command.into(),
            confirmation: true,
        }
    }

    pub fn manual(command: impl Into<String>) -> Self {
        Self {
            kind: RepairKind::Manual,
            command: command.into(),
            confirmation: false,
        }
    }
}

/// One actionable configuration finding. Values are sanitized before they
/// enter this public structure so JSON, logs, and TUI rows cannot expose an
/// Authorization header or credential helper body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitConfigDiagnostic {
    pub code: DiagnosticCode,
    pub severity: Severity,
    pub key: String,
    pub value: String,
    pub scope: String,
    pub origin: String,
    pub impact: String,
    pub suggestion: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repair: Option<RepairAction>,
}

/// A doctor check separate from the detailed Git configuration scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiagnosticCheck {
    pub code: DiagnosticCode,
    pub severity: Severity,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repair: Option<RepairAction>,
}

impl DiagnosticCheck {
    pub fn new(
        code: DiagnosticCode,
        severity: Severity,
        summary: impl Into<String>,
        repair: Option<RepairAction>,
    ) -> Self {
        Self {
            code,
            severity,
            summary: bounded(&summary.into()),
            repair,
        }
    }
}

/// Structured report consumed by both front ends.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GitConfigReport {
    pub info: usize,
    pub warnings: usize,
    pub errors: usize,
    pub diagnostics: Vec<GitConfigDiagnostic>,
}

impl GitConfigReport {
    fn from_diagnostics(diagnostics: Vec<GitConfigDiagnostic>) -> Self {
        // Git's merged list is ordered by source and value. Keep that order:
        // it is material for multi-valued credential helpers and URL rewrite
        // precedence. Severity counts let the callers present a summary
        // without rewriting the underlying configuration order.
        let info = diagnostics
            .iter()
            .filter(|item| item.severity == Severity::Info)
            .count();
        let warnings = diagnostics
            .iter()
            .filter(|item| item.severity == Severity::Warning)
            .count();
        let errors = diagnostics
            .iter()
            .filter(|item| item.severity == Severity::Error)
            .count();
        Self {
            info,
            warnings,
            errors,
            diagnostics,
        }
    }
}

/// Compact summary suitable for a status row or a doctor heading.
pub fn summary_line(report: &GitConfigReport) -> String {
    format!(
        "信息={} 警告={} 错误={}",
        report.info, report.warnings, report.errors
    )
}

/// Render one finding for a text terminal or a filterable TUI list row.
pub fn render_row(item: &GitConfigDiagnostic) -> String {
    format!(
        "{}\t{}={}\t范围={} 来源={}\t影响={}\t建议={}",
        item.severity.label(),
        item.key,
        item.value,
        item.scope,
        item.origin,
        item.impact,
        item.suggestion
    )
}

/// Inspect Git's merged configuration without changing any setting.
pub fn scan_git_config(
    cwd: &Path,
    profile: Option<&Profile>,
    remote: Option<&Remote>,
    identities: Option<&EffectiveIdentities>,
) -> git::Result<GitConfigReport> {
    let entries = git::config_entries(cwd)?;
    Ok(scan_entries(&entries, profile, remote, identities))
}

/// Analyze entries already obtained from Git. This pure form keeps tests
/// independent from the caller's real HOME and global Git configuration.
pub fn scan_entries(
    entries: &[ConfigEntry],
    profile: Option<&Profile>,
    remote: Option<&Remote>,
    identities: Option<&EffectiveIdentities>,
) -> GitConfigReport {
    let mut diagnostics = Vec::new();

    for entry in entries {
        if is_ghis_fragment(entry) {
            continue;
        }
        let key = entry.key.as_str();
        if matches!(key, "user.name" | "user.email") {
            inspect_identity_entry(entry, profile, &mut diagnostics);
        } else if matches!(
            key,
            "user.signingkey" | "commit.gpgsign" | "gpg.format" | "gpg.ssh.program"
        ) {
            inspect_signing_entry(entry, profile, &mut diagnostics);
        } else if key == "core.sshcommand" {
            inspect_ssh_command(entry, profile, remote, &mut diagnostics);
        } else if is_credential_helper(key) {
            inspect_credential_helper(entry, profile, remote, &mut diagnostics);
        } else if is_extra_header(key) {
            inspect_extra_header(entry, profile, remote, &mut diagnostics);
        } else if is_url_rewrite(key) {
            inspect_url_rewrite(entry, remote, &mut diagnostics);
        }
    }

    inspect_effective_identity(profile, identities, &mut diagnostics);
    GitConfigReport::from_diagnostics(diagnostics)
}

fn inspect_identity_entry(
    entry: &ConfigEntry,
    profile: Option<&Profile>,
    diagnostics: &mut Vec<GitConfigDiagnostic>,
) {
    let Some(profile) = profile else {
        return;
    };
    let expected = if entry.key == "user.name" {
        &profile.git_name
    } else {
        &profile.git_email
    };
    if entry.value.trim_end() == expected {
        return;
    }
    diagnostics.push(finding(
        Severity::Warning,
        entry,
        "这个身份值与当前 Profile 不同，可能在未绑定仓库或显式覆盖时成为提交身份",
        "保留前先确认其适用范围；ghis 不会自动删除全局、system 或仓库配置",
    ));
}

fn inspect_signing_entry(
    entry: &ConfigEntry,
    profile: Option<&Profile>,
    diagnostics: &mut Vec<GitConfigDiagnostic>,
) {
    let Some(profile) = profile else {
        return;
    };
    let value = entry.value.trim();
    let conflicts = match entry.key.as_str() {
        "commit.gpgsign" => {
            parse_git_bool(value).is_some_and(|enabled| enabled != profile.signing.enabled)
        }
        "gpg.format" => profile.signing.enabled && !value.eq_ignore_ascii_case("ssh"),
        "gpg.ssh.program" => {
            profile.signing.enabled
                && expected_signing_program(profile)
                    .is_some_and(|expected| !same_config_value(value, &expected))
        }
        "user.signingkey" => {
            profile.signing.enabled
                && profile_signing_key(profile)
                    .as_deref()
                    .is_some_and(|expected| !same_signing_key(value, expected))
        }
        _ => false,
    };
    if !conflicts {
        return;
    }
    diagnostics.push(finding(
        Severity::Warning,
        entry,
        "这个签名设置可能覆盖当前 Profile 的 SSH signing 配置或强制启用另一种签名方式",
        "把账号相关签名设置放进 Profile；ghis 只报告，不会改写现有全局配置",
    ));
}

/// The generated fragment uses an explicit signing key when supplied, and
/// otherwise reuses the managed SSH public-key path. Keep that same fallback
/// here so a matching global value is not reported as a conflict merely
/// because the profile chose the authentication key for signing as well.
fn profile_signing_key(profile: &Profile) -> Option<String> {
    profile.signing.signing_key.clone().or_else(|| {
        profile
            .ssh
            .as_ref()
            .and_then(|ssh| ssh.public_key.as_ref())
            .map(|path| path.to_string_lossy().into_owned())
    })
}

fn same_config_value(actual: &str, expected: &str) -> bool {
    actual.trim() == expected.trim()
}

fn expected_signing_program(profile: &Profile) -> Option<String> {
    profile
        .signing
        .program
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .or_else(|| {
            signing::discover_signing_program(None)
                .map(|program| program.path.to_string_lossy().into_owned())
        })
}

fn same_signing_key(actual: &str, expected: &str) -> bool {
    let actual = actual.trim();
    let expected = expected.trim();
    let actual_material = actual
        .strip_prefix("key::")
        .and_then(signing::public_key_material);
    let expected_material = expected
        .strip_prefix("key::")
        .and_then(signing::public_key_material);
    match (actual_material, expected_material) {
        (Some(actual), Some(expected)) => actual == expected,
        _ => same_config_value(actual, expected),
    }
}

fn inspect_ssh_command(
    entry: &ConfigEntry,
    profile: Option<&Profile>,
    remote: Option<&Remote>,
    diagnostics: &mut Vec<GitConfigDiagnostic>,
) {
    let managed = profile
        .and_then(|profile| profile.ssh.as_ref())
        .is_some_and(|ssh| matches!(ssh.mode, SshMode::OnePassword | SshMode::Managed));
    let ssh_remote = remote.is_some_and(|remote| remote.transport.is_ssh());
    diagnostics.push(finding(
        if managed || ssh_remote {
            Severity::Warning
        } else {
            Severity::Info
        },
        entry,
        "core.sshCommand 会改变 SSH push 使用的程序、Agent 或 key",
        "确认它不会固定到其他账号；使用纳管 SSH 时应由 Profile fragment 提供连接设置",
    ));
}

fn inspect_credential_helper(
    entry: &ConfigEntry,
    profile: Option<&Profile>,
    remote: Option<&Remote>,
    diagnostics: &mut Vec<GitConfigDiagnostic>,
) {
    let managed = is_ghis_helper(&entry.value);
    let applies = entry_applies_to_host(entry, profile, remote);
    diagnostics.push(finding(
        if managed {
            Severity::Info
        } else if applies {
            Severity::Warning
        } else {
            Severity::Info
        },
        entry,
        if managed {
            "这是 ghis 安装的专用 credential helper；它会按当前 Profile 选择凭据"
        } else {
            "credential helper 参与凭据查找；错误的链顺序可能让 Git 尝试另一个账号"
        },
        if managed {
            "确认它前面的空值重置和 helper 链顺序没有被其他配置覆盖"
        } else {
            "检查 helper 的作用域和顺序；绑定仓库会为 Profile 主机安装专用 helper"
        },
    ));
}

fn inspect_extra_header(
    entry: &ConfigEntry,
    profile: Option<&Profile>,
    remote: Option<&Remote>,
    diagnostics: &mut Vec<GitConfigDiagnostic>,
) {
    let authorization = is_authorization_header(&entry.value);
    let applies = entry_applies_to_host(entry, profile, remote);
    diagnostics.push(finding(
        if authorization && applies {
            Severity::Error
        } else if authorization {
            Severity::Warning
        } else {
            Severity::Info
        },
        entry,
        if authorization {
            "Authorization extraHeader 会绕过 credential helper，可能直接使用另一个账号的凭据"
        } else {
            "HTTP extraHeader 会改变 Git 与远端的请求"
        },
        "核对并手动移除不需要的 extraHeader；为防止泄露，ghis 不显示其值也不会自动清理",
    ));
}

fn inspect_url_rewrite(
    entry: &ConfigEntry,
    remote: Option<&Remote>,
    diagnostics: &mut Vec<GitConfigDiagnostic>,
) {
    let applies = remote.is_some_and(|remote| remote.url.starts_with(entry.value.trim()));
    diagnostics.push(finding(
        if applies {
            Severity::Warning
        } else {
            Severity::Info
        },
        entry,
        "URL rewrite 可能把显示的 remote 改写到另一个协议、主机或仓库",
        "确认改写后的目标仍属于当前 Profile；ghis 不会修改 remote 或 URL rewrite",
    ));
}

fn inspect_effective_identity(
    profile: Option<&Profile>,
    identities: Option<&EffectiveIdentities>,
    diagnostics: &mut Vec<GitConfigDiagnostic>,
) {
    let (Some(profile), Some(identities)) = (profile, identities) else {
        return;
    };
    for (role, identity) in [
        ("author", &identities.author),
        ("committer", &identities.committer),
    ] {
        if identity.name == profile.git_name && identity.email == profile.git_email {
            continue;
        }
        diagnostics.push(GitConfigDiagnostic {
            code: DiagnosticCode::EffectiveIdentityMismatch,
            severity: Severity::Error,
            key: format!("effective.{role}"),
            value: bounded(&format!("{} <{}>", identity.name, identity.email)),
            scope: "effective".into(),
            origin: "git var".into(),
            impact: bounded(&format!(
                "Git 最终使用的 {role} 与当前 Profile 的 {} <{}> 不一致",
                profile.git_name, profile.git_email
            )),
            suggestion: "在 commit 前检查环境变量、-c 参数以及 local/worktree 配置".into(),
            repair: Some(RepairAction::manual(
                "git var GIT_AUTHOR_IDENT && git var GIT_COMMITTER_IDENT",
            )),
        });
    }
}

fn finding(
    severity: Severity,
    entry: &ConfigEntry,
    impact: impl Into<String>,
    suggestion: impl Into<String>,
) -> GitConfigDiagnostic {
    let (code, repair) = classify_git_config_repair(entry);
    GitConfigDiagnostic {
        code,
        severity,
        key: redact_key(&entry.key),
        value: display_value(entry),
        scope: bounded(&entry.scope),
        origin: bounded(&entry.origin),
        impact: bounded(&impact.into()),
        suggestion: bounded(&suggestion.into()),
        repair,
    }
}

fn classify_git_config_repair(entry: &ConfigEntry) -> (DiagnosticCode, Option<RepairAction>) {
    let key = entry.key.as_str();
    let code = if matches!(key, "user.name" | "user.email") {
        DiagnosticCode::GitIdentityConflict
    } else if matches!(
        key,
        "user.signingkey" | "commit.gpgsign" | "gpg.format" | "gpg.ssh.program"
    ) {
        DiagnosticCode::SigningConflict
    } else if key == "core.sshcommand" {
        DiagnosticCode::SshCommand
    } else if is_credential_helper(key) {
        DiagnosticCode::CredentialHelper
    } else if is_extra_header(key) {
        DiagnosticCode::AuthorizationHeader
    } else {
        DiagnosticCode::UrlRewrite
    };
    // Existing Git settings, especially system/global values and credentials,
    // are never removed automatically. The command is deliberately read-only.
    let repair = RepairAction::manual(format!(
        "git config --show-origin --show-scope --get-all {}",
        shell_quote_argument(&redact_key(key))
    ));
    (code, Some(repair))
}

fn shell_quote_argument(value: &str) -> String {
    format!("'{value}'")
}

fn is_ghis_fragment(entry: &ConfigEntry) -> bool {
    let origin = entry.origin.replace('\\', "/").to_ascii_lowercase();
    origin.contains("/ghis/fragments/") || origin.contains("/ghis/fragments/config-")
}

/// Recognize only the command shape ghis writes for its managed helper. A
/// loose substring check could hide an unrelated helper containing the words
/// "ghis credential-helper".
fn is_ghis_helper(value: &str) -> bool {
    let Some(command) = value.trim().strip_prefix('!') else {
        return false;
    };
    let Some((program, arguments)) = command.split_once(" --config ") else {
        return false;
    };
    let Some(config_argument) = arguments.strip_suffix(" credential-helper") else {
        return false;
    };
    if config_argument.trim().is_empty() {
        return false;
    }
    let program = program.trim();
    let program = program
        .strip_prefix('\'')
        .and_then(|program| program.strip_suffix('\''))
        .unwrap_or(program);
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "ghis")
}

fn is_credential_helper(key: &str) -> bool {
    key == "credential.helper" || (key.starts_with("credential.") && key.ends_with(".helper"))
}

fn is_extra_header(key: &str) -> bool {
    key == "http.extraheader" || (key.starts_with("http.") && key.ends_with(".extraheader"))
}

fn is_url_rewrite(key: &str) -> bool {
    key.starts_with("url.") && (key.ends_with(".insteadof") || key.ends_with(".pushinsteadof"))
}

fn entry_applies_to_host(
    entry: &ConfigEntry,
    profile: Option<&Profile>,
    remote: Option<&Remote>,
) -> bool {
    let key = entry.key.to_ascii_lowercase();
    if matches!(key.as_str(), "credential.helper" | "http.extraheader") {
        return true;
    }
    let scoped_host = if is_credential_helper(&key) {
        key.strip_prefix("credential.")
            .and_then(|value| value.strip_suffix(".helper"))
            .and_then(|selector| crate::repo::parse_remote("config", selector).host)
    } else if is_extra_header(&key) {
        key.strip_prefix("http.")
            .and_then(|value| value.strip_suffix(".extraheader"))
            .and_then(|selector| crate::repo::parse_remote("config", selector).host)
    } else {
        None
    };
    let profile_host = profile.map(|profile| crate::github::normalize_host(&profile.host));
    let remote_host = remote
        .and_then(|remote| remote.host.as_deref())
        .map(crate::github::normalize_host);
    scoped_host.is_some_and(|host| {
        let host = crate::github::normalize_host(&host);
        profile_host
            .iter()
            .chain(remote_host.iter())
            .any(|candidate| *candidate == host)
    })
}

fn is_authorization_header(value: &str) -> bool {
    value.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, _)| {
            matches!(
                name.trim().to_ascii_lowercase().as_str(),
                "authorization" | "proxy-authorization"
            )
        })
    })
}

fn parse_git_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

fn display_value(entry: &ConfigEntry) -> String {
    let key = entry.key.as_str();
    if is_extra_header(key) {
        return "<已隐藏>".into();
    }
    if key == "core.sshcommand" {
        return "<SSH 命令内容已隐藏>".into();
    }
    if is_credential_helper(key) {
        let value = entry.value.trim();
        if value.is_empty() {
            return "<空值：重置 helper 链>".into();
        }
        if value.starts_with('!') {
            return "<shell helper，内容已隐藏>".into();
        }
        return bounded(&redact_url_userinfo(
            value.split_whitespace().next().unwrap_or("<已隐藏>"),
        ));
    }
    if key == "user.signingkey" && signing::public_key_material(entry.value.trim_start()).is_some()
    {
        return "<SSH 公钥内容已隐藏>".into();
    }
    bounded(&redact_url_userinfo(entry.value.trim()))
}

fn redact_key(key: &str) -> String {
    bounded(&redact_url_userinfo(key))
}

fn redact_url_userinfo(value: &str) -> String {
    let Some(scheme) = value.find("://") else {
        return value.to_owned();
    };
    let authority_start = scheme + 3;
    let authority_end = value[authority_start..]
        .find(['/', '?', '#'])
        .map_or(value.len(), |offset| authority_start + offset);
    let authority = &value[authority_start..authority_end];
    let Some(at) = authority.rfind('@') else {
        return value.to_owned();
    };
    format!(
        "{}<已隐藏>@{}{}",
        &value[..authority_start],
        &authority[at + 1..],
        &value[authority_end..]
    )
}

fn bounded(value: &str) -> String {
    const LIMIT: usize = 160;
    let sanitized = sanitize_display_text(value);
    let mut chars = sanitized.chars();
    let prefix = chars.by_ref().take(LIMIT).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

/// Do not let a hostile Git config value create a new terminal line, inject an
/// ANSI control sequence, or resize a TUI row. JSON would escape controls, but
/// the text doctor output and TUI consume these fields directly.
/// Escape terminal control characters before a value is rendered in text or
/// TUI output. This is also used by the TUI for data returned by external
/// agents and GitHub commands.
pub fn sanitize_display_text(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => sanitized.push_str("\\n"),
            '\r' => sanitized.push_str("\\r"),
            '\t' => sanitized.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                write!(&mut sanitized, "\\u{{{:04X}}}", character as u32)
                    .expect("writing to a string cannot fail");
            }
            character => sanitized.push(character),
        }
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GitIdentity;

    fn entry(scope: &str, origin: &str, key: &str, value: &str) -> ConfigEntry {
        ConfigEntry {
            scope: scope.into(),
            origin: origin.into(),
            key: key.into(),
            value: value.into(),
        }
    }

    fn profile() -> Profile {
        Profile {
            host: "github.com".into(),
            login: "alice".into(),
            git_name: "Alice".into(),
            git_email: "alice@users.noreply.github.com".into(),
            ..Profile::default()
        }
    }

    #[test]
    fn reports_effective_identity_mismatch_as_error() {
        let identities = EffectiveIdentities {
            author: GitIdentity {
                name: "Other".into(),
                email: "other@example.test".into(),
                raw: String::new(),
            },
            committer: GitIdentity {
                name: "Alice".into(),
                email: "alice@users.noreply.github.com".into(),
                raw: String::new(),
            },
        };
        let report = scan_entries(&[], Some(&profile()), None, Some(&identities));
        assert_eq!(report.errors, 1);
        assert_eq!(report.diagnostics[0].key, "effective.author");
    }

    #[test]
    fn redacts_authorization_and_shell_helper_values() {
        let report = scan_entries(
            &[
                entry(
                    "global",
                    "file:/tmp/config",
                    "http.https://github.com.extraheader",
                    "Authorization: basic top-secret",
                ),
                entry(
                    "global",
                    "file:/tmp/config",
                    "credential.helper",
                    "!printf password=top-secret",
                ),
            ],
            Some(&profile()),
            None,
            None,
        );
        assert_eq!(report.errors, 1);
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("top-secret"));
        assert!(json.contains("<已隐藏>"));
    }

    #[test]
    fn redacts_userinfo_from_non_shell_credential_helpers() {
        let report = scan_entries(
            &[entry(
                "global",
                "file:/tmp/config",
                "credential.helper",
                "https://user:secret@example.test/helper --flag",
            )],
            Some(&profile()),
            None,
            None,
        );
        let value = &report.diagnostics[0].value;
        assert!(!value.contains("secret"));
        assert!(value.contains("<已隐藏>@example.test"));
    }

    #[test]
    fn keeps_scope_origin_and_multivalue_helper_order() {
        let report = scan_entries(
            &[
                entry(
                    "system",
                    "file:/etc/gitconfig",
                    "credential.helper",
                    "cache",
                ),
                entry(
                    "global",
                    "file:/home/test/.gitconfig",
                    "credential.helper",
                    "",
                ),
                entry(
                    "global",
                    "file:/home/test/.gitconfig",
                    "credential.helper",
                    "store",
                ),
            ],
            Some(&profile()),
            None,
            None,
        );
        assert_eq!(report.warnings, 3);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|item| item.scope == "system" && item.origin == "file:/etc/gitconfig")
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|item| item.value.contains("重置 helper 链"))
        );
        assert_eq!(
            report
                .diagnostics
                .iter()
                .map(|item| item.value.as_str())
                .collect::<Vec<_>>(),
            vec!["cache", "<空值：重置 helper 链>", "store"]
        );
    }

    #[test]
    fn scopes_authentication_settings_to_an_exact_host() {
        let report = scan_entries(
            &[
                entry(
                    "global",
                    "file:/tmp/config",
                    "credential.https://notgithub.com.helper",
                    "store",
                ),
                entry(
                    "global",
                    "file:/tmp/config",
                    "credential.https://github.com.helper",
                    "store",
                ),
            ],
            Some(&profile()),
            None,
            None,
        );
        assert_eq!(report.diagnostics[0].severity, Severity::Info);
        assert_eq!(report.diagnostics[1].severity, Severity::Warning);
    }

    #[test]
    fn reports_proxy_authorization_and_hides_ssh_command_contents() {
        let report = scan_entries(
            &[
                entry(
                    "global",
                    "file:/tmp/config",
                    "http.https://github.com/.extraheader",
                    "Proxy-Authorization: Basic proxy-secret",
                ),
                entry(
                    "global",
                    "file:/tmp/config",
                    "core.sshcommand",
                    "ssh -o ProxyCommand='echo ssh-command-secret'",
                ),
            ],
            Some(&profile()),
            None,
            None,
        );
        assert_eq!(report.diagnostics[0].severity, Severity::Error);
        assert_eq!(report.diagnostics[0].value, "<已隐藏>");
        assert_eq!(report.diagnostics[1].value, "<SSH 命令内容已隐藏>");
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("ssh-command-secret")
        );
    }

    #[test]
    fn does_not_silently_hide_an_unrelated_helper_with_ghis_words() {
        let report = scan_entries(
            &[
                entry(
                    "worktree",
                    "file:.git/config.worktree",
                    "credential.https://github.com.helper",
                    "!printf 'ghis credential-helper'",
                ),
                entry(
                    "worktree",
                    "file:.git/config.worktree",
                    "credential.https://github.com.helper",
                    "!'/usr/bin/ghis' --config '/tmp/ghis.toml' credential-helper",
                ),
            ],
            Some(&profile()),
            None,
            None,
        );
        assert_eq!(report.diagnostics.len(), 2);
        assert_eq!(report.diagnostics[0].severity, Severity::Warning);
        assert_eq!(report.diagnostics[1].severity, Severity::Info);
        assert_eq!(report.diagnostics[0].value, "<shell helper，内容已隐藏>");
    }

    #[test]
    fn ignores_ghis_generated_fragment_entries() {
        let report = scan_entries(
            &[entry(
                "worktree",
                "file:/home/test/.config/ghis/fragments/work.gitconfig",
                "user.name",
                "Other",
            )],
            Some(&profile()),
            None,
            None,
        );
        assert!(report.diagnostics.is_empty());
    }

    #[test]
    fn ignores_profile_matching_ssh_signing_settings() {
        let mut configured = profile();
        configured.signing.enabled = true;
        configured.signing.signing_key = Some("/keys/alice-signing.pub".into());
        configured.signing.program = Some("/opt/1Password/op-ssh-sign".into());

        let report = scan_entries(
            &[
                entry("global", "file:/tmp/config", "commit.gpgsign", "true"),
                entry("global", "file:/tmp/config", "gpg.format", "ssh"),
                entry(
                    "global",
                    "file:/tmp/config",
                    "gpg.ssh.program",
                    " /opt/1Password/op-ssh-sign ",
                ),
                entry(
                    "global",
                    "file:/tmp/config",
                    "user.signingkey",
                    " /keys/alice-signing.pub ",
                ),
            ],
            Some(&configured),
            None,
            None,
        );

        assert!(report.diagnostics.is_empty());
    }

    #[test]
    fn reports_only_mismatched_profile_signing_settings() {
        let mut configured = profile();
        configured.signing.enabled = true;
        configured.signing.signing_key = Some("/keys/alice-signing.pub".into());
        configured.signing.program = Some("/opt/1Password/op-ssh-sign".into());

        let report = scan_entries(
            &[
                entry(
                    "global",
                    "file:/tmp/config",
                    "gpg.ssh.program",
                    "/usr/bin/ssh-keygen",
                ),
                entry(
                    "global",
                    "file:/tmp/config",
                    "user.signingkey",
                    "/keys/other-signing.pub",
                ),
            ],
            Some(&configured),
            None,
            None,
        );

        assert_eq!(report.warnings, 2);
        assert_eq!(
            report
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.key.as_str())
                .collect::<Vec<_>>(),
            vec!["gpg.ssh.program", "user.signingkey"]
        );
    }

    #[test]
    fn compares_inline_signing_material_without_comments_and_redacts_all_key_shapes() {
        let mut configured = profile();
        configured.signing.enabled = true;
        configured.signing.signing_key =
            Some("key::ecdsa-sha2-nistp256 AAAATEST profile comment".into());

        let report = scan_entries(
            &[entry(
                "global",
                "file:/tmp/config",
                "user.signingkey",
                "key::ecdsa-sha2-nistp256 AAAATEST global comment",
            )],
            Some(&configured),
            None,
            None,
        );

        assert!(report.diagnostics.is_empty());
        let diagnostic = finding(
            Severity::Warning,
            &entry(
                "global",
                "file:/tmp/config",
                "user.signingkey",
                "sk-ssh-ed25519@openssh.com AAAASECRET local comment",
            ),
            "impact",
            "suggestion",
        );
        assert_eq!(diagnostic.value, "<SSH 公钥内容已隐藏>");
        assert!(
            !serde_json::to_string(&diagnostic)
                .unwrap()
                .contains("AAAASECRET")
        );
        let prefixed = finding(
            Severity::Warning,
            &entry(
                "global",
                "file:/tmp/config",
                "user.signingkey",
                "key::ecdsa-sha2-nistp256 AAAAPREFIXED local comment",
            ),
            "impact",
            "suggestion",
        );
        assert_eq!(prefixed.value, "<SSH 公钥内容已隐藏>");
        assert!(
            !serde_json::to_string(&prefixed)
                .unwrap()
                .contains("AAAAPREFIXED")
        );
    }

    #[test]
    fn accepts_managed_ssh_key_as_the_default_signing_key() {
        let mut configured = profile();
        configured.signing.enabled = true;
        configured.ssh = Some(crate::config::SshProfile {
            mode: SshMode::OnePassword,
            public_key: Some("/keys/authentication.pub".into()),
            ..crate::config::SshProfile::default()
        });

        let report = scan_entries(
            &[entry(
                "global",
                "file:/tmp/config",
                "user.signingkey",
                "/keys/authentication.pub",
            )],
            Some(&configured),
            None,
            None,
        );

        assert!(report.diagnostics.is_empty());
    }

    #[test]
    fn ignores_inert_key_and_program_when_profile_signing_is_disabled() {
        let report = scan_entries(
            &[
                entry(
                    "global",
                    "file:/tmp/config",
                    "gpg.ssh.program",
                    "/opt/1Password/op-ssh-sign",
                ),
                entry(
                    "global",
                    "file:/tmp/config",
                    "user.signingkey",
                    "/keys/alice-signing.pub",
                ),
            ],
            Some(&profile()),
            None,
            None,
        );

        assert!(report.diagnostics.is_empty());
    }

    #[test]
    fn redacts_userinfo_embedded_in_url_rewrite_keys_and_values() {
        let diagnostic = finding(
            Severity::Warning,
            &entry(
                "global",
                "file:/tmp/config",
                "url.https://token@example.test/.insteadof",
                "https://secret@example.test/",
            ),
            "impact",
            "suggestion",
        );
        assert!(!diagnostic.key.contains("token"));
        assert!(!diagnostic.value.contains("secret"));
    }

    #[test]
    fn sanitizes_control_characters_in_config_provenance_and_values() {
        let diagnostic = finding(
            Severity::Warning,
            &entry(
                "global\t",
                "file:/tmp/evil\u{1b}[2J",
                "user.name",
                "Alice\nwarning\r\t\u{7}",
            ),
            "impact\nwith newline",
            "suggestion\u{1b}[31m",
        );
        let rendered = render_row(&diagnostic);
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{7}'));
        assert!(!rendered.contains("Alice\nwarning"));
        assert!(rendered.contains("Alice\\nwarning\\r\\t\\u{0007}"));
        assert!(rendered.contains("file:/tmp/evil\\u{001B}[2J"));
    }
}
