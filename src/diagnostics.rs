//! Read-only diagnostics for Git configuration that can alter identity,
//! authentication, signing, or the actual remote used by an operation.

use crate::config::{ConfigPaths, CredentialMode, Profile, SshMode};
use crate::git::{self, ConfigEntry, EffectiveIdentities};
use crate::repo::Remote;
use crate::signing;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// Report unknown ghis configuration keys for doctor-style diagnostics.
///
/// Unknown keys are legal and preserved across saves; this only surfaces
/// likely typos to the operator.  A missing or unparseable file reports
/// nothing here because the normal load path already explains those states.
pub fn unknown_config_keys_report(path: &Path) -> Vec<String> {
    crate::config::scan_unknown_config_keys(path)
}

/// Diagnostic severity shared by CLI JSON and text output.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    GitIdentityConflict,
    SigningConflict,
    SigningProgram,
    SshCommand,
    SshExternal,
    CredentialHelper,
    CredentialUsername,
    AuthorizationHeader,
    HttpExtraHeader,
    AskPass,
    UrlRewrite,
    EnvironmentOverride,
    EffectiveIdentityMismatch,
    InvalidBinding,
    BindingDrift,
    Baseline,
    ShellIntegration,
    ToolMissing,
    GhAuth,
    CredentialUnavailable,
    GithubSigningKey,
    ConfigUnknownKey,
    SshKey,
    GitConfigRead,
}

/// Which layer a finding belongs to.  Text output groups findings by this
/// order, from "settings ghis does not own" to "things ghis depends on".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    /// Settings outside ghis that can decide identity, signing or credentials.
    Foreign,
    /// The user-global Git baseline ghis owns.
    Baseline,
    /// ghis-owned fragments, bindings and hooks that drifted.
    Ownership,
    /// The identity this repository actually resolves to.
    Context,
    /// Tools and external services ghis depends on.
    Dependency,
}

impl Layer {
    pub const ORDER: [Layer; 5] = [
        Self::Foreign,
        Self::Baseline,
        Self::Ownership,
        Self::Context,
        Self::Dependency,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Foreign => "外部来源",
            Self::Baseline => "全局基线",
            Self::Ownership => "ghis 状态",
            Self::Context => "当前仓库",
            Self::Dependency => "外部依赖",
        }
    }
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
/// enter this public structure so JSON and text output cannot expose an
/// Authorization header or credential helper body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub layer: Layer,
    pub code: DiagnosticCode,
    pub severity: Severity,
    /// Where the value comes from: `scope:origin`, `env`, `ssh -G` or a tool.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub key: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub value: String,
    pub impact: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub suggestion: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repair: Option<RepairAction>,
}

impl Finding {
    pub fn new(
        layer: Layer,
        code: DiagnosticCode,
        severity: Severity,
        impact: impl Into<String>,
    ) -> Self {
        Self {
            layer,
            code,
            severity,
            source: String::new(),
            key: String::new(),
            value: String::new(),
            impact: bounded(&impact.into()),
            suggestion: String::new(),
            repair: None,
        }
    }

    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = bounded(&source.into());
        self
    }

    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.key = bounded(&key.into());
        self
    }

    pub fn value(mut self, value: impl Into<String>) -> Self {
        self.value = bounded(&value.into());
        self
    }

    pub fn suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = bounded(&suggestion.into());
        self
    }

    pub fn repair(mut self, repair: RepairAction) -> Self {
        self.repair = Some(repair);
        self
    }

    pub fn with_severity(mut self, severity: Severity) -> Self {
        self.severity = severity;
        self
    }
}

/// Structured report consumed by the JSON and text front ends.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DiagnosticReport {
    pub info: usize,
    pub warnings: usize,
    pub errors: usize,
    pub findings: Vec<Finding>,
}

impl DiagnosticReport {
    pub fn from_findings(mut findings: Vec<Finding>) -> Self {
        // Keep material ordering inside a layer (Git's own config order decides
        // multi-value helper precedence) and only group by layer.
        findings.sort_by_key(|finding| finding.layer as u8);
        let count = |severity| {
            findings
                .iter()
                .filter(|finding| finding.severity == severity)
                .count()
        };
        Self {
            info: count(Severity::Info),
            warnings: count(Severity::Warning),
            errors: count(Severity::Error),
            findings,
        }
    }

    pub fn extend(&mut self, findings: impl IntoIterator<Item = Finding>) {
        self.findings.extend(findings);
        *self = Self::from_findings(std::mem::take(&mut self.findings));
    }
}

/// Compact summary suitable for a status row or a doctor heading.
pub fn summary_line(report: &DiagnosticReport) -> String {
    format!(
        "信息={} 警告={} 错误={}",
        report.info, report.warnings, report.errors
    )
}

/// Render one finding for text terminal output.
pub fn render_row(item: &Finding) -> String {
    let mut rendered = format!(
        "{}\t{}={}\t来源={}\t影响={}",
        item.severity.label(),
        item.key,
        item.value,
        item.source,
        item.impact
    );
    if !item.suggestion.is_empty() {
        rendered.push_str(&format!("\t建议={}", item.suggestion));
    }
    rendered
}

/// Inspect Git's merged configuration without changing any setting.
pub fn scan_git_config(
    cwd: &Path,
    paths: &ConfigPaths,
    profile: Option<&Profile>,
    identities: Option<&EffectiveIdentities>,
) -> git::Result<DiagnosticReport> {
    let entries = git::config_entries(cwd)?;
    Ok(scan_entries(&entries, paths, profile, identities))
}

/// Analyze entries already obtained from Git. This pure form keeps tests
/// independent from the caller's real HOME and global Git configuration.
///
/// The judgment is absolute rather than profile-relative: any identity,
/// signing or credential setting that ghis did not write is a finding, even
/// when its value happens to match the current Profile.  A matching value is
/// still foreign state that can decide an unbound repository or another
/// Profile.
pub fn scan_entries(
    entries: &[ConfigEntry],
    paths: &ConfigPaths,
    profile: Option<&Profile>,
    identities: Option<&EffectiveIdentities>,
) -> DiagnosticReport {
    let visible = effective_credential_helpers(entries);
    let owned = owned_entries(entries, paths);
    let mut findings = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if owned[index] {
            continue;
        }
        if is_credential_helper(&entry.key) && !visible[index] {
            continue;
        }
        let Some(category) = foreign_category(&entry.key) else {
            continue;
        };
        findings.push(foreign_finding(entry, category, entries, &owned, index));
    }
    inspect_effective_identity(profile, identities, &mut findings);
    DiagnosticReport::from_findings(findings)
}

/// Entry-level ownership.  A fragment or the baseline path is ghis's; so is
/// the exact credential-helper string ghis writes and the empty value directly
/// before it that cuts the inherited chain.  Everything else is foreign,
/// including user entries that happen to equal a Profile value.
fn owned_entries(entries: &[ConfigEntry], paths: &ConfigPaths) -> Vec<bool> {
    let mut owned = entries
        .iter()
        .map(|entry| origin_is_ghis(entry, paths))
        .collect::<Vec<_>>();
    for (index, entry) in entries.iter().enumerate() {
        if !is_credential_helper(&entry.key) {
            continue;
        }
        if is_ghis_helper(&entry.value)
            || (entry.value.trim().is_empty()
                && entries.get(index + 1).is_some_and(|next| {
                    next.scope == entry.scope
                        && next.key.eq_ignore_ascii_case(&entry.key)
                        && is_ghis_helper(&next.value)
                }))
        {
            owned[index] = true;
        }
    }
    owned
}

fn origin_is_ghis(entry: &ConfigEntry, paths: &ConfigPaths) -> bool {
    let Some(path) = entry.origin.strip_prefix("file:") else {
        return false;
    };
    let path = Path::new(path);
    path_matches(path, &crate::baseline::baseline_path(paths))
        || path_is_under(path, &paths.fragments_dir)
}

#[cfg(windows)]
fn path_matches(path: &Path, target: &Path) -> bool {
    path.to_string_lossy()
        .eq_ignore_ascii_case(&target.to_string_lossy())
}

#[cfg(not(windows))]
fn path_matches(path: &Path, target: &Path) -> bool {
    path == target
}

#[cfg(windows)]
fn path_is_under(path: &Path, directory: &Path) -> bool {
    let path = path.to_string_lossy().to_ascii_lowercase();
    let directory = directory.to_string_lossy().to_ascii_lowercase();
    path == directory
        || path.starts_with(&format!("{}{}", directory, std::path::MAIN_SEPARATOR))
        || path.starts_with(&format!("{directory}/"))
        || path.starts_with(&format!("{directory}\\"))
}

#[cfg(not(windows))]
fn path_is_under(path: &Path, directory: &Path) -> bool {
    path.starts_with(directory)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForeignCategory {
    Identity,
    Signing,
    SshCommand,
    CredentialHelper,
    CredentialUsername,
    ExtraHeader,
    AskPass,
    UrlRewrite,
}

/// The identity-plane keys ghis owns.  `gpg.ssh.allowedSignersFile` is
/// deliberately absent: it verifies signatures and does not choose an identity.
fn foreign_category(key: &str) -> Option<ForeignCategory> {
    let key = key.to_ascii_lowercase();
    match key.as_str() {
        "user.name" | "user.email" | "user.useconfigonly" | "author.name" | "author.email"
        | "committer.name" | "committer.email" => Some(ForeignCategory::Identity),
        "user.signingkey"
        | "commit.gpgsign"
        | "tag.gpgsign"
        | "push.gpgsign"
        | "gpg.format"
        | "gpg.program"
        | "gpg.ssh.defaultkeycommand" => Some(ForeignCategory::Signing),
        "core.sshcommand" => Some(ForeignCategory::SshCommand),
        "core.askpass" => Some(ForeignCategory::AskPass),
        _ if key.starts_with("gpg.") && key.ends_with(".program") => Some(ForeignCategory::Signing),
        _ if is_credential_helper(&key) => Some(ForeignCategory::CredentialHelper),
        _ if key.starts_with("credential.")
            && (key == "credential.username" || key.ends_with(".username")) =>
        {
            Some(ForeignCategory::CredentialUsername)
        }
        _ if is_extra_header(&key) => Some(ForeignCategory::ExtraHeader),
        _ if is_url_rewrite(&key) => Some(ForeignCategory::UrlRewrite),
        _ => None,
    }
}

fn foreign_finding(
    entry: &ConfigEntry,
    category: ForeignCategory,
    entries: &[ConfigEntry],
    owned: &[bool],
    index: usize,
) -> Finding {
    let authorization = is_authorization_header(&entry.value);
    let (code, severity, impact, suggestion) = match category {
        ForeignCategory::Identity => (
            DiagnosticCode::GitIdentityConflict,
            Severity::Error,
            "全局、system 或仓库配置提供的身份会与 Profile 竞争；author/committer 键优先于 user 键",
            "删除这些条目，让身份只来自 Profile fragment",
        ),
        ForeignCategory::Signing => (
            DiagnosticCode::SigningConflict,
            Severity::Error,
            "签名设置可能覆盖当前 Profile 的 SSH signing 配置或强制启用另一种签名方式",
            "把账号相关签名设置放进 Profile；ghis 不会自动删除现有配置",
        ),
        ForeignCategory::SshCommand => (
            DiagnosticCode::SshCommand,
            Severity::Error,
            "core.sshCommand 会改变 SSH push 使用的程序、Agent 或 key",
            "把 SSH 连接设置放进 Profile 的 ssh.mode = managed/1password",
        ),
        ForeignCategory::CredentialHelper => (
            DiagnosticCode::CredentialHelper,
            Severity::Error,
            "credential helper 参与凭据查找；错误的链顺序可能让 Git 尝试另一个账号",
            "删除该 helper，让绑定仓库只使用 ghis 的 credential helper",
        ),
        ForeignCategory::CredentialUsername => (
            DiagnosticCode::CredentialUsername,
            Severity::Error,
            "credential username 会固定 HTTPS 基本认证的用户名，覆盖 Profile 的账号",
            "删除该 username 设置，让 ghis 按 Profile 提供用户名",
        ),
        ForeignCategory::AskPass => (
            DiagnosticCode::AskPass,
            Severity::Error,
            "core.askPass 让 Git 从另一个程序取密码，可能绕过 credential helper 和 Profile 凭据策略",
            "删除该设置；需要自定义取件时改用 Profile 的 credential_mode = command",
        ),
        ForeignCategory::ExtraHeader if authorization => (
            DiagnosticCode::AuthorizationHeader,
            Severity::Error,
            "Authorization extraHeader 会绕过 credential helper，可能直接使用另一个账号的凭据",
            "核对并手动移除不需要的 extraHeader；为防止泄露，ghis 不显示其值也不会自动清理",
        ),
        ForeignCategory::ExtraHeader => (
            DiagnosticCode::HttpExtraHeader,
            Severity::Warning,
            "HTTP extraHeader 会改变 Git 与远端的请求",
            "核对并手动移除不需要的 extraHeader",
        ),
        ForeignCategory::UrlRewrite => (
            DiagnosticCode::UrlRewrite,
            Severity::Warning,
            "URL rewrite 可能把显示的 remote 改写到另一个协议、主机或仓库",
            "确认改写后的目标仍属于当前 Profile；ghis 不会修改 remote 或 URL rewrite",
        ),
    };
    let mut suggestion = suggestion.to_owned();
    if entry.scope == "system" {
        suggestion.push_str("（system 配置需要管理员权限）");
    }
    Finding::new(Layer::Foreign, code, severity, impact)
        .source(format!("{}:{}", entry.scope, entry.origin))
        .key(redact_key(&entry.key))
        .value(display_value(entry))
        .suggestion(suggestion)
        .repair(foreign_repair(entry, entries, owned, index))
}

/// Suggested repair is read-only and manual: ghis never deletes a setting the
/// user wrote.  Choose `--edit` when the same file also holds a ghis-owned
/// value under the key, and when the key itself cannot be shown safely.
fn foreign_repair(
    entry: &ConfigEntry,
    entries: &[ConfigEntry],
    owned: &[bool],
    index: usize,
) -> RepairAction {
    let key = redact_key(&entry.key);
    let hidden_key = key != entry.key;
    let shared_origin = entries.iter().enumerate().any(|(other, candidate)| {
        other != index
            && owned[other]
            && candidate.origin == entry.origin
            && candidate.key.eq_ignore_ascii_case(&entry.key)
    });
    if let Some(file) = entry.origin.strip_prefix("file:")
        && !hidden_key
        && !shared_origin
    {
        return RepairAction::manual(format!(
            "git config --file {} --unset-all {}",
            shell_quote_argument(file),
            shell_quote_argument(&key)
        ));
    }
    if let Some(file) = entry.origin.strip_prefix("file:") {
        return RepairAction::manual(format!(
            "git config --file {} --edit",
            shell_quote_argument(file)
        ));
    }
    RepairAction::manual(format!(
        "git config --show-origin --show-scope --get-all {}",
        shell_quote_argument(&key)
    ))
}

fn inspect_effective_identity(
    profile: Option<&Profile>,
    identities: Option<&EffectiveIdentities>,
    findings: &mut Vec<Finding>,
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
        findings.push(
            Finding::new(
                Layer::Context,
                DiagnosticCode::EffectiveIdentityMismatch,
                Severity::Error,
                format!(
                    "Git 最终使用的 {role} 与当前 Profile 的 {} <{}> 不一致",
                    profile.git_name, profile.git_email
                ),
            )
            .source("git var")
            .key(format!("effective.{role}"))
            .value(format!("{} <{}>", identity.name, identity.email))
            .suggestion("在 commit 前检查环境变量、-c 参数以及 local/worktree 配置")
            .repair(RepairAction::manual(
                "git var GIT_AUTHOR_IDENT && git var GIT_COMMITTER_IDENT",
            )),
        );
    }
}

fn shell_quote_argument(value: &str) -> String {
    format!("'{value}'")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvironmentCategory {
    Identity,
    Token,
    Transport,
    ConfigSelection,
}

fn environment_category(name: &str) -> Option<EnvironmentCategory> {
    match name {
        "GIT_AUTHOR_NAME" | "GIT_AUTHOR_EMAIL" | "GIT_COMMITTER_NAME" | "GIT_COMMITTER_EMAIL" => {
            Some(EnvironmentCategory::Identity)
        }
        "GH_TOKEN"
        | "GITHUB_TOKEN"
        | "GH_ENTERPRISE_TOKEN"
        | "GITHUB_ENTERPRISE_TOKEN"
        | "GH_HOST"
        | "GH_CONFIG_DIR" => Some(EnvironmentCategory::Token),
        "GIT_SSH_COMMAND" | "GIT_SSH" | "GIT_ASKPASS" | "SSH_ASKPASS" => {
            Some(EnvironmentCategory::Transport)
        }
        "GIT_CONFIG_GLOBAL" | "GIT_CONFIG_SYSTEM" | "GIT_CONFIG_NOSYSTEM" | "GIT_CONFIG_COUNT" => {
            Some(EnvironmentCategory::ConfigSelection)
        }
        _ => None,
    }
}

/// Names of environment variables that can decide identity, credentials or
/// transport.  Values are never read: reporting the name is enough to explain
/// why a command used another account, and a token must not enter a report.
pub fn scan_process_environment() -> Vec<Finding> {
    scan_environment(std::env::vars_os().map(|(name, _)| name.to_string_lossy().into_owned()))
}

pub fn scan_environment<I>(names: I) -> Vec<Finding>
where
    I: IntoIterator<Item = String>,
{
    let names = names
        .into_iter()
        .filter(|name| environment_category(name).is_some())
        .collect::<std::collections::BTreeSet<_>>();
    names
        .into_iter()
        .map(|name| {
            let (severity, impact, suggestion) =
                match environment_category(&name).expect("filtered to known categories") {
                    EnvironmentCategory::Identity => (
                        Severity::Error,
                        "环境变量身份优先于所有 Git 配置，会改变实际提交身份",
                        "在当前 Shell 中取消该变量；ghis 不修改环境变量",
                    ),
                    EnvironmentCategory::Token => (
                        Severity::Warning,
                        "该变量会让 ghis 调用 gh 时使用另一凭据或实例",
                        "在当前 Shell 中取消该变量；ghis 不读取或修改它",
                    ),
                    EnvironmentCategory::Transport => (
                        Severity::Warning,
                        "该变量会替换 SSH 或凭据提示程序，可能绕过 Profile 的传输策略",
                        "在当前 Shell 中取消该变量",
                    ),
                    EnvironmentCategory::ConfigSelection => (
                        Severity::Info,
                        "该变量改变 Git 读取的全局或 system 配置文件",
                        "确认目标文件仍是预期来源；ghis 不修改环境变量",
                    ),
                };
            Finding::new(
                Layer::Foreign,
                DiagnosticCode::EnvironmentOverride,
                severity,
                impact,
            )
            .source("env")
            .key(name)
            .value("<已设置>")
            .suggestion(suggestion)
        })
        .collect()
}

/// Whether an externally managed SSH configuration can decide this identity.
///
/// Only external SSH with no ghis-managed key is relevant: a managed profile
/// runs `ssh -F <generated config>`, so the user's `~/.ssh/config` cannot pick
/// another key for it.  `none` profiles matter even over HTTPS because SSH is
/// the transport that replaces the refused credential.
pub fn scan_ssh(profile: Option<&Profile>, remote: Option<&Remote>) -> Vec<Finding> {
    let Some(profile) = profile else {
        return Vec::new();
    };
    let external = profile
        .ssh
        .as_ref()
        .is_none_or(|ssh| ssh.mode == SshMode::External);
    if !external {
        return Vec::new();
    }
    let relevant = profile.credential_mode() == CredentialMode::None
        || remote.is_some_and(|remote| remote.transport.is_ssh());
    if !relevant {
        return Vec::new();
    }
    let host = profile.host.split(':').next().unwrap_or(&profile.host);
    if host.is_empty() {
        return Vec::new();
    }
    let Some(configured) = ssh_settings(host, None) else {
        return Vec::new();
    };
    let Some(defaults) = ssh_settings(host, Some(empty_ssh_config())) else {
        return Vec::new();
    };
    let differences = ssh_differences(&configured, &defaults);
    // Report which settings differ, never their values: an IdentityFile or
    // ProxyCommand can contain paths and commands the user did not ask to share.
    let value = if differences.is_empty() {
        "<未检测到差异>".to_owned()
    } else {
        format!("差异：{}", differences.join("、"))
    };
    vec![
        Finding::new(
            Layer::Foreign,
            DiagnosticCode::SshExternal,
            Severity::Warning,
            "系统 SSH 配置会决定 SSH remote 使用的 key 与 Agent，ghis 无法保证它与 Profile 一致",
        )
        .source(format!("ssh -G {host}"))
        .key("ssh")
        .value(value)
        .suggestion("需要 ghis 接管时，在 Profile 中设置 ssh.mode = managed 或 1password"),
    ]
}

/// Settings that can select a key, agent or route.  Only the key names leave
/// this function; the values are compared, never rendered.
fn ssh_differences(
    configured: &BTreeMap<String, Vec<String>>,
    defaults: &BTreeMap<String, Vec<String>>,
) -> Vec<&'static str> {
    const COMPARED: [&str; 9] = [
        "user",
        "hostname",
        "port",
        "identityfile",
        "identityagent",
        "identitiesonly",
        "proxycommand",
        "proxyjump",
        "certificatefile",
    ];
    COMPARED
        .iter()
        .filter(|key| configured.get(**key) != defaults.get(**key))
        .copied()
        .collect()
}

fn ssh_settings(host: &str, config: Option<&str>) -> Option<BTreeMap<String, Vec<String>>> {
    let mut command = std::process::Command::new("ssh");
    if let Some(config) = config {
        command.arg("-F").arg(config);
    }
    let output = command.arg("-G").arg(host).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let mut settings: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim_end();
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        settings
            .entry(key.to_ascii_lowercase())
            .or_default()
            .push(value.trim().to_owned());
    }
    Some(settings)
}

#[cfg(windows)]
fn empty_ssh_config() -> &'static str {
    "NUL"
}

#[cfg(not(windows))]
fn empty_ssh_config() -> &'static str {
    "/dev/null"
}

/// The global baseline is ghis-owned state; a missing or stale file is a
/// finding even when no repository is open.
pub fn baseline_findings(paths: &ConfigPaths) -> Vec<Finding> {
    let status = match crate::baseline::status(paths) {
        Ok(status) => status,
        Err(error) => {
            return vec![
                Finding::new(
                    Layer::Baseline,
                    DiagnosticCode::Baseline,
                    Severity::Warning,
                    format!("无法读取全局 Git 基线：{error}"),
                )
                .source("git config --global include.path"),
            ];
        }
    };
    if status.up_to_date() {
        return Vec::new();
    }
    let (value, impact) = if !status.installed() {
        (
            "<未设置>",
            "未安装全局 Git 基线；未绑定 Profile 的仓库可能用自动推断的身份提交",
        )
    } else {
        (
            "<需更新>",
            "全局 Git 基线文件已过期，或同一路径被 include 多次",
        )
    };
    vec![
        Finding::new(
            Layer::Baseline,
            DiagnosticCode::Baseline,
            Severity::Warning,
            impact,
        )
        .source("git config --global include.path")
        .key("include.path")
        .value(value)
        .suggestion("运行 `ghis gitconfig setup`；它只写入 ghis 自己的 include 条目")
        .repair(RepairAction::confirmation("ghis gitconfig setup")),
    ]
}

/// Return only helper entries that can participate in the final helper chain.
/// Git treats an empty helper value as a reset, so values before the last reset
/// for the same key are shadowed. ghis's shell wrapper may also append the
/// same reset/helper pair on the command line; those ephemeral duplicates are
/// omitted when an equivalent non-command pair is already present.
fn effective_credential_helpers(entries: &[ConfigEntry]) -> Vec<bool> {
    let mut visible = vec![true; entries.len()];

    for (index, entry) in entries.iter().enumerate() {
        if !is_credential_helper(&entry.key) || entry.scope != "command" {
            continue;
        }
        let duplicate = is_ghis_helper(&entry.value)
            || (entry.value.trim().is_empty()
                && entries[index + 1..].iter().any(|next| {
                    next.scope == "command"
                        && next.key.eq_ignore_ascii_case(&entry.key)
                        && is_ghis_helper(&next.value)
                }));
        if duplicate
            && entries[..index].iter().any(|previous| {
                previous.scope != "command"
                    && previous.key.eq_ignore_ascii_case(&entry.key)
                    && (is_ghis_helper(&previous.value) || previous.value.trim().is_empty())
            })
        {
            visible[index] = false;
        }
    }

    let mut last_reset = std::collections::BTreeMap::<String, usize>::new();
    for (index, entry) in entries.iter().enumerate() {
        if visible[index] && is_credential_helper(&entry.key) && entry.value.trim().is_empty() {
            last_reset.insert(entry.key.to_ascii_lowercase(), index);
        }
    }
    for (index, entry) in entries.iter().enumerate() {
        if !visible[index] || !is_credential_helper(&entry.key) {
            continue;
        }
        if let Some(reset) = last_reset.get(&entry.key.to_ascii_lowercase()) {
            visible[index] = index >= *reset;
        }
    }
    visible
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

/// Sanitize untrusted output captured from an external diagnostic probe.
///
/// Probe output can contain credentials as well as terminal sequences crafted
/// to make a JSON consumer display misleading diagnostics. Keep useful text
/// such as exit statuses, but hide common credential/header forms, public-key
/// material and comments, remove ANSI sequences, escape remaining controls,
/// and cap the resulting field.
pub fn sanitize_external_output(value: &str) -> String {
    let without_ansi = strip_ansi_sequences(value);
    let mut redacted = Vec::new();
    for line in without_ansi.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        redacted.push(redact_external_line(trimmed));
    }
    bounded(&redacted.join("\\n"))
}

fn redact_external_line(line: &str) -> String {
    let words = line.split_whitespace().collect::<Vec<_>>();
    if let Some(index) = words.iter().enumerate().position(|(index, word)| {
        let word = word.trim_matches(|character: char| {
            matches!(character, '"' | '\'' | '(' | '[' | '{' | ':' | ',')
        });
        is_public_key_algorithm(word)
            && words.get(index + 1).is_some_and(|material| {
                !material.starts_with('-')
                    && material.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=')
                    })
            })
    }) {
        let prefix = words[..index].join(" ");
        return if prefix.is_empty() {
            "<SSH 公钥内容已隐藏>".into()
        } else {
            format!("{prefix} <SSH 公钥内容已隐藏>")
        };
    }

    let lower = line.to_ascii_lowercase();
    for header in ["authorization:", "proxy-authorization:"] {
        if let Some(index) = lower.find(header) {
            return format!(
                "{}{} <已隐藏>",
                &line[..index],
                &line[index..index + header.len()]
            );
        }
    }

    let mut output = Vec::with_capacity(words.len());
    let mut hide_next = false;
    for word in words {
        if hide_next {
            output.push("<已隐藏>".to_owned());
            hide_next = false;
            continue;
        }
        let lower = word.to_ascii_lowercase();
        if lower == "bearer" {
            output.push(word.to_owned());
            hide_next = true;
            continue;
        }
        let separator = word.find(['=', ':']);
        if let Some(index) = separator {
            let name = lower[..index].trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '_' && character != '-'
            });
            if is_sensitive_external_name(name) {
                output.push(format!("{}<已隐藏>", &word[..=index]));
                continue;
            }
        }
        if is_sensitive_external_name(lower.trim_matches(|character: char| {
            !character.is_ascii_alphanumeric() && character != '_' && character != '-'
        })) {
            output.push(word.to_owned());
            hide_next = true;
            continue;
        }
        output.push(word.to_owned());
    }
    output.join(" ")
}

fn is_public_key_algorithm(value: &str) -> bool {
    value.starts_with("ssh-ed25519")
        || value.starts_with("ssh-rsa")
        || value.starts_with("ecdsa-sha2-")
        || value.starts_with("sk-ssh-")
        || value.starts_with("sk-ecdsa-")
        || value.starts_with("rsa-sha2-")
}

fn is_sensitive_external_name(name: &str) -> bool {
    matches!(
        name,
        "token"
            | "access_token"
            | "auth_token"
            | "github_token"
            | "gh_token"
            | "api_key"
            | "apikey"
            | "secret"
    )
}

fn strip_ansi_sequences(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            output.push(character);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                let mut previous_escape = false;
                for next in chars.by_ref() {
                    if next == '\u{7}' || previous_escape && next == '\\' {
                        break;
                    }
                    previous_escape = next == '\u{1b}';
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    output
}

/// Build the explicitly requested share-safe JSON view without changing the
/// long-standing default JSON schema. Structural fields remain present, while
/// environment-specific paths, external tool details, identity data and
/// executable repair commands are replaced in-place.
pub fn redact_json_value(value: &mut Value) {
    redact_json_node(None, value);
}

fn redact_json_node(key: Option<&str>, value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (field, child) in object {
                if redacted_json_field(key, field) {
                    if !child.is_null() {
                        *child = Value::String(redacted_json_placeholder(field).into());
                    }
                } else {
                    redact_json_node(Some(field), child);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_json_node(key, item);
            }
        }
        Value::String(text) => {
            let sanitized = sanitize_display_text(text);
            if contains_sensitive_text(&sanitized) || contains_absolute_path(&sanitized) {
                *text = "<已隐藏>".into();
            } else {
                *text = sanitized;
            }
        }
        _ => {}
    }
}

fn redacted_json_field(parent: Option<&str>, field: &str) -> bool {
    if parent == Some("operation_summary") {
        return false;
    }
    matches!(
        field,
        "repository"
            | "origin"
            | "socket"
            | "path"
            | "key"
            | "version"
            | "shell_kind"
            | "name"
            | "email"
            | "command"
            | "health_marker"
    ) || {
        let lower = field.to_ascii_lowercase();
        matches!(
            lower.as_str(),
            "token" | "access_token" | "secret" | "password" | "authorization"
        )
    }
}

fn redacted_json_placeholder(field: &str) -> &'static str {
    match field {
        "repository" | "origin" | "socket" | "path" => "<路径已隐藏>",
        "command" => "<命令已隐藏>",
        "key" => "<密钥已隐藏>",
        "version" | "shell_kind" => "<工具信息已隐藏>",
        "name" | "email" => "<身份信息已隐藏>",
        _ => "<已隐藏>",
    }
}

fn contains_sensitive_text(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("key::")
        || lower.contains("authorization:")
        || lower.contains("password=")
        || lower.contains("token=")
        || lower.contains("secret=")
}

fn contains_absolute_path(value: &str) -> bool {
    if value.starts_with('/') || value.contains("file:/") {
        return true;
    }
    value.split_whitespace().any(|word| {
        let word = word.trim_matches(|character: char| {
            matches!(
                character,
                '`' | '\'' | '"' | '(' | ')' | '[' | ']' | ',' | ';'
            )
        });
        word.starts_with('/')
            || word.starts_with("~/")
            || (word.len() >= 3
                && word.as_bytes()[1] == b':'
                && matches!(word.as_bytes()[2], b'/' | b'\\'))
    })
}

/// Escape terminal control characters before a value is rendered in text.
/// JSON would escape controls, but the text doctor output consumes these
/// fields directly.
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
    use crate::config::ConfigPaths;
    use crate::git::GitIdentity;
    use tempfile::TempDir;

    fn entry(scope: &str, origin: &str, key: &str, value: &str) -> ConfigEntry {
        ConfigEntry {
            scope: scope.into(),
            origin: origin.into(),
            key: key.into(),
            value: value.into(),
        }
    }

    fn paths(temp: &TempDir) -> ConfigPaths {
        let config_dir = temp.path().join("config/ghis");
        ConfigPaths {
            config_file: config_dir.join("config.toml"),
            cache_dir: temp.path().join("cache/ghis"),
            state_dir: temp.path().join("state/ghis"),
            fragments_dir: config_dir.join("fragments"),
            log_file: temp.path().join("state/ghis/log"),
            repositories_file: temp.path().join("state/ghis/repositories.json"),
            config_dir,
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

    fn identities(name: &str, email: &str) -> EffectiveIdentities {
        EffectiveIdentities {
            author: GitIdentity {
                name: name.into(),
                email: email.into(),
                raw: String::new(),
            },
            committer: GitIdentity {
                name: name.into(),
                email: email.into(),
                raw: String::new(),
            },
        }
    }

    #[test]
    fn reports_effective_identity_mismatch_as_error() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let report = scan_entries(
            &[],
            &paths(&temp),
            Some(&profile()),
            Some(&identities("Other", "other@example.test")),
        );
        assert_eq!(report.errors, 2);
        assert!(
            report
                .findings
                .iter()
                .all(|item| item.layer == Layer::Context)
        );
        assert!(
            report
                .findings
                .iter()
                .any(|item| item.key == "effective.author")
        );
    }

    #[test]
    fn a_global_identity_that_matches_the_profile_is_still_foreign() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let configured = profile();
        let report = scan_entries(
            &[
                entry(
                    "global",
                    "file:/home/test/.gitconfig",
                    "user.name",
                    &configured.git_name,
                ),
                entry(
                    "global",
                    "file:/home/test/.gitconfig",
                    "user.email",
                    &configured.git_email,
                ),
            ],
            &paths(&temp),
            Some(&configured),
            None,
        );
        assert_eq!(report.errors, 2);
        assert!(report.findings.iter().all(|item| {
            item.layer == Layer::Foreign
                && item.code == DiagnosticCode::GitIdentityConflict
                && item.source == "global:file:/home/test/.gitconfig"
        }));
    }

    #[test]
    fn reports_role_specific_identity_keys() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let report = scan_entries(
            &[entry(
                "global",
                "file:/home/test/.gitconfig",
                "author.email",
                "other@example.test",
            )],
            &paths(&temp),
            Some(&profile()),
            None,
        );
        assert_eq!(report.errors, 1);
        assert_eq!(report.findings[0].key, "author.email");
        assert_eq!(report.findings[0].value, "other@example.test");
    }

    #[test]
    fn ignores_ghis_fragments_and_the_generated_baseline() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let paths = paths(&temp);
        let fragment = paths.fragments_dir.join("work.gitconfig");
        let baseline = crate::baseline::baseline_path(&paths);
        let report = scan_entries(
            &[
                entry(
                    "worktree",
                    &format!("file:{}", fragment.display()),
                    "user.name",
                    "Other",
                ),
                entry(
                    "global",
                    &format!("file:{}", baseline.display()),
                    "user.useconfigonly",
                    "true",
                ),
            ],
            &paths,
            Some(&profile()),
            None,
        );
        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }

    #[test]
    fn redacts_authorization_and_shell_helper_values() {
        let temp = tempfile::tempdir().expect("temporary directory");
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
            &paths(&temp),
            Some(&profile()),
            None,
        );
        assert_eq!(report.errors, 2);
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("top-secret"));
        assert!(json.contains("<已隐藏>"));
    }

    #[test]
    fn redacts_userinfo_from_non_shell_credential_helpers() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let report = scan_entries(
            &[entry(
                "global",
                "file:/tmp/config",
                "credential.helper",
                "https://user:secret@example.test/helper --flag",
            )],
            &paths(&temp),
            Some(&profile()),
            None,
        );
        let value = &report.findings[0].value;
        assert!(!value.contains("secret"));
        assert!(value.contains("<已隐藏>@example.test"));
    }

    #[test]
    fn keeps_scope_origin_and_multivalue_helper_order() {
        let temp = tempfile::tempdir().expect("temporary directory");
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
            &paths(&temp),
            Some(&profile()),
            None,
        );
        assert_eq!(report.errors, 2);
        assert!(
            report
                .findings
                .iter()
                .all(|item| item.source == "global:file:/home/test/.gitconfig")
        );
        assert_eq!(
            report
                .findings
                .iter()
                .map(|item| item.value.as_str())
                .collect::<Vec<_>>(),
            vec!["<空值：重置 helper 链>", "store"]
        );
    }

    #[test]
    fn a_later_reset_shadows_earlier_foreign_helpers() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let helper = "!'/usr/bin/ghis' --config '/tmp/ghis.toml' credential-helper";
        let report = scan_entries(
            &[
                entry(
                    "global",
                    "file:/home/test/.gitconfig",
                    "credential.https://github.com.helper",
                    "",
                ),
                entry(
                    "global",
                    "file:/home/test/.gitconfig",
                    "credential.https://github.com.helper",
                    "!/usr/bin/gh auth git-credential",
                ),
                entry(
                    "worktree",
                    "file:.git/config.worktree",
                    "credential.https://github.com.helper",
                    "",
                ),
                entry(
                    "worktree",
                    "file:.git/config.worktree",
                    "credential.https://github.com.helper",
                    helper,
                ),
                entry(
                    "command",
                    "command line:",
                    "credential.https://github.com.helper",
                    "",
                ),
                entry(
                    "command",
                    "command line:",
                    "credential.https://github.com.helper",
                    helper,
                ),
            ],
            &paths(&temp),
            Some(&profile()),
            None,
        );
        // The worktree reset shadows the global pair, and the worktree pair is
        // the helper ghis itself wrote; the wrapper's command-line duplicate is
        // invisible.
        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }

    #[test]
    fn credential_helpers_for_other_hosts_are_still_foreign() {
        let temp = tempfile::tempdir().expect("temporary directory");
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
            &paths(&temp),
            Some(&profile()),
            None,
        );
        assert_eq!(report.errors, 2);
        assert!(
            report
                .findings
                .iter()
                .all(|item| item.code == DiagnosticCode::CredentialHelper)
        );
    }

    #[test]
    fn reports_proxy_authorization_and_hides_ssh_command_contents() {
        let temp = tempfile::tempdir().expect("temporary directory");
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
            &paths(&temp),
            Some(&profile()),
            None,
        );
        assert_eq!(report.errors, 2);
        assert_eq!(report.findings[0].value, "<已隐藏>");
        assert_eq!(report.findings[1].value, "<SSH 命令内容已隐藏>");
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("ssh-command-secret")
        );
    }

    #[test]
    fn does_not_silently_hide_an_unrelated_helper_with_ghis_words() {
        let temp = tempfile::tempdir().expect("temporary directory");
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
            &paths(&temp),
            Some(&profile()),
            None,
        );
        assert_eq!(report.errors, 1);
        assert_eq!(report.findings[0].value, "<shell helper，内容已隐藏>");
    }

    #[test]
    fn a_global_signing_setting_is_foreign_even_when_it_matches_the_profile() {
        let temp = tempfile::tempdir().expect("temporary directory");
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
            &paths(&temp),
            Some(&configured),
            None,
        );

        assert_eq!(report.errors, 4);
        assert!(
            report
                .findings
                .iter()
                .all(|item| item.code == DiagnosticCode::SigningConflict)
        );
    }

    #[test]
    fn redacts_userinfo_embedded_in_url_rewrite_keys_and_values() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let report = scan_entries(
            &[entry(
                "global",
                "file:/tmp/config",
                "url.https://token@example.test/.insteadof",
                "https://secret@example.test/",
            )],
            &paths(&temp),
            Some(&profile()),
            None,
        );
        assert_eq!(report.warnings, 1);
        assert_eq!(report.findings[0].code, DiagnosticCode::UrlRewrite);
        assert!(!report.findings[0].key.contains("token"));
        assert!(!report.findings[0].value.contains("secret"));
    }

    #[test]
    fn sanitizes_control_characters_in_config_provenance_and_values() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let report = scan_entries(
            &[entry(
                "global\t",
                "file:/tmp/evil\u{1b}[2J",
                "user.name",
                "Alice\nwarning\r\t\u{7}",
            )],
            &paths(&temp),
            None,
            None,
        );
        let rendered = render_row(&report.findings[0]);
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{7}'));
        assert!(!rendered.contains("Alice\nwarning"));
        assert!(rendered.contains("Alice\\nwarning\\r\\t\\u{0007}"));
        assert!(rendered.contains("file:/tmp/evil\\u{001B}[2J"));
    }

    #[test]
    fn environment_findings_report_names_never_values() {
        let findings = scan_environment(
            [
                "GIT_AUTHOR_EMAIL",
                "GH_TOKEN",
                "GIT_CONFIG_GLOBAL",
                "UNRELATED",
            ]
            .into_iter()
            .map(str::to_owned),
        );
        assert_eq!(findings.len(), 3);
        assert_eq!(
            findings
                .iter()
                .map(|item| item.key.as_str())
                .collect::<Vec<_>>(),
            vec!["GH_TOKEN", "GIT_AUTHOR_EMAIL", "GIT_CONFIG_GLOBAL"]
        );
        assert!(
            findings
                .iter()
                .all(|item| item.value == "<已设置>" && item.source == "env")
        );
        let severity = |key: &str| {
            findings
                .iter()
                .find(|item| item.key == key)
                .expect("finding")
                .severity
        };
        assert_eq!(severity("GIT_AUTHOR_EMAIL"), Severity::Error);
        assert_eq!(severity("GH_TOKEN"), Severity::Warning);
        assert_eq!(severity("GIT_CONFIG_GLOBAL"), Severity::Info);
    }

    #[test]
    fn ssh_differences_name_changed_settings_without_their_values() {
        let mut configured = BTreeMap::new();
        configured.insert(
            "identityfile".to_owned(),
            vec!["/home/test/.ssh/id_work".into()],
        );
        configured.insert(
            "proxycommand".to_owned(),
            vec!["ssh -W %h:%p secret-jump".into()],
        );
        configured.insert("port".to_owned(), vec!["22".into()]);
        let mut defaults = BTreeMap::new();
        defaults.insert(
            "identityfile".to_owned(),
            vec!["/home/test/.ssh/id_ed25519".into()],
        );
        defaults.insert("proxycommand".to_owned(), vec!["none".into()]);
        defaults.insert("port".to_owned(), vec!["22".into()]);

        let differences = ssh_differences(&configured, &defaults);
        assert_eq!(differences, vec!["identityfile", "proxycommand"]);
        assert!(!differences.iter().any(|key| key.contains("secret")));
    }

    #[test]
    fn sanitizes_external_probe_credentials_keys_controls_and_length() {
        let long = "x".repeat(400);
        let raw = format!(
            "\u{1b}[31mAuthorization: Bearer header-secret\u{1b}[0m\nGH_TOKEN=token-secret\nssh-ed25519 AAAAKEY raw-key-comment\nexit 73: {long}\r\u{7}"
        );
        let sanitized = sanitize_external_output(&raw);
        assert!(sanitized.contains("exit 73"));
        for secret in [
            "header-secret",
            "token-secret",
            "AAAAKEY",
            "raw-key-comment",
        ] {
            assert!(!sanitized.contains(secret));
        }
        assert!(!sanitized.contains('\u{1b}'));
        assert!(!sanitized.contains('\r'));
        assert!(!sanitized.contains('\u{7}'));
        assert!(sanitized.chars().count() <= 163);
    }
}
