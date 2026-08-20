//! 应用层用例：把配置、仓库解析和外部命令串起来。

use crate::config::{
    self, Config, ConfigError, ConfigPaths, Profile, ProfileResolution, ResolutionSource,
    RuleContext,
};
use crate::git::{self, EffectiveIdentities, FragmentOptions};
use crate::github;
use crate::process::{CommandRunner, CommandSpec, ProcessError, SystemCommandRunner};
use crate::repo::{self, Remote, RepoError, Repository, Transport};
use crate::signing;
use crate::state::{self, AuditAction, RepositoryBindingRecord};
use fd_lock::RwLock;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

pub const PROFILE_CONFIG_KEY: &str = "ghis.profile";

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("配置错误：{0}")]
    Config(#[from] ConfigError),
    #[error("仓库错误：{0}")]
    Repo(#[from] RepoError),
    #[error("Git 错误：{0}")]
    Git(#[from] git::GitError),
    #[error("GitHub 错误：{0}")]
    Github(#[from] github::GhError),
    #[error("命令错误：{0}")]
    Process(#[from] ProcessError),
    #[error("SSH/签名错误：{0}")]
    Signing(#[from] signing::SigningError),
    #[error("I/O 错误：{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Message(String),
}

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, Clone)]
pub struct AppContext {
    pub paths: ConfigPaths,
    pub config: Config,
    pub repository: Option<Repository>,
    pub remote: Option<Remote>,
    pub resolution: ProfileResolution,
    pub profile: Option<Profile>,
    pub identities: Option<EffectiveIdentities>,
    pub warnings: Vec<String>,
}

impl AppContext {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut paths = ConfigPaths::discover()?;
        if let Some(path) = path {
            paths.set_config_file(path)?;
        }
        let config = Config::load(&paths.config_file)?;
        Self::from_config(paths, config, std::env::current_dir()?, None)
    }

    pub fn from_config(
        paths: ConfigPaths,
        config: Config,
        cwd: impl AsRef<Path>,
        explicit: Option<&str>,
    ) -> Result<Self> {
        Self::from_config_with_target(paths, config, cwd, explicit, None, true)
    }

    /// Resolve the minimal prompt state without inspecting Git identities.
    ///
    /// This deliberately shares the normal resolution chain while avoiding the
    /// extra `git var` subprocess that is useful for detailed status reporting
    /// but unnecessary for a prompt or direnv integration.
    pub fn from_config_for_prompt(
        paths: ConfigPaths,
        config: Config,
        cwd: impl AsRef<Path>,
        explicit: Option<&str>,
    ) -> Result<Self> {
        Self::from_config_with_target(paths, config, cwd, explicit, None, false)
    }

    fn from_config_for_gh(
        paths: ConfigPaths,
        config: Config,
        cwd: impl AsRef<Path>,
        explicit: Option<&str>,
        target: Option<&GhProfileTarget>,
    ) -> Result<Self> {
        Self::from_config_with_target(paths, config, cwd, explicit, target, true)
    }

    fn from_config_with_target(
        paths: ConfigPaths,
        config: Config,
        cwd: impl AsRef<Path>,
        explicit: Option<&str>,
        target: Option<&GhProfileTarget>,
        inspect_identities: bool,
    ) -> Result<Self> {
        let cwd = std::path::absolute(cwd.as_ref())?;
        let mut warnings = Vec::new();
        let (repository, remote, binding, identities) = match repo::discover(&cwd) {
            Ok(repository) => {
                let remote = repo::primary_remote(&repository)?;
                let binding = repo::local_config(&repository, PROFILE_CONFIG_KEY)?;
                let identities = inspect_identities
                    .then(|| git::effective_identities(&repository).ok())
                    .flatten();
                (Some(repository), remote, binding, identities)
            }
            Err(RepoError::NotRepository { .. }) => (None, None, None, None),
            Err(error) => return Err(error.into()),
        };
        let (repository_context, repository_match) = if let Some(remote) = remote.as_ref() {
            (
                RuleContext {
                    host: remote.host.clone(),
                    owner: remote.owner.clone(),
                    repo: remote.repo.clone(),
                    remote: Some(remote.url.clone()),
                    gitdir: repository.as_ref().map(|item| item.git_dir.clone()),
                    cwd: Some(cwd.clone()),
                },
                unique_profile_for_target(&config, remote.host.as_deref(), remote.owner.as_deref()),
            )
        } else {
            (
                RuleContext {
                    gitdir: repository.as_ref().map(|item| item.git_dir.clone()),
                    cwd: Some(cwd.clone()),
                    ..RuleContext::default()
                },
                None,
            )
        };
        let mut context = target
            .map(|target| target.context.clone())
            .unwrap_or(repository_context);
        context.cwd = Some(cwd);
        let github_match = if target.is_some() {
            unique_profile_for_target(&config, context.host.as_deref(), context.owner.as_deref())
        } else {
            repository_match
        };
        let env_explicit = std::env::var("GHIS_PROFILE").ok();
        let explicit = explicit.or(env_explicit.as_deref());
        let resolution = config::resolve_profile(
            &config,
            &context,
            explicit,
            target.is_none().then_some(binding.as_deref()).flatten(),
            github_match.as_deref(),
        );
        warnings.extend(resolution.warnings.clone());
        if matches!(
            resolution.source,
            ResolutionSource::Unresolved
                | ResolutionSource::Ambiguous
                | ResolutionSource::InvalidExplicit
                | ResolutionSource::InvalidRepositoryBinding
        ) && repository.is_some()
        {
            warnings.push(match resolution.source {
                ResolutionSource::Ambiguous => "仓库身份规则存在歧义，未自动选择账号".into(),
                ResolutionSource::InvalidExplicit => {
                    "显式指定的 Profile 不存在，拒绝回退到其他身份".into()
                }
                ResolutionSource::InvalidRepositoryBinding => {
                    "仓库绑定的 Profile 已失效，拒绝回退到其他身份".into()
                }
                _ => "仓库尚未绑定 ghis profile，将继续使用现有 Git 配置".into(),
            });
        }
        let profile = resolution
            .profile
            .as_deref()
            .and_then(|id| config.profiles.get(id).cloned());
        Ok(Self {
            paths,
            config,
            repository,
            remote,
            resolution,
            profile,
            identities,
            warnings,
        })
    }

    pub fn profile_id(&self) -> Option<&str> {
        self.resolution.profile.as_deref()
    }

    pub fn status_summary(&self) -> String {
        match (self.profile_id(), self.profile.as_ref()) {
            (Some(id), Some(profile)) => match profile.description.as_deref() {
                Some(description) => format!("当前 Profile：{id}\n  {description}"),
                None => format!("当前 Profile：{id}"),
            },
            _ => "当前 Profile：未解析".into(),
        }
    }

    /// Keep automatic wrapper output compact. Detailed identity information is
    /// available through `ghis status` and should not accompany every command.
    pub fn operation_banner(&self) -> String {
        match self.profile_id() {
            Some(id) => format!("GHIS Profile: {id}"),
            None => "GHIS Profile: 未解析".into(),
        }
    }

    /// Report Git-resolved identities only when a hook detects a mismatch.
    ///
    /// The wrapper can inspect its own `-c` and `--author` arguments before
    /// launching Git. A hook may also be reached through an absolute-path Git
    /// invocation, so it reports `git var` results instead of assuming that
    /// the Profile fragment won every config/environment override.
    pub fn hook_identity_warning(&self) -> Option<String> {
        let (Some(profile), Some(identities)) = (self.profile.as_ref(), self.identities.as_ref())
        else {
            return None;
        };
        let check = identities.check(git::IdentityExpectation {
            name: &profile.git_name,
            email: &profile.git_email,
        });
        if check.author_matches && check.committer_matches {
            return None;
        }
        Some(format!(
            "ghis: 警告：Git 实际身份与 Profile 不一致；实际作者={} <{}>；实际提交者={} <{}>",
            identities.author.name,
            identities.author.email,
            identities.committer.name,
            identities.committer.email
        ))
    }

    /// Refuse an explicit or stored selection that points at a deleted Profile.
    /// Falling through to a rule/default here could use a different account.
    pub fn ensure_selection_available(&self) -> Result<()> {
        match self.resolution.source {
            ResolutionSource::InvalidExplicit => Err(AppError::Message(
                "显式指定的 Profile 不存在；为防止用错身份，已停止操作".into(),
            )),
            ResolutionSource::InvalidRepositoryBinding => Err(AppError::Message(
                "仓库绑定的 Profile 已不存在；请先运行 `ghis unbind` 或重新绑定".into(),
            )),
            _ => Ok(()),
        }
    }
}

fn unique_profile_for_target(
    config: &Config,
    host: Option<&str>,
    owner: Option<&str>,
) -> Option<String> {
    let host = host?;
    let owner = owner?;
    let mut matches = config
        .profiles
        .iter()
        .filter(|(_, profile)| {
            github::normalize_host(&profile.host) == github::normalize_host(host)
                && profile.login.eq_ignore_ascii_case(owner)
        })
        .map(|(id, _)| id.clone());
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

pub fn profile_fragment_path(paths: &ConfigPaths, profile_id: &str) -> PathBuf {
    let mut safe = String::with_capacity(profile_id.len());
    for byte in profile_id.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            safe.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut safe, "%{byte:02X}").expect("writing to a string cannot fail");
        }
    }
    paths.fragments_dir.join(format!("{safe}.gitconfig"))
}

fn managed_ssh_config_path(paths: &ConfigPaths, profile_id: &str) -> PathBuf {
    profile_fragment_path(paths, profile_id).with_extension("sshconfig")
}

pub fn write_profile_fragment(paths: &ConfigPaths, id: &str, profile: &Profile) -> Result<PathBuf> {
    with_fragment_write_lock(paths, || {
        let authoritative = authoritative_config(paths, None)?;
        let profile = match authoritative.as_ref() {
            Some(config) => {
                let current = config.profiles.get(id).ok_or_else(|| {
                    AppError::Message(format!(
                        "Profile `{id}` 已被删除；配置在操作期间发生变化，请重试"
                    ))
                })?;
                if current != profile {
                    return Err(AppError::Message(format!(
                        "Profile `{id}` 已被其他进程修改；请重试当前操作"
                    )));
                }
                current
            }
            None => profile,
        };
        write_profile_fragment_unlocked(paths, id, profile)
    })
}

fn write_profile_fragment_unlocked(
    paths: &ConfigPaths,
    id: &str,
    profile: &Profile,
) -> Result<PathBuf> {
    let path = profile_fragment_path(paths, id);
    let ssh_config = write_managed_ssh_config_unlocked(paths, id, profile)?;
    let content = profile_fragment_content(profile, ssh_config.as_deref());
    let temporary = path.with_extension(format!("gitconfig.{}.tmp", std::process::id()));
    fs::write(&temporary, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&temporary, &path)?;
    Ok(path)
}

fn write_managed_ssh_config_unlocked(
    paths: &ConfigPaths,
    id: &str,
    profile: &Profile,
) -> Result<Option<PathBuf>> {
    let path = managed_ssh_config_path(paths, id);
    let managed = profile.ssh.as_ref().filter(|ssh| {
        matches!(
            ssh.mode,
            config::SshMode::OnePassword | config::SshMode::Managed
        )
    });
    let material = managed.and_then(|ssh| {
        let socket = signing::discover_agent_socket(ssh.agent_socket.as_deref())?;
        let public_key = ssh.public_key.as_deref().map(signing::expand_user)?;
        Some((socket, public_key))
    });
    let Some((socket, public_key)) = material else {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        return Ok(None);
    };

    let temporary = path.with_extension(format!("sshconfig.{}.tmp", std::process::id()));
    let user_known_hosts = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .map(|home| home.join(".ssh").join("known_hosts"));
    fs::write(
        &temporary,
        signing::render_managed_ssh_config(&socket, &public_key, user_known_hosts.as_deref()),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&temporary, &path)?;
    Ok(Some(path))
}

fn write_managed_ssh_config(
    paths: &ConfigPaths,
    id: &str,
    profile: &Profile,
) -> Result<Option<PathBuf>> {
    with_fragment_write_lock(paths, || {
        write_managed_ssh_config_unlocked(paths, id, profile)
    })
}

fn with_fragment_write_lock<T>(
    paths: &ConfigPaths,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    paths.create_dirs()?;
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(paths.fragments_dir.join(".lock"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        lock_file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let mut lock = RwLock::new(lock_file);
    let _guard = lock.write()?;
    operation()
}

fn authoritative_config(paths: &ConfigPaths, fallback: Option<&Config>) -> Result<Option<Config>> {
    match fs::symlink_metadata(&paths.config_file) {
        Ok(_) => {
            // A dangling symlink is an invalid configured source, not a
            // signal to recreate fragments from an in-memory fallback.
            fs::metadata(&paths.config_file)?;
            Ok(Some(Config::load(&paths.config_file)?))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(fallback.cloned()),
        Err(error) => Err(error.into()),
    }
}

fn profile_fragment_content(profile: &Profile, ssh_config_path: Option<&Path>) -> String {
    let signing = &profile.signing;
    // The command may target a non-primary remote or an `insteadOf` rewrite,
    // so install the managed SSH restriction independently of `ctx.remote`.
    let managed_ssh = profile.ssh.as_ref().filter(|ssh| {
        matches!(
            ssh.mode,
            config::SshMode::OnePassword | config::SshMode::Managed
        )
    });
    let signing_key = signing
        .enabled
        .then(|| {
            signing.signing_key.clone().or_else(|| {
                if signing.fingerprint.is_some() {
                    None
                } else {
                    profile
                        .ssh
                        .as_ref()
                        .and_then(|ssh| ssh.public_key.as_ref())
                        .map(|path| path.to_string_lossy().into_owned())
                }
            })
        })
        .flatten();
    let signing_program = signing
        .enabled
        .then(|| {
            signing
                .program
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .or_else(|| {
                    matches!(signing.transport, config::SigningTransport::LocalAgent)
                        .then(|| {
                            signing::discover_signing_program(None)
                                .map(|program| program.path.to_string_lossy().into_owned())
                        })
                        .flatten()
                })
        })
        .flatten();
    let options = FragmentOptions {
        name: profile.git_name.clone(),
        email: profile.git_email.clone(),
        credential_helper: None,
        ssh_command: managed_ssh.map(|ssh| {
            ssh_config_path.map_or_else(ssh_guard_command, |config_path| {
                signing::render_ssh_command(config_path, &ssh.proxy_jump, ssh.forward_agent)
            })
        }),
        signing_key,
        signing_program,
        commit_gpg_sign: signing.enabled,
    };
    git::render_profile_fragment(&options)
}

/// 将 profile 绑定到仓库，并同步其 include/helper 设置。
pub fn bind_repository(ctx: &AppContext, id: &str) -> Result<()> {
    let repository = ctx
        .repository
        .as_ref()
        .ok_or_else(|| AppError::Message("当前目录不是 Git 仓库".into()))?;
    let profile = ctx
        .config
        .profiles
        .get(id)
        .ok_or_else(|| AppError::Message(format!("profile `{id}` 不存在")))?;
    let fragment = write_profile_fragment(&ctx.paths, id, profile)?;
    remove_managed_credential_helpers(repository)?;
    git::set_git_config(repository, PROFILE_CONFIG_KEY, id)?;
    let include_key = profile_include_key(repository);
    for old_key in profile_include_cleanup_keys(repository) {
        unset_profile_include(repository, &old_key)?;
    }
    repo::add_local_config(
        repository,
        &include_key,
        fragment.to_string_lossy().as_ref(),
    )?;
    let helper = credential_helper_command(&ctx.paths);
    git::install_credential_helper(repository, &profile.host, &helper)?;
    if git::supports_named_hooks() {
        let binary = current_binary();
        git::install_named_hooks_with_config(repository, &binary, Some(&ctx.paths.config_file))?;
    }
    remember_repository(ctx, id, AuditAction::Bind);
    Ok(())
}

pub fn unbind_repository(ctx: &AppContext) -> Result<()> {
    let repository = ctx
        .repository
        .as_ref()
        .ok_or_else(|| AppError::Message("当前目录不是 Git 仓库".into()))?;
    git::unset_git_config(repository, PROFILE_CONFIG_KEY)?;
    for include_key in profile_include_cleanup_keys(repository) {
        unset_profile_include(repository, &include_key)?;
    }
    remove_managed_credential_helpers(repository)?;
    if git::supports_named_hooks() {
        let _ = git::remove_named_hooks(repository);
    }
    forget_repository(ctx, AuditAction::Unbind);
    Ok(())
}

fn remember_repository(ctx: &AppContext, profile: &str, action: AuditAction) {
    register_repository(ctx, profile);
    let Some(repository) = ctx.repository.as_ref() else {
        return;
    };
    if let Err(error) = state::append_audit_event(
        &ctx.paths.log_file,
        action,
        repository.command_dir(),
        Some(profile),
    ) {
        eprintln!("ghis: 无法写入脱敏状态日志：{error}");
    }
}

fn register_repository(ctx: &AppContext, profile: &str) {
    let Some(repository) = ctx.repository.as_ref() else {
        return;
    };
    let record = RepositoryBindingRecord {
        config_file: ctx.paths.config_file.clone(),
        repository_path: repository.command_dir().to_path_buf(),
        git_dir: repository.git_dir.clone(),
        profile: profile.to_owned(),
    };
    if let Err(error) = state::register(&ctx.paths.repositories_file, record) {
        eprintln!("ghis: 无法更新仓库状态索引：{error}");
    }
}

fn forget_repository(ctx: &AppContext, action: AuditAction) {
    let Some(repository) = ctx.repository.as_ref() else {
        return;
    };
    if let Err(error) = state::unregister(
        &ctx.paths.repositories_file,
        &ctx.paths.config_file,
        &repository.git_dir,
    ) {
        eprintln!("ghis: 无法更新仓库状态索引：{error}");
    }
    if let Err(error) = state::append_audit_event(
        &ctx.paths.log_file,
        action,
        repository.command_dir(),
        ctx.profile_id(),
    ) {
        eprintln!("ghis: 无法写入脱敏状态日志：{error}");
    }
}

fn unset_profile_include(repository: &Repository, key: &str) -> Result<()> {
    repo::unset_local_config(repository, key)?;
    if !repository.bare {
        // Before worktreeConfig support, ghis wrote this gitdir-specific key
        // into the shared config. Remove that legacy value as part of bind and
        // unbind so an old fragment cannot become active again later.
        repo::unset_common_config(repository, key)?;
    }
    Ok(())
}

fn profile_include_key(repository: &Repository) -> String {
    let gitdir = fs::canonicalize(&repository.git_dir)
        .unwrap_or_else(|_| repository.git_dir.clone())
        .to_string_lossy()
        .replace('\\', "/");
    format!("includeIf.gitdir:{gitdir}.path")
}

fn profile_include_cleanup_keys(repository: &Repository) -> Vec<String> {
    let gitdir = repository.git_dir.to_string_lossy().replace('\\', "/");
    let mut keys = vec![profile_include_key(repository)];
    for key in [
        format!("includeIf.gitdir:{gitdir}.path"),
        format!("includeIf.gitdir:{gitdir}/.path"),
    ] {
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

pub fn sync_fragments(paths: &ConfigPaths, config: &Config) -> Result<Vec<PathBuf>> {
    with_fragment_write_lock(paths, || {
        let config = authoritative_config(paths, Some(config))?
            .expect("a fallback configuration is always available");
        let mut paths_written = Vec::new();
        for (id, profile) in &config.profiles {
            paths_written.push(write_profile_fragment_unlocked(paths, id, profile)?);
        }
        let current = paths_written.iter().cloned().collect::<BTreeSet<_>>();
        for entry in fs::read_dir(&paths.fragments_dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if path.extension().and_then(|value| value.to_str()) == Some("gitconfig")
                && (file_type.is_file() || file_type.is_symlink())
                && !current.contains(&path)
            {
                fs::remove_file(path)?;
            }
        }
        Ok(paths_written)
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositorySyncReport {
    pub checked: usize,
    pub repaired: usize,
    pub forgotten: usize,
    pub warnings: Vec<String>,
}

/// Check every repository remembered for this config and repair its generated
/// fragment, credential helper, and named hooks. The repository's own binding
/// remains authoritative; stale state records are removed instead of rebound.
pub fn sync_registered_repositories(paths: &ConfigPaths, config: &Config) -> RepositorySyncReport {
    let mut report = RepositorySyncReport::default();
    let records = match state::repositories_for_config(&paths.repositories_file, &paths.config_file)
    {
        Ok(records) => records,
        Err(error) => {
            report
                .warnings
                .push(format!("无法读取仓库状态索引：{error}"));
            return report;
        }
    };

    for record in records {
        report.checked += 1;
        let repository = match repo::discover(&record.repository_path) {
            Ok(repository) => repository,
            Err(error) => {
                report.warnings.push(format!(
                    "无法检查仓库 {}：{error}",
                    record.repository_path.display()
                ));
                continue;
            }
        };
        if repository.git_dir != record.git_dir {
            report.warnings.push(format!(
                "{} 已指向另一个 Git 工作树，已忘记旧记录",
                record.repository_path.display()
            ));
            forget_registry_record(paths, &record, &mut report);
            continue;
        }
        let binding = match repo::local_config(&repository, PROFILE_CONFIG_KEY) {
            Ok(Some(binding)) => binding,
            Ok(None) => {
                forget_registry_record(paths, &record, &mut report);
                continue;
            }
            Err(error) => {
                report.warnings.push(format!(
                    "无法读取 {} 的绑定：{error}",
                    record.repository_path.display()
                ));
                continue;
            }
        };
        if !config.profiles.contains_key(&binding) {
            report.warnings.push(format!(
                "{} 绑定了已不存在的 Profile `{binding}`",
                record.repository_path.display()
            ));
            continue;
        }
        let ctx = match AppContext::from_config(
            paths.clone(),
            config.clone(),
            repository.command_dir(),
            Some(&binding),
        ) {
            Ok(ctx) => ctx,
            Err(error) => {
                report.warnings.push(format!(
                    "无法解析 {}：{error}",
                    record.repository_path.display()
                ));
                continue;
            }
        };
        match repository_binding_needs_repair(&ctx) {
            Ok(true) => match bind_repository(&ctx, &binding) {
                Ok(()) => {
                    report.repaired += 1;
                    if let Err(error) = state::append_audit_event(
                        &paths.log_file,
                        AuditAction::SyncRepair,
                        repository.command_dir(),
                        Some(&binding),
                    ) {
                        report
                            .warnings
                            .push(format!("无法写入脱敏状态日志：{error}"));
                    }
                }
                Err(error) => report.warnings.push(format!(
                    "修复 {} 失败：{error}",
                    record.repository_path.display()
                )),
            },
            Ok(false) => {
                remember_repository(&ctx, &binding, AuditAction::SyncCheck);
            }
            Err(error) => report.warnings.push(format!(
                "检查 {} 失败：{error}",
                record.repository_path.display()
            )),
        }
    }
    report
}

fn forget_registry_record(
    paths: &ConfigPaths,
    record: &RepositoryBindingRecord,
    report: &mut RepositorySyncReport,
) {
    match state::unregister(
        &paths.repositories_file,
        &paths.config_file,
        &record.git_dir,
    ) {
        Ok(true) => report.forgotten += 1,
        Ok(false) => {}
        Err(error) => report
            .warnings
            .push(format!("无法清理仓库状态记录：{error}")),
    }
}

pub fn display_status(ctx: &AppContext) -> StatusReport {
    StatusReport::from_context(ctx)
}

/// Versioned, credential-free resolution state for prompts and direnv.
///
/// This intentionally exposes only the active profile and why it was chosen.
/// It is safe to consume from a non-interactive shell without loading a
/// wrapper, inspecting credentials, or creating ghis state.
#[derive(Debug, Clone, Serialize)]
pub struct PromptStatusReport {
    pub schema_version: u32,
    pub profile: Option<String>,
    pub resolution_source: String,
}

impl PromptStatusReport {
    pub fn from_context(ctx: &AppContext) -> Self {
        Self {
            schema_version: 1,
            profile: ctx.profile_id().map(str::to_owned),
            resolution_source: resolution_source_name(&ctx.resolution.source),
        }
    }

    pub fn is_resolved(&self) -> bool {
        self.profile.is_some()
    }
}

pub fn prompt_status(ctx: &AppContext) -> PromptStatusReport {
    PromptStatusReport::from_context(ctx)
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub schema_version: u32,
    pub repository: Option<String>,
    pub profile: Option<String>,
    pub resolution_source: String,
    pub author: Option<IdentityReport>,
    pub committer: Option<IdentityReport>,
    pub github: Option<GithubReport>,
    pub transport: Option<String>,
    pub signing: Option<SigningReport>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IdentityReport {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GithubReport {
    pub host: String,
    pub login: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SigningReport {
    pub enabled: bool,
    pub transport: String,
    pub key: Option<String>,
}

impl StatusReport {
    fn from_context(ctx: &AppContext) -> Self {
        let repository = ctx.repository.as_ref().map(|item| {
            item.root
                .as_deref()
                .unwrap_or(&item.path)
                .display()
                .to_string()
        });
        let (author, committer) = ctx
            .identities
            .as_ref()
            .map(|identities| {
                (
                    Some(IdentityReport {
                        name: identities.author.name.clone(),
                        email: identities.author.email.clone(),
                    }),
                    Some(IdentityReport {
                        name: identities.committer.name.clone(),
                        email: identities.committer.email.clone(),
                    }),
                )
            })
            .unwrap_or((None, None));
        let github = ctx.profile.as_ref().map(|profile| GithubReport {
            host: profile.host.clone(),
            login: profile.login.clone(),
        });
        let signing = ctx.profile.as_ref().map(|profile| SigningReport {
            enabled: profile.signing.enabled,
            transport: match profile.signing.transport {
                config::SigningTransport::LocalAgent => "local-agent",
                config::SigningTransport::ForwardedAgent => "forwarded-agent",
            }
            .into(),
            key: status_signing_key_selector(profile),
        });
        Self {
            schema_version: crate::SCHEMA_VERSION,
            repository,
            profile: ctx.profile_id().map(str::to_owned),
            resolution_source: resolution_source_name(&ctx.resolution.source),
            author,
            committer,
            github,
            transport: ctx
                .remote
                .as_ref()
                .map(|remote| remote.transport.as_str().to_owned()),
            signing,
            warnings: ctx.warnings.clone(),
        }
    }
}

fn status_signing_key_selector(profile: &Profile) -> Option<String> {
    if let Some(fingerprint) = profile.signing.fingerprint.as_deref() {
        return Some(format!(
            "fingerprint:{}",
            crate::diagnostics::sanitize_display_text(fingerprint.trim())
        ));
    }
    let key = profile.signing.signing_key.as_deref()?;
    let inline = key.trim_start().strip_prefix("key::").unwrap_or(key);
    if signing::is_public_key_line(inline) {
        let key_type = inline.split_whitespace().next().unwrap_or("unknown");
        return Some(format!("inline:{key_type}"));
    }
    Some("<签名公钥路径已隐藏>".into())
}

fn resolution_source_name(source: &ResolutionSource) -> String {
    match source {
        ResolutionSource::Explicit => "explicit",
        ResolutionSource::InvalidExplicit => "invalid-explicit",
        ResolutionSource::RepositoryBinding => "repository-binding",
        ResolutionSource::InvalidRepositoryBinding => "invalid-repository-binding",
        ResolutionSource::Rule { .. } => "rule",
        ResolutionSource::GithubLogin => "github-login",
        ResolutionSource::Default => "default",
        ResolutionSource::Unresolved => "unresolved",
        ResolutionSource::Ambiguous => "ambiguous",
    }
    .into()
}

/// 执行一次经过身份解析的 git 命令。
pub fn run_git<A>(
    args: &[A],
    config_path: Option<&Path>,
    explicit: Option<&str>,
    cwd: &Path,
) -> Result<i32>
where
    A: AsRef<std::ffi::OsStr>,
{
    let forwarded_args = args
        .iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect::<Vec<_>>();
    // The CLI wrapper invokes this command as `ghis git -- "$@"` so Clap can
    // preserve arbitrary Git options. Remove that transport separator before
    // handing arguments to Git; otherwise `ghis git -- --version` turns the
    // version flag into a subcommand and Git reports a misleading `-c` error.
    let forwarded_args = forwarded_args
        .strip_prefix(&[std::ffi::OsString::from("--")])
        .unwrap_or(&forwarded_args);
    let argument_view = forwarded_args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let args = argument_view.as_slice();
    // A small, closed whitelist lets ordinary local inspection keep Git's own
    // argv, cwd and stdio untouched. Keep this before ConfigPaths::discover:
    // the wrapper must not create any ghis config/cache/state artifacts for a
    // command that can be proven not to need profile resolution.
    if is_local_read_only_git(args) {
        let command = CommandSpec::git().args(forwarded_args.iter().cloned());
        let status = SystemCommandRunner::new().run_passthrough(&command)?;
        return Ok(status.code().unwrap_or(128));
    }

    let mut paths = ConfigPaths::discover()?;
    if let Some(path) = config_path {
        paths.set_config_file(path)?;
    }
    let config = Config::load(&paths.config_file)?;
    let ctx = AppContext::from_config(paths, config, cwd, explicit)?;
    ctx.ensure_selection_available()?;
    let operation = resolve_git_operation(&ctx, args, cwd);
    if ctx.profile.is_none()
        && ctx.config.behavior.unresolved == config::UnresolvedPolicy::Fail
        && operation.is_sensitive()
    {
        return Err(AppError::Message(
            "当前仓库身份无法唯一解析，已按 unresolved=fail 停止操作".into(),
        ));
    }
    validate_commit_signing_argv(&ctx, args, &operation)?;
    validate_git_auth_safety(&ctx, args, &operation)?;
    let execution_policy = validate_profile_operation(&ctx, &operation)?;
    let repository_binding = matches!(ctx.resolution.source, ResolutionSource::RepositoryBinding);
    let binding_needs_repair = repository_binding && repository_binding_needs_repair(&ctx)?;
    let should_auto_bind = ctx.repository.is_some()
        && ctx.config.behavior.auto_bind
        && matches!(ctx.resolution.source, ResolutionSource::Rule { .. });
    if should_auto_bind || binding_needs_repair {
        if let Some(id) = ctx.profile_id() {
            bind_repository(&ctx, id)?;
        }
    } else if repository_binding && let Some(id) = ctx.profile_id() {
        // Migrate bindings created by older ghis versions into the disposable
        // state index without rewriting an unchanged registry on every call.
        register_repository(&ctx, id);
    } else if !ctx.warnings.is_empty() {
        eprintln!("ghis: {}", ctx.warnings.join("；"));
    }
    let banner =
        should_display_identity_banner(&ctx, operation.is_sensitive(), io::stderr().is_terminal());
    let identity_override = identity_override_warning(&ctx, args, &operation);
    if banner {
        eprintln!("{}", ctx.operation_banner());
    }
    if let Some(warning) = identity_override.as_deref() {
        eprintln!("{warning}");
    }
    // Keep the caller's working directory for the real Git invocation.  In
    // particular, forwarding `git -C relative/path` after changing into that
    // path would make Git apply the relative path a second time.
    let policy_index = git_policy_insertion_index(args);
    let mut command = CommandSpec::git();
    if let (Some(id), Some(profile)) = (ctx.profile_id(), ctx.profile.as_ref())
        && !repository_binding
        && !should_auto_bind
    {
        let fragment = write_profile_fragment(&ctx.paths, id, profile)?;
        command = command
            .arg("-c")
            .arg(format!("include.path={}", fragment.display()))
            .env("GHIS_PROFILE", id);
    }
    // Keep identity fragments before caller options so an explicit
    // `-c user.name=...` still works (and is reported by the banner). Put the
    // credential policy after caller global options, but before Git's `--`
    // option terminator/subcommand. Appending `-c` after `--` makes Git treat it
    // as a command (notably for `git --version` and `ghis git -- --version`).
    command = command.args(forwarded_args[..policy_index].iter().cloned());
    if let Some(signing_key) = execution_policy.signing_key.as_deref() {
        command = command
            .arg("-c")
            .arg(format!("user.signingKey={signing_key}"));
    }
    if execution_policy.force_commit_signing {
        // Keep this after caller-supplied global options. The argv validator
        // also rejects unsafe overrides embedded in aliases, where insertion
        // order cannot reliably restore the Profile policy.
        command = command.arg("-c").arg("commit.gpgSign=true");
    }
    if let Some(profile) = ctx.profile.as_ref() {
        // Restrict the fail-closed helper to this Profile's GitHub host. Other
        // HTTPS services and cross-host submodules keep their own helper chain.
        let helper = credential_helper_command(&ctx.paths);
        let helper_key = git::credential_helper_key(&profile.host);
        command = command
            .arg("-c")
            .arg(format!("{helper_key}="))
            .arg("-c")
            .arg(format!("{helper_key}={helper}"));
    }
    if config_path.is_some() {
        // Pass the resolved absolute path so hooks and nested wrappers keep
        // the same custom cache/state namespace after Git changes directory.
        command = command.env("GHIS_CONFIG", ctx.paths.config_file.as_os_str());
    }
    command = command.args(forwarded_args[policy_index..].iter().cloned());
    if let Some(ssh_command) = execution_policy.ssh_command {
        command = apply_managed_ssh_environment(command, ssh_command);
    }
    if banner || identity_override.is_some() {
        command = command.env("GHIS_BANNER_SHOWN", "1");
    }
    let status = SystemCommandRunner::new().run_passthrough(&command)?;
    Ok(status.code().unwrap_or(128))
}

#[derive(Debug, Default)]
struct GitExecutionPolicy {
    ssh_command: Option<String>,
    signing_key: Option<String>,
    force_commit_signing: bool,
}

/// Return true only for Git invocations whose command and relevant options are
/// in the local-inspection allowlist. Unknown commands and aliases deliberately
/// return false: resolving an alias would require consulting Git config, and a
/// shell alias can run arbitrary commands or contact a remote.
fn is_local_read_only_git(args: &[String]) -> bool {
    if args.is_empty()
        || args.iter().any(|arg| {
            arg == "-c"
                || arg.starts_with("-c")
                || arg == "--config-env"
                || arg.starts_with("--config-env=")
        })
    {
        return false;
    }

    let Some(index) = git_subcommand_index(args) else {
        return matches!(args, [flag] if matches!(flag.as_str(), "--version" | "--help" | "-h"));
    };
    let command = args[index].as_str();
    let command_args = &args[index + 1..];
    match command {
        "branch" => is_read_only_branch(command_args),
        "config" => is_read_only_config(command_args),
        "remote" => is_read_only_remote(command_args),
        "reflog" => is_read_only_reflog(command_args),
        "stash" => is_read_only_stash(command_args),
        "tag" => is_read_only_tag(command_args),
        "worktree" => command_args.first().is_some_and(|arg| arg == "list"),
        "sparse-checkout" => command_args.first().is_some_and(|arg| arg == "list"),
        "status" | "log" | "diff" | "show" | "rev-parse" | "rev-list" | "describe" | "ls-files"
        | "ls-tree" | "cat-file" | "for-each-ref" | "for-each-reflog" | "name-rev" | "shortlog"
        | "blame" | "grep" | "check-ignore" | "check-attr" | "verify-commit" | "verify-tag"
        | "whatchanged" => true,
        _ => false,
    }
}

fn is_read_only_branch(args: &[String]) -> bool {
    if args.is_empty() {
        return true;
    }
    if args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-d" | "-D"
                | "-m"
                | "-M"
                | "-c"
                | "-C"
                | "--delete"
                | "--move"
                | "--copy"
                | "--set-upstream-to"
                | "-u"
                | "--unset-upstream"
                | "--edit-description"
                | "--create-reflog"
                | "--track"
                | "--no-track"
        ) || arg.starts_with("--delete=")
            || arg.starts_with("--move=")
            || arg.starts_with("--copy=")
            || arg.starts_with("--set-upstream-to=")
    }) {
        return false;
    }
    // Require an explicit listing selector when options are present. This
    // intentionally declines harmless but harder-to-parse forms rather than
    // risking that a branch creation option is treated as a read.
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--list" | "-l" | "-a" | "--all" | "-r" | "--remotes"
        )
    })
}

fn is_read_only_config(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--get"
                | "--get-all"
                | "--get-regexp"
                | "--get-urlmatch"
                | "--list"
                | "-l"
                | "--name-only"
                | "--show-origin"
                | "--show-scope"
        )
    })
}

fn is_read_only_remote(args: &[String]) -> bool {
    args.is_empty()
        || args
            .iter()
            .all(|arg| matches!(arg.as_str(), "-v" | "--verbose" | "get-url"))
}

fn is_read_only_reflog(args: &[String]) -> bool {
    args.first().is_some_and(|arg| arg == "show")
}

fn is_read_only_stash(args: &[String]) -> bool {
    args.first()
        .is_some_and(|arg| matches!(arg.as_str(), "list" | "show"))
}

fn is_read_only_tag(args: &[String]) -> bool {
    args.is_empty()
        || args
            .first()
            .is_some_and(|arg| matches!(arg.as_str(), "--list" | "-l"))
}

#[derive(Debug)]
struct ManagedSshMaterial {
    socket: Option<PathBuf>,
    public_key_path: PathBuf,
    command: String,
}

fn managed_ssh_material(
    paths: &ConfigPaths,
    profile_id: &str,
    profile: &Profile,
) -> Result<Option<ManagedSshMaterial>> {
    let Some(ssh) = profile.ssh.as_ref().filter(|ssh| {
        matches!(
            ssh.mode,
            config::SshMode::OnePassword | config::SshMode::Managed
        )
    }) else {
        return Ok(None);
    };
    let public_key_path = ssh
        .public_key
        .as_deref()
        .map(signing::expand_user)
        .ok_or_else(|| AppError::Message("当前 Profile 已纳管 SSH，但未配置公钥文件".into()))?;
    let socket = signing::discover_agent_socket(ssh.agent_socket.as_deref());
    let command = match socket.as_deref() {
        Some(_) => {
            let config_path = write_managed_ssh_config(paths, profile_id, profile)?
                .ok_or_else(|| AppError::Message("无法生成 managed SSH 配置".into()))?;
            signing::render_ssh_command(&config_path, &ssh.proxy_jump, ssh.forward_agent)
        }
        None => ssh_guard_command(),
    };
    Ok(Some(ManagedSshMaterial {
        socket,
        public_key_path,
        command,
    }))
}

fn ssh_guard_command() -> String {
    format!("{} ssh-guard", git::shell_quote(&current_binary()))
}

fn apply_managed_ssh_environment(
    command: CommandSpec,
    ssh_command: impl Into<std::ffi::OsString>,
) -> CommandSpec {
    command
        .remove_env("GIT_SSH")
        .remove_env("GIT_SSH_COMMAND")
        .remove_env("GIT_SSH_VARIANT")
        .env("GIT_SSH_COMMAND", ssh_command)
        .env("GIT_SSH_VARIANT", "ssh")
}

fn profile_manages_ssh(profile: &Profile) -> bool {
    profile.ssh.as_ref().is_some_and(|ssh| {
        matches!(
            ssh.mode,
            config::SshMode::OnePassword | config::SshMode::Managed
        )
    })
}

fn managed_ssh_environment_command(
    profile: &Profile,
    material: Option<&ManagedSshMaterial>,
) -> Option<String> {
    material
        .map(|material| material.command.clone())
        .or_else(|| {
            // Local commands never execute this value. If an unclassified Git or
            // gh operation unexpectedly starts SSH, it fails instead of inheriting
            // another Agent identity from the caller.
            profile_manages_ssh(profile).then(ssh_guard_command)
        })
}

fn validate_profile_operation(
    ctx: &AppContext,
    operation: &GitOperation,
) -> Result<GitExecutionPolicy> {
    let mut policy = GitExecutionPolicy::default();
    let Some(profile) = ctx.profile.as_ref() else {
        return Ok(policy);
    };
    if operation.name == "commit" && profile.signing.enabled {
        policy.force_commit_signing = true;
        let status = inspect_profile_signing(profile)?;
        if !status.warnings.is_empty() {
            return Err(AppError::Message(format!(
                "签名检查失败：{}",
                status.warnings.join("；")
            )));
        }
        if profile.signing.signing_key.is_none() && profile.signing.fingerprint.is_some() {
            let selected = status
                .selected_key
                .ok_or_else(|| AppError::Message("签名检查未返回严格匹配的 Agent 公钥".into()))?;
            let material = signing::public_key_material(&selected.public_key)
                .ok_or_else(|| AppError::Message("Agent 返回的签名公钥格式无效".into()))?;
            policy.signing_key = Some(signing::git_signing_key_value(&material));
        }
    }

    let may_use_remote = operation.may_contact_remote();
    let profile_id = ctx
        .profile_id()
        .ok_or_else(|| AppError::Message("未解析 Profile，无法准备 managed SSH".into()))?;
    let managed_material = managed_ssh_material(&ctx.paths, profile_id, profile)?;
    policy.ssh_command = managed_ssh_environment_command(profile, managed_material.as_ref())
        .or_else(|| {
            (ctx.config.behavior.ssh_unmanaged == config::SshUnmanagedPolicy::Fail)
                .then(ssh_guard_command)
        });

    let remote_target = if operation.requires_remote_resolution() {
        ctx.repository
            .as_ref()
            .map(|repository| {
                repo::operation_remote(repository, &operation.name, &operation.arguments)
            })
            .transpose()?
    } else {
        None
    };
    let target_may_use_ssh = remote_target.as_ref().is_some_and(|target| {
        target.is_uncertain()
            || target
                .remotes()
                .iter()
                .any(|remote| remote.transport == Transport::Ssh)
    });
    if may_use_remote && target_may_use_ssh {
        match profile.ssh.as_ref() {
            None
            | Some(config::SshProfile {
                mode: config::SshMode::External,
                ..
            }) => match ctx.config.behavior.ssh_unmanaged {
                config::SshUnmanagedPolicy::WarnAndContinue => {
                    if remote_target
                        .as_ref()
                        .is_some_and(repo::OperationRemote::is_uncertain)
                    {
                        eprintln!("ghis: 无法确定 Git 实际 SSH remote，保留系统 SSH 配置");
                    } else {
                        eprintln!("ghis: SSH remote 由系统 SSH 配置管理，未限制 Agent key");
                    }
                }
                config::SshUnmanagedPolicy::Fail => {
                    return Err(AppError::Message(
                        "当前 Profile 未纳管 SSH key，或无法确定实际 SSH remote；已阻止 SSH 操作"
                            .into(),
                    ));
                }
            },
            Some(_) => {
                let material = managed_material.as_ref().ok_or_else(|| {
                    AppError::Message("当前 Profile 已纳管 SSH，但无法生成受限的 SSH 命令".into())
                })?;
                let socket = material.socket.as_ref().ok_or_else(|| {
                    AppError::Message("找不到当前 Profile 的 SSH Agent socket".into())
                })?;
                let public_key =
                    fs::read_to_string(&material.public_key_path).map_err(|error| {
                        AppError::Message(format!(
                            "无法读取 SSH 公钥 {}：{error}",
                            material.public_key_path.display()
                        ))
                    })?;
                let agent = signing::inspect_agent(socket)?;
                if !agent.available {
                    return Err(AppError::Message(format!(
                        "SSH Agent 不可用：{}",
                        agent.error.as_deref().unwrap_or("未知错误")
                    )));
                }
                let selector = signing::SigningProfile {
                    public_key: Some(public_key),
                    fingerprint: profile.ssh.as_ref().and_then(|ssh| ssh.fingerprint.clone()),
                    ..signing::SigningProfile::default()
                };
                if signing::select_key(&agent.keys, &selector).is_none() {
                    return Err(AppError::Message(
                        "SSH Agent 中找不到当前 Profile 的公钥，已停止操作".into(),
                    ));
                }
            }
        }
    }
    Ok(policy)
}

fn profile_public_key(profile: &Profile) -> Result<Option<String>> {
    if let Some(value) = profile.signing.signing_key.as_deref() {
        let inline = value.trim_start().strip_prefix("key::").unwrap_or(value);
        if signing::is_public_key_line(inline) {
            return Ok(Some(inline.to_owned()));
        }
        let path = signing::expand_user(Path::new(value));
        return fs::read_to_string(&path).map(Some).map_err(|error| {
            AppError::Message(format!("无法读取签名公钥 {}：{error}", path.display()))
        });
    }
    if profile.signing.fingerprint.is_some() {
        return Ok(None);
    }
    let Some(path) = profile
        .ssh
        .as_ref()
        .and_then(|ssh| ssh.public_key.as_deref())
    else {
        return Ok(None);
    };
    let path = signing::expand_user(path);
    fs::read_to_string(&path)
        .map(Some)
        .map_err(|error| AppError::Message(format!("无法读取签名公钥 {}：{error}", path.display())))
}

pub fn inspect_profile_signing(profile: &Profile) -> Result<signing::SigningStatus> {
    let public_key = profile_public_key(profile)?;
    Ok(signing::inspect(&signing::SigningProfile {
        enabled: true,
        transport: profile.signing.transport,
        agent_socket: matches!(
            profile.signing.transport,
            config::SigningTransport::LocalAgent
        )
        .then(|| {
            profile
                .ssh
                .as_ref()
                .and_then(|ssh| ssh.agent_socket.clone())
        })
        .flatten(),
        public_key,
        fingerprint: profile.signing.fingerprint.clone(),
        signing_program: profile.signing.program.clone(),
    }))
}

/// Return the commit-signing selector, preserving the legacy authentication
/// fingerprint fallback only when signing has no independent selector.
pub fn profile_signing_fingerprint(profile: &Profile) -> Option<String> {
    profile.signing.fingerprint.clone().or_else(|| {
        profile
            .signing
            .signing_key
            .is_none()
            .then(|| profile.ssh.as_ref().and_then(|ssh| ssh.fingerprint.clone()))
            .flatten()
    })
}

/// 执行一次带 profile token 的 gh 命令，token 只进入子进程环境。
pub fn run_gh<A>(
    args: &[A],
    config_path: Option<&Path>,
    explicit: Option<&str>,
    cwd: &Path,
) -> Result<i32>
where
    A: AsRef<std::ffi::OsStr>,
{
    let forwarded_args = args
        .iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect::<Vec<_>>();
    let argument_view = forwarded_args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let args = argument_view.as_slice();
    let mut paths = ConfigPaths::discover()?;
    if let Some(path) = config_path {
        paths.set_config_file(path)?;
    }
    let config = Config::load(&paths.config_file)?;
    let view = parse_gh_argument_view(args)?;
    let inherited_repo = std::env::var_os("GH_REPO");
    let target = gh_profile_target(&view, inherited_repo.as_deref())?;
    let ctx = AppContext::from_config_for_gh(paths, config, cwd, explicit, target.as_ref())?;
    ctx.ensure_selection_available()?;
    if ctx.profile.is_none()
        && ctx.config.behavior.unresolved == config::UnresolvedPolicy::Fail
        && is_sensitive_operation("gh", args)
    {
        return Err(AppError::Message(
            "当前仓库身份无法唯一解析，已按 unresolved=fail 停止操作".into(),
        ));
    }
    if should_display_identity_banner(
        &ctx,
        is_sensitive_operation("gh", args),
        io::stderr().is_terminal(),
    ) {
        eprintln!("{}", ctx.operation_banner());
    }
    let Some(profile) = ctx.profile.as_ref() else {
        if !ctx.warnings.is_empty() {
            eprintln!("ghis: {}", ctx.warnings.join("；"));
        }
        let mut command = CommandSpec::gh()
            .args(forwarded_args.iter().cloned())
            .current_dir(cwd);
        if config_path.is_some() {
            command = command.env("GHIS_CONFIG", ctx.paths.config_file.as_os_str());
        }
        return Ok(SystemCommandRunner::new()
            .run_passthrough(&command)?
            .code()
            .unwrap_or(128));
    };
    let gh_remote = ctx
        .repository
        .as_ref()
        .map(|repository| repo::gh_remote_context(repository, &profile.host))
        .transpose()?;
    let target_policy = validate_gh_target_view(
        &profile.host,
        &view,
        inherited_repo.as_deref(),
        gh_remote
            .as_ref()
            .and_then(repo::GhRemoteContext::remote)
            .and_then(|remote| remote.host.as_deref()),
    )?;
    let profile_id = ctx
        .profile_id()
        .ok_or_else(|| AppError::Message("未解析 Profile，无法准备 managed SSH".into()))?;
    let ssh_command = managed_ssh_material(&ctx.paths, profile_id, profile)?
        .as_ref()
        .and_then(|material| managed_ssh_environment_command(profile, Some(material)))
        .or_else(|| {
            (ctx.config.behavior.ssh_unmanaged == config::SshUnmanagedPolicy::Fail)
                .then(ssh_guard_command)
        });
    let token = github::token(&profile.host, &profile.login)?;
    let token_text = token
        .as_str()
        .ok_or_else(|| AppError::Message("gh 返回的 token 不是 UTF-8".into()))?;
    let mut command = CommandSpec::gh()
        .args(forwarded_args.iter().cloned())
        .clear_github_auth_env()
        .remove_env("GH_REPO")
        .env_secret(
            github::token_environment_variable(&profile.host),
            token_text,
        )
        .env("GH_HOST", profile.host.clone())
        .current_dir(cwd);
    if let Some(repository) = target_policy.repository_environment {
        command = command.env("GH_REPO", repository);
    }
    if config_path.is_some() {
        command = command.env("GHIS_CONFIG", ctx.paths.config_file.as_os_str());
    }
    if let Some(ssh_command) = ssh_command {
        command = apply_managed_ssh_environment(command, ssh_command);
    }
    let status = SystemCommandRunner::new().run_passthrough(&command)?;
    drop(token);
    Ok(status.code().unwrap_or(128))
}

#[derive(Debug, Default)]
struct GhTargetPolicy {
    repository_environment: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct GhProfileTarget {
    context: RuleContext,
}

fn gh_profile_target(
    view: &GhArgumentView<'_>,
    inherited_repo: Option<&std::ffi::OsStr>,
) -> Result<Option<GhProfileTarget>> {
    let inherited_repo = inherited_repo
        .map(|value| {
            value.to_str().ok_or_else(|| {
                AppError::Message("GH_REPO 不是有效 UTF-8，无法确认目标 GitHub 主机".into())
            })
        })
        .transpose()?;
    let explicit_repository = view.repository_selectors.first().copied().or_else(|| {
        view.positionals
            .iter()
            .copied()
            .find(|value| gh_repository_selector_host(value).is_some())
    });
    let repository = explicit_repository.or(inherited_repo);
    let mut host = view
        .hostname_selectors
        .first()
        .map(|value| (*value).to_owned());
    let mut owner = None;
    let mut repo_name = None;
    if let Some(repository) = repository {
        let (parsed_host, parsed_owner, parsed_repo) = parse_gh_repository_target(repository);
        host = if explicit_repository.is_some() {
            parsed_host.or(host)
        } else {
            host.or(parsed_host)
        }
        .or_else(|| Some("github.com".into()));
        owner = parsed_owner;
        repo_name = parsed_repo;
    }
    if host.is_none() && owner.is_none() {
        return Ok(None);
    }
    Ok(Some(GhProfileTarget {
        context: RuleContext {
            host,
            owner,
            repo: repo_name,
            ..RuleContext::default()
        },
    }))
}

fn parse_gh_repository_target(value: &str) -> (Option<String>, Option<String>, Option<String>) {
    let value = value.trim();
    let host = gh_repository_selector_host(value);
    let path = if let Some((_, rest)) = value.split_once("://") {
        rest.split(['?', '#'])
            .next()
            .unwrap_or(rest)
            .split_once('/')
            .map(|(_, path)| path)
    } else if let Some((_, path)) = value.split_once(':') {
        Some(path)
    } else if host.is_some() {
        value.split_once('/').map(|(_, path)| path)
    } else {
        Some(value)
    };
    let mut parts = path.unwrap_or_default().trim_matches('/').split('/');
    let owner = parts
        .next()
        .filter(|part| !part.is_empty())
        .map(str::to_owned);
    let repo = parts
        .next()
        .filter(|part| !part.is_empty())
        .map(|part| part.trim_end_matches(".git").to_owned());
    (host, owner, repo)
}

#[derive(Debug)]
struct GhArgumentView<'a> {
    command: &'a str,
    subcommand: Option<&'a str>,
    positionals: Vec<&'a str>,
    repository_selectors: Vec<&'a str>,
    hostname_selectors: Vec<&'a str>,
    option_values: Vec<(&'a str, &'a str)>,
}

#[cfg(test)]
fn validate_gh_target_host(
    profile_host: &str,
    args: &[String],
    inherited_repo: Option<&std::ffi::OsStr>,
    current_repository_host: Option<&str>,
) -> Result<GhTargetPolicy> {
    let view = parse_gh_argument_view(args)?;
    validate_gh_target_view(profile_host, &view, inherited_repo, current_repository_host)
}

fn validate_gh_target_view(
    profile_host: &str,
    view: &GhArgumentView<'_>,
    inherited_repo: Option<&std::ffi::OsStr>,
    current_repository_host: Option<&str>,
) -> Result<GhTargetPolicy> {
    if !gh_supported_top_level_command(view.command) {
        return Err(AppError::Message(format!(
            "gh 命令 `{}` 尚未纳管，或它是自定义 alias/extension；无法确认目标主机，已在取 token 前停止。确需执行时请使用 GHIS_BYPASS=1",
            view.command
        )));
    }
    for host in &view.hostname_selectors {
        ensure_gh_host_matches(profile_host, host, "--hostname")?;
    }
    for repository in &view.repository_selectors {
        validate_gh_repository_host(profile_host, repository, "--repo/-R")?;
    }

    let mut repository_context_overridden = !view.repository_selectors.is_empty();
    match (view.command, view.subcommand) {
        ("api", _) => {
            if let Some(endpoint) = view.positionals.first()
                && let Some(host) = gh_url_repository_host(endpoint)
            {
                ensure_gh_host_matches(profile_host, &host, "API URL")?;
            }
            repository_context_overridden = true;
        }
        ("auth", _) => repository_context_overridden = true,
        ("pr", Some(subcommand)) if gh_pr_accepts_item_target(subcommand) => {
            if let Some(target) = view.positionals.first()
                && let Some(host) = gh_item_url_host(target, "pr")
            {
                ensure_gh_host_matches(profile_host, &host, "Pull Request URL")?;
                repository_context_overridden = true;
            }
        }
        ("issue", Some("edit")) => {
            let mut all_targets_are_urls = !view.positionals.is_empty();
            for target in &view.positionals {
                if let Some(host) = gh_item_url_host(target, "issue") {
                    ensure_gh_host_matches(profile_host, &host, "Issue URL")?;
                } else {
                    all_targets_are_urls = false;
                }
            }
            repository_context_overridden |= all_targets_are_urls;
        }
        ("issue", Some(subcommand)) if gh_issue_accepts_item_target(subcommand) => {
            if let Some(target) = view.positionals.first()
                && let Some(host) = gh_item_url_host(target, "issue")
            {
                ensure_gh_host_matches(profile_host, &host, "Issue URL")?;
                repository_context_overridden = true;
            }
            if subcommand == "transfer"
                && let Some(destination) = view.positionals.get(1)
            {
                validate_gh_repository_host(profile_host, destination, "Issue 目标仓库")?;
            }
        }
        ("gist", Some(subcommand)) if gh_gist_accepts_item_target(subcommand) => {
            if let Some(target) = view.positionals.first()
                && let Some(host) = gh_gist_selector_host(target)
            {
                ensure_gh_gist_host_matches(profile_host, &host)?;
            }
        }
        ("repo", Some(subcommand)) if gh_repo_accepts_repository_target(subcommand) => {
            if let Some(repository) = view.positionals.first() {
                validate_gh_repository_host(profile_host, repository, "仓库位置参数")?;
                repository_context_overridden = true;
            } else if matches!(subcommand, "clone" | "create") {
                // `clone` will report its missing operand itself; `create` always
                // targets the selected account rather than the current remote.
                repository_context_overridden = true;
            }
        }
        ("repo", Some("list" | "gitignore" | "license")) => {
            repository_context_overridden = true;
        }
        ("label", Some("clone")) => {
            if let Some(source) = view.positionals.first() {
                validate_gh_repository_host(profile_host, source, "标签来源仓库")?;
            }
        }
        _ => {}
    }

    for (name, value) in &view.option_values {
        let source = match (view.command, view.subcommand, *name) {
            ("repo", Some("sync"), "source") => Some("repo sync --source"),
            ("repo", Some("create"), "template") => Some("repo create --template"),
            ("issue", Some("develop"), "branch-repo") => Some("issue develop --branch-repo"),
            _ => None,
        };
        if let Some(source) = source {
            validate_gh_repository_host(profile_host, value, source)?;
        }
        if view.command == "issue"
            && matches!(
                *name,
                "add-blocked-by"
                    | "add-blocking"
                    | "add-sub-issue"
                    | "duplicate-of"
                    | "parent"
                    | "remove-blocked-by"
                    | "remove-blocking"
                    | "remove-sub-issue"
            )
        {
            validate_gh_issue_relation_hosts(profile_host, value)?;
        }
    }

    let needs_repository_context =
        gh_command_uses_repository_context(view) && !repository_context_overridden;
    if needs_repository_context && let Some(repository) = inherited_repo {
        let repository = repository.to_str().ok_or_else(|| {
            AppError::Message("GH_REPO 不是有效 UTF-8，无法确认目标 GitHub 主机".into())
        })?;
        validate_gh_repository_host(profile_host, repository, "GH_REPO")?;
        return Ok(GhTargetPolicy {
            repository_environment: Some(repository.to_owned()),
        });
    }
    if needs_repository_context
        && inherited_repo.is_none()
        && let Some(host) = current_repository_host
    {
        ensure_gh_host_matches(profile_host, host, "当前仓库 remote")?;
    }
    Ok(GhTargetPolicy::default())
}

fn parse_gh_argument_view(args: &[String]) -> Result<GhArgumentView<'_>> {
    let Some(raw_command) = args.first().map(String::as_str) else {
        return Ok(GhArgumentView {
            command: "",
            subcommand: None,
            positionals: Vec::new(),
            repository_selectors: Vec::new(),
            hostname_selectors: Vec::new(),
            option_values: Vec::new(),
        });
    };
    let (command, subcommand, start) = if raw_command == "co" {
        ("pr", Some("checkout"), 1)
    } else if matches!(
        raw_command,
        "pr" | "issue" | "repo" | "label" | "auth" | "gist"
    ) {
        let subcommand = args.get(1).map(String::as_str);
        if subcommand.is_some_and(|subcommand| subcommand.starts_with('-')) {
            return Err(AppError::Message(format!(
                "无法安全解析 `gh {raw_command}`：请把子命令写在选项之前"
            )));
        }
        let subcommand = normalize_gh_subcommand(raw_command, subcommand);
        (
            raw_command,
            subcommand,
            usize::from(subcommand.is_some()) + 1,
        )
    } else {
        (raw_command, None, 1)
    };

    let mut view = GhArgumentView {
        command,
        subcommand,
        positionals: Vec::new(),
        repository_selectors: Vec::new(),
        hostname_selectors: Vec::new(),
        option_values: Vec::new(),
    };
    let mut index = start;
    let mut positional_only = false;
    while let Some(argument) = args.get(index).map(String::as_str) {
        if positional_only {
            view.positionals.push(argument);
            index += 1;
            continue;
        }
        if argument == "--" {
            positional_only = true;
            index += 1;
            continue;
        }
        if matches!(argument, "-R" | "--repo" | "--hostname") {
            let value = args.get(index + 1).ok_or_else(|| {
                AppError::Message(format!("gh 选项 `{argument}` 缺少参数，已停止操作"))
            })?;
            if argument == "--hostname" {
                view.hostname_selectors.push(value);
            } else {
                view.repository_selectors.push(value);
            }
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--repo=") {
            require_nonempty_gh_option("--repo", value)?;
            view.repository_selectors.push(value);
            index += 1;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--hostname=") {
            require_nonempty_gh_option("--hostname", value)?;
            view.hostname_selectors.push(value);
            index += 1;
            continue;
        }
        if let Some(value) = argument.strip_prefix("-R")
            && !value.is_empty()
        {
            let value = value.strip_prefix('=').unwrap_or(value);
            require_nonempty_gh_option("-R", value)?;
            view.repository_selectors.push(value);
            index += 1;
            continue;
        }
        if let Some(option) = argument.strip_prefix("--") {
            let (name, inline) = option
                .split_once('=')
                .map_or((option, None), |(name, value)| (name, Some(value)));
            if let Some(value) = inline {
                if gh_long_option_takes_value(command, subcommand, name) {
                    if name == "hostname" {
                        view.hostname_selectors.push(value);
                    } else {
                        view.option_values.push((name, value));
                    }
                } else if gh_strict_option_spec(command, subcommand)
                    && !gh_long_option_is_boolean(command, subcommand, name)
                {
                    return Err(unknown_gh_option(argument));
                }
                index += 1;
                continue;
            }
            if gh_long_option_has_optional_value(command, subcommand, name) {
                index += 1;
            } else if gh_long_option_takes_value(command, subcommand, name) {
                let value = args.get(index + 1).ok_or_else(|| {
                    AppError::Message(format!("gh 选项 `--{name}` 缺少参数，已停止操作"))
                })?;
                if name == "hostname" {
                    view.hostname_selectors.push(value);
                } else {
                    view.option_values.push((name, value));
                }
                index += 2;
            } else {
                if gh_strict_option_spec(command, subcommand)
                    && !gh_long_option_is_boolean(command, subcommand, name)
                {
                    return Err(unknown_gh_option(argument));
                }
                index += 1;
            }
            continue;
        }
        if argument.starts_with('-') && argument != "-" {
            let mut characters = argument[1..].chars();
            let Some(short) = characters.next() else {
                index += 1;
                continue;
            };
            if let Some(name) = gh_short_value_option(command, subcommand, short) {
                let attached = &argument[1 + short.len_utf8()..];
                if attached.is_empty()
                    && gh_short_option_has_optional_value(command, subcommand, short)
                {
                    index += 1;
                } else if attached.is_empty() {
                    let value = args.get(index + 1).ok_or_else(|| {
                        AppError::Message(format!("gh 选项 `-{short}` 缺少参数，已停止操作"))
                    })?;
                    if name == "hostname" {
                        view.hostname_selectors.push(value);
                    } else {
                        view.option_values.push((name, value));
                    }
                    index += 2;
                } else {
                    if name == "hostname" {
                        view.hostname_selectors.push(attached);
                    } else {
                        view.option_values.push((name, attached));
                    }
                    index += 1;
                }
            } else {
                if gh_strict_option_spec(command, subcommand)
                    && !argument[1..]
                        .chars()
                        .all(|short| gh_short_option_is_boolean(command, subcommand, short))
                {
                    return Err(unknown_gh_option(argument));
                }
                index += 1;
            }
            continue;
        }
        view.positionals.push(argument);
        index += 1;
    }
    Ok(view)
}

fn normalize_gh_subcommand<'a>(command: &str, subcommand: Option<&'a str>) -> Option<&'a str> {
    match (command, subcommand) {
        ("pr", Some("co")) => Some("checkout"),
        ("pr" | "issue" | "repo", Some("new")) => Some("create"),
        ("pr" | "issue" | "repo" | "label" | "gist", Some("ls")) => Some("list"),
        (_, subcommand) => subcommand,
    }
}

fn require_nonempty_gh_option(option: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(AppError::Message(format!(
            "gh 选项 `{option}` 的参数为空，已停止操作"
        )));
    }
    Ok(())
}

fn unknown_gh_option(option: &str) -> AppError {
    AppError::Message(format!(
        "无法安全判断 gh 选项 `{option}` 是否带参数；为防止把内容误认成仓库目标，已在取 token 前停止"
    ))
}

fn gh_repository_selector_host(repository: &str) -> Option<String> {
    let repository = repository.trim();
    if let Some(host) = gh_url_repository_host(repository) {
        return Some(host);
    }
    if let Some(closing_bracket) = repository.find("]:") {
        let host = &repository[..=closing_bracket];
        let path = &repository[closing_bracket + 2..];
        let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
        if path.split('/').filter(|part| !part.is_empty()).count() >= 2 {
            return Some(host.to_owned());
        }
    }
    if let Some((host, path)) = repository.split_once(':')
        && !host.contains('/')
        && path.split('/').filter(|part| !part.is_empty()).count() >= 2
    {
        let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
        return (!host.is_empty()).then(|| host.to_owned());
    }
    let parts = repository.split('/').collect::<Vec<_>>();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return None;
    }
    Some(parts[0].to_owned())
}

fn gh_url_repository_host(repository: &str) -> Option<String> {
    if let Some((scheme, rest)) = repository.split_once("://")
        && matches!(
            scheme.to_ascii_lowercase().as_str(),
            "https" | "http" | "ssh" | "git" | "git+ssh" | "git+https"
        )
    {
        let authority = rest.split(['/', '?', '#']).next()?;
        let authority = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        return (!authority.is_empty()).then(|| authority.to_owned());
    }
    None
}

fn gh_gist_selector_host(selector: &str) -> Option<String> {
    let selector = selector.trim();
    if let Some(host) = gh_url_repository_host(selector) {
        return Some(host);
    }
    if let Some((host, path)) = selector.split_once(':')
        && !host.contains('/')
        && !path.is_empty()
    {
        let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
        return (!host.is_empty()).then(|| host.to_owned());
    }
    None
}

fn validate_gh_repository_host(profile_host: &str, repository: &str, source: &str) -> Result<()> {
    if let Some(host) = gh_repository_selector_host(repository) {
        ensure_gh_host_matches(profile_host, &host, source)?;
    }
    Ok(())
}

fn gh_pr_accepts_item_target(subcommand: &str) -> bool {
    matches!(
        subcommand,
        "checkout"
            | "checks"
            | "close"
            | "comment"
            | "diff"
            | "edit"
            | "lock"
            | "merge"
            | "ready"
            | "reopen"
            | "revert"
            | "review"
            | "unlock"
            | "update-branch"
            | "view"
    )
}

fn gh_issue_accepts_item_target(subcommand: &str) -> bool {
    matches!(
        subcommand,
        "close"
            | "comment"
            | "delete"
            | "develop"
            | "lock"
            | "pin"
            | "reopen"
            | "transfer"
            | "unlock"
            | "unpin"
            | "view"
    )
}

fn gh_gist_accepts_item_target(subcommand: &str) -> bool {
    matches!(subcommand, "clone" | "delete" | "edit" | "rename" | "view")
}

fn gh_repo_accepts_repository_target(subcommand: &str) -> bool {
    matches!(
        subcommand,
        "archive"
            | "clone"
            | "create"
            | "delete"
            | "edit"
            | "fork"
            | "set-default"
            | "sync"
            | "unarchive"
            | "view"
    )
}

fn gh_structured_target_command(command: &str, subcommand: Option<&str>) -> bool {
    match (command, subcommand) {
        ("pr", Some(subcommand)) => gh_pr_accepts_item_target(subcommand),
        ("issue", Some("edit")) => true,
        ("issue", Some(subcommand)) => gh_issue_accepts_item_target(subcommand),
        ("repo", Some(subcommand)) => gh_repo_accepts_repository_target(subcommand),
        ("label", Some("clone")) => true,
        _ => false,
    }
}

fn gh_strict_option_spec(command: &str, subcommand: Option<&str>) -> bool {
    gh_structured_target_command(command, subcommand) || matches!(command, "browse" | "gist")
}

fn gh_command_uses_repository_context(view: &GhArgumentView<'_>) -> bool {
    match (view.command, view.subcommand) {
        ("pr" | "issue", _) => true,
        ("repo", Some(subcommand)) => !matches!(
            subcommand,
            "clone" | "create" | "gitignore" | "license" | "list"
        ),
        (
            "attestation" | "browse" | "cache" | "label" | "release" | "ruleset" | "run" | "secret"
            | "variable" | "workflow",
            _,
        ) => true,
        _ => false,
    }
}

fn gh_supported_top_level_command(command: &str) -> bool {
    matches!(
        command,
        "" | "--help"
            | "--version"
            | "alias"
            | "api"
            | "auth"
            | "browse"
            | "cache"
            | "completion"
            | "config"
            | "gist"
            | "gpg-key"
            | "help"
            | "issue"
            | "label"
            | "licenses"
            | "org"
            | "pr"
            | "release"
            | "repo"
            | "ruleset"
            | "run"
            | "search"
            | "secret"
            | "ssh-key"
            | "status"
            | "variable"
            | "workflow"
    )
}

fn gh_long_option_has_optional_value(command: &str, subcommand: Option<&str>, name: &str) -> bool {
    matches!((command, subcommand, name), ("browse", None, "commit"))
}

fn gh_short_option_has_optional_value(
    command: &str,
    subcommand: Option<&str>,
    short: char,
) -> bool {
    matches!((command, subcommand, short), ("browse", None, 'c'))
}

fn gh_long_option_takes_value(command: &str, subcommand: Option<&str>, name: &str) -> bool {
    if matches!(
        (command, subcommand, name),
        ("browse", None, "branch" | "commit")
            | ("gist", Some("create"), "desc" | "filename")
            | ("gist", Some("edit"), "add" | "desc" | "filename" | "remove")
            | ("gist", Some("list"), "filter" | "limit")
            | ("gist", Some("view"), "filename")
    ) {
        return true;
    }
    if matches!(
        (command, subcommand, name),
        ("pr", Some("review"), "comment")
            | ("repo", Some("edit"), "template")
            | ("repo", Some("fork"), "remote")
            | ("repo", Some("list"), "source")
    ) {
        return false;
    }
    matches!(
        name,
        "add-assignee"
            | "add-topic"
            | "add-blocked-by"
            | "add-blocking"
            | "add-label"
            | "add-project"
            | "add-reviewer"
            | "add-sub-issue"
            | "app"
            | "assignee"
            | "author"
            | "author-email"
            | "base"
            | "blocked-by"
            | "blocking"
            | "body"
            | "body-file"
            | "branch"
            | "branch-repo"
            | "color"
            | "comment"
            | "default-branch"
            | "description"
            | "duplicate-of"
            | "exclude"
            | "fork-name"
            | "gitignore"
            | "head"
            | "homepage"
            | "interval"
            | "jq"
            | "json"
            | "label"
            | "license"
            | "limit"
            | "match-head-commit"
            | "mention"
            | "milestone"
            | "name"
            | "org"
            | "parent"
            | "project"
            | "reason"
            | "recover"
            | "remote"
            | "remote-name"
            | "remove-assignee"
            | "remove-blocked-by"
            | "remove-blocking"
            | "remove-label"
            | "remove-project"
            | "remove-reviewer"
            | "remove-sub-issue"
            | "remove-topic"
            | "reviewer"
            | "search"
            | "source"
            | "squash-merge-commit-message"
            | "state"
            | "subject"
            | "team"
            | "template"
            | "title"
            | "topic"
            | "type"
            | "upstream-remote-name"
            | "visibility"
    )
}

fn gh_long_option_is_boolean(command: &str, subcommand: Option<&str>, name: &str) -> bool {
    if name == "help" {
        return true;
    }
    if matches!(
        (command, subcommand, name),
        ("pr", Some("review"), "comment")
            | ("repo", Some("edit"), "template")
            | ("repo", Some("fork"), "remote")
            | ("repo", Some("list"), "source")
    ) {
        return true;
    }
    if matches!(
        (command, subcommand, name),
        (
            "browse",
            None,
            "actions"
                | "blame"
                | "commit"
                | "no-browser"
                | "projects"
                | "releases"
                | "settings"
                | "wiki"
        ) | ("gist", Some("create"), "public" | "web")
            | ("gist", Some("delete"), "yes")
            | (
                "gist",
                Some("list"),
                "include-content" | "public" | "secret"
            )
            | (
                "gist",
                Some("view"),
                "allow-escape-sequences" | "files" | "raw" | "web"
            )
    ) {
        return true;
    }
    matches!(
        name,
        "accept-visibility-change-consequences"
            | "add-readme"
            | "admin"
            | "allow-escape-sequences"
            | "allow-forking"
            | "allow-update-branch"
            | "approve"
            | "auto"
            | "checkout"
            | "clone"
            | "comments"
            | "create-if-none"
            | "default-branch-only"
            | "delete-branch"
            | "delete-branch-on-merge"
            | "delete-last"
            | "detach"
            | "disable-auto"
            | "disable-issues"
            | "disable-wiki"
            | "draft"
            | "dry-run"
            | "edit-last"
            | "editor"
            | "enable-advanced-security"
            | "enable-auto-merge"
            | "enable-discussions"
            | "enable-issues"
            | "enable-merge-commit"
            | "enable-projects"
            | "enable-rebase-merge"
            | "enable-secret-scanning"
            | "enable-secret-scanning-push-protection"
            | "enable-squash-merge"
            | "enable-wiki"
            | "fail-fast"
            | "fill"
            | "fill-first"
            | "fill-verbose"
            | "force"
            | "include-all-branches"
            | "internal"
            | "list"
            | "merge"
            | "name-only"
            | "no-maintainer-edit"
            | "no-upstream"
            | "patch"
            | "private"
            | "public"
            | "push"
            | "rebase"
            | "recurse-submodules"
            | "remote"
            | "remove-milestone"
            | "remove-parent"
            | "remove-type"
            | "required"
            | "request-changes"
            | "squash"
            | "template"
            | "undo"
            | "unset"
            | "view"
            | "watch"
            | "web"
            | "yes"
    )
}

fn gh_short_value_option(
    command: &str,
    subcommand: Option<&str>,
    short: char,
) -> Option<&'static str> {
    let option = match (command, subcommand, short) {
        ("browse", None, 'b') => "branch",
        ("browse", None, 'c') => "commit",
        ("gist", Some("create"), 'd') => "desc",
        ("gist", Some("create" | "edit" | "view"), 'f') => "filename",
        ("gist", Some("edit"), 'a') => "add",
        ("gist", Some("edit"), 'd') => "desc",
        ("gist", Some("edit"), 'r') => "remove",
        ("gist", Some("list"), 'L') => "limit",
        ("pr" | "issue", _, 'F') => "body-file",
        ("pr", Some("edit" | "create"), 'B') => "base",
        ("pr", Some("create"), 'H') => "head",
        ("pr" | "issue", _, 'q') => "jq",
        ("pr" | "issue", _, 'j') => "json",
        ("pr" | "issue", _, 't') => "title",
        ("pr", Some("checkout"), 'b') => "branch",
        ("pr", Some("comment" | "edit" | "merge" | "revert" | "review"), 'b') => "body",
        ("issue", Some("comment" | "edit"), 'b') => "body",
        ("issue", Some("develop"), 'b') => "base",
        ("pr", Some("checks"), 'i') => "interval",
        ("pr" | "issue", Some("close" | "reopen"), 'c') => "comment",
        ("pr", Some("diff"), 'e') => "exclude",
        ("pr" | "issue", Some("lock"), 'r') => "reason",
        ("pr" | "issue", Some("edit"), 'm') => "milestone",
        ("pr" | "issue", Some("edit"), 'l') => "label",
        ("issue", Some("develop"), 'n') => "name",
        ("repo", Some("clone"), 'u') => "upstream-remote-name",
        ("repo", Some("sync" | "view"), 'b') => "branch",
        ("repo", Some("create" | "edit"), 'd') => "description",
        ("repo", Some("create"), 'g') => "gitignore",
        ("repo", Some("create" | "edit"), 'h') => "homepage",
        ("repo", Some("create"), 'l') => "license",
        ("repo", Some("create"), 'p') => "template",
        ("repo", Some("create"), 'r') => "remote",
        ("repo", Some("create" | "sync"), 's') => "source",
        ("repo", Some("create"), 't') => "team",
        ("repo", Some("view"), 'q') => "jq",
        ("repo", Some("view"), 'j') => "json",
        ("repo", Some("view"), 't') => "template",
        ("auth", _, 'h') => "hostname",
        _ => return None,
    };
    Some(option)
}

fn gh_short_option_is_boolean(command: &str, subcommand: Option<&str>, short: char) -> bool {
    matches!(
        (command, subcommand, short),
        ("browse", None, 'a' | 'n' | 'p' | 'r' | 's' | 'w')
            | ("gist", Some("create"), 'p' | 'w')
            | ("gist", Some("view"), 'r' | 'w')
            | ("pr", Some("checkout"), 'f')
            | ("pr", Some("checks"), 'w')
            | ("pr", Some("close"), 'd')
            | ("pr", Some("comment"), 'e' | 'w')
            | ("pr", Some("diff"), 'w')
            | ("pr", Some("merge"), 'd' | 'm' | 'r' | 's')
            | ("pr", Some("review"), 'a' | 'c' | 'r')
            | ("pr", Some("view"), 'c' | 'w')
            | ("issue", Some("comment"), 'e' | 'w')
            | ("issue", Some("develop"), 'c' | 'l')
            | ("issue", Some("view"), 'c' | 'w')
            | ("repo", Some("archive" | "unarchive"), 'y')
            | ("repo", Some("create"), 'c')
            | ("repo", Some("set-default"), 'u' | 'v')
            | ("repo", Some("view"), 'w')
            | ("label", Some("clone"), 'f')
    )
}

fn gh_item_url_host(argument: &str, command: &str) -> Option<String> {
    let host = gh_url_repository_host(argument)?;
    let (_, rest) = argument.split_once("://")?;
    let path = rest.split_once('/')?.1;
    let mut segments = path.split('/');
    let owner = segments.next()?;
    let repository = segments.next()?;
    let kind = segments.next()?;
    let number = segments.next()?.split(['?', '#']).next()?;
    let kind_matches = if command == "pr" {
        kind == "pull"
    } else {
        matches!(kind, "issues" | "pull")
    };
    (!owner.is_empty()
        && !repository.is_empty()
        && kind_matches
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && !number.is_empty())
    .then_some(host)
}

fn validate_gh_issue_relation_hosts(profile_host: &str, value: &str) -> Result<()> {
    for reference in value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        if let Some(host) = gh_item_url_host(reference, "issue") {
            ensure_gh_host_matches(profile_host, &host, "Issue 关系 URL")?;
        }
    }
    Ok(())
}

fn ensure_gh_gist_host_matches(profile_host: &str, target_host: &str) -> Result<()> {
    let expected = github::normalize_host(profile_host);
    let actual = github::normalize_host(target_host);
    let api_host = actual.strip_prefix("gist.").unwrap_or(&actual);
    if network_hosts_match(&actual, &expected) || network_hosts_match(api_host, &expected) {
        return Ok(());
    }
    Err(AppError::Message(format!(
        "Gist URL 指向 GitHub 主机 {actual}，与当前 Profile 主机 {expected} 不一致，已停止操作"
    )))
}

fn ensure_gh_host_matches(profile_host: &str, target_host: &str, source: &str) -> Result<()> {
    let expected = github::normalize_host(profile_host);
    let actual = github::normalize_host(target_host);
    if network_hosts_match(&actual, &expected) {
        return Ok(());
    }
    Err(AppError::Message(format!(
        "{source} 指向 GitHub 主机 {actual}，与当前 Profile 主机 {expected} 不一致，已停止操作"
    )))
}

fn network_hosts_match(left: &str, right: &str) -> bool {
    fn comparable(host: &str) -> String {
        let host = github::normalize_host(host);
        host.strip_suffix(":443").unwrap_or(&host).to_owned()
    }
    comparable(left) == comparable(right)
}

fn credential_helper_command(paths: &ConfigPaths) -> String {
    let binary = current_binary();
    format!(
        "!{} --config {} credential-helper",
        git::shell_quote(&binary),
        git::shell_quote(&paths.config_file.to_string_lossy())
    )
}

fn remove_managed_credential_helpers(repository: &Repository) -> Result<()> {
    let keys = repo::local_config_keys_matching(repository, r"^credential\.https://.*\.helper$")?;
    for key in keys {
        let values = repo::local_config_values(repository, &key)?;
        let Some(retained) = remove_managed_helper_entries(&values) else {
            continue;
        };
        repo::replace_local_config_values(repository, &key, &retained)?;
    }
    Ok(())
}

fn remove_managed_helper_entries(values: &[String]) -> Option<Vec<String>> {
    let mut remove = vec![false; values.len()];
    let mut found = false;
    for (index, value) in values.iter().enumerate() {
        if !is_managed_credential_helper(value) {
            continue;
        }
        found = true;
        remove[index] = true;
        if index > 0 && values[index - 1].is_empty() && !remove[index - 1] {
            remove[index - 1] = true;
        }
    }
    found.then(|| {
        values
            .iter()
            .zip(remove)
            .filter_map(|(value, remove)| (!remove).then_some(value.clone()))
            .collect()
    })
}

fn is_managed_credential_helper(value: &str) -> bool {
    let Some((quoted_program, arguments)) = value
        .strip_prefix('!')
        .and_then(|command| command.split_once(" --config "))
    else {
        return false;
    };
    let Some(config_argument) = arguments.strip_suffix(" credential-helper") else {
        return false;
    };
    if config_argument.trim().is_empty() {
        return false;
    }
    if quoted_program == git::shell_quote(&current_binary()) {
        return true;
    }
    let program = quoted_program
        .strip_prefix('\'')
        .and_then(|program| program.strip_suffix('\''))
        .unwrap_or(quoted_program);
    Path::new(program)
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|name| name == "ghis")
}

fn current_binary() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
        .unwrap_or_else(|| "ghis".into())
}

fn repository_binding_needs_repair(ctx: &AppContext) -> Result<bool> {
    let Some(repository) = ctx.repository.as_ref() else {
        return Ok(false);
    };
    let Some(profile_id) = ctx.profile_id() else {
        return Ok(false);
    };
    let profile = ctx
        .profile
        .as_ref()
        .expect("a resolved profile id always has profile data");
    if !repository.bare && !repo::uses_worktree_config(repository)? {
        return Ok(true);
    }
    let fragment = profile_fragment_path(&ctx.paths, profile_id);
    let ssh_config = managed_ssh_config_path(&ctx.paths, profile_id);
    let expected_fragment =
        profile_fragment_content(profile, ssh_config.exists().then_some(ssh_config.as_path()));
    if fs::read_to_string(&fragment).ok().as_deref() != Some(expected_fragment.as_str()) {
        return Ok(true);
    }
    let include_key = profile_include_key(repository);
    let configured = repo::local_config(repository, &include_key)?;
    if configured.as_deref() != Some(fragment.to_string_lossy().as_ref()) {
        return Ok(true);
    }
    let key = git::credential_helper_key(&profile.host);
    let expected = credential_helper_command(&ctx.paths);
    let expected_values = vec![String::new(), expected];
    if repo::local_config_values(repository, &key)? != expected_values {
        return Ok(true);
    }
    if git::supports_named_hooks()
        && !git::named_hooks_match(repository, &current_binary(), Some(&ctx.paths.config_file))?
    {
        return Ok(true);
    }
    Ok(false)
}

fn should_display_identity_banner(
    ctx: &AppContext,
    sensitive: bool,
    stderr_is_terminal: bool,
) -> bool {
    stderr_is_terminal
        && match ctx.config.behavior.display_identity {
            config::DisplayIdentity::Always => true,
            config::DisplayIdentity::Never => false,
            config::DisplayIdentity::SensitiveCommands => sensitive,
        }
}

fn is_sensitive_operation(command: &str, args: &[String]) -> bool {
    let first = if command == "git" {
        git_subcommand(args)
    } else {
        args.first().map(String::as_str).unwrap_or_default()
    };
    if command == "git" {
        matches!(
            first,
            "commit" | "push" | "pull" | "fetch" | "clone" | "submodule"
        )
    } else {
        matches!(
            first,
            "pr" | "issue" | "repo" | "release" | "run" | "workflow" | "auth"
        )
    }
}

fn git_subcommand(args: &[String]) -> &str {
    git_subcommand_index(args)
        .and_then(|index| args.get(index))
        .map(String::as_str)
        .unwrap_or_default()
}

fn git_subcommand_index(args: &[String]) -> Option<usize> {
    let mut index = 0usize;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            return args.get(index + 1).map(|_| index + 1);
        }
        if !arg.starts_with('-') || arg == "-" {
            return Some(index);
        }
        let takes_value = matches!(
            arg.as_str(),
            "-C" | "-c"
                | "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--super-prefix"
                | "--config-env"
        );
        index += if takes_value { 2 } else { 1 };
    }
    None
}

fn git_policy_insertion_index(args: &[String]) -> usize {
    let Some(subcommand) = git_subcommand_index(args) else {
        // Commands such as `git --version` have no subcommand. Git's global
        // `-c` options must precede these flags as well.
        return 0;
    };
    if subcommand > 0 && args.get(subcommand - 1).is_some_and(|arg| arg == "--") {
        subcommand - 1
    } else {
        subcommand
    }
}

#[derive(Debug, Clone)]
struct GitOperation {
    name: String,
    conservative_sensitive: bool,
    uninspectable_alias: bool,
    arguments: Vec<String>,
}

impl GitOperation {
    fn is_sensitive(&self) -> bool {
        self.conservative_sensitive
            || self.writes_identity()
            || matches!(
                self.name.as_str(),
                "commit" | "push" | "pull" | "fetch" | "clone" | "submodule"
            )
    }

    fn writes_identity(&self) -> bool {
        matches!(
            self.name.as_str(),
            "commit" | "merge" | "cherry-pick" | "revert" | "rebase" | "am" | "tag"
        )
    }

    fn requires_remote_resolution(&self) -> bool {
        if matches!(self.name.as_str(), "push" | "pull" | "fetch" | "ls-remote") {
            return true;
        }
        let arguments = git_subcommand_index(&self.arguments)
            .map(|index| &self.arguments[index + 1..])
            .unwrap_or_default();
        matches!(self.name.as_str(), "remote")
            && arguments
                .iter()
                .any(|argument| matches!(argument.as_str(), "update" | "prune"))
    }

    fn may_contact_remote(&self) -> bool {
        if self.conservative_sensitive
            || matches!(
                self.name.as_str(),
                "push" | "pull" | "fetch" | "clone" | "submodule" | "ls-remote"
            )
        {
            return true;
        }
        let arguments = git_subcommand_index(&self.arguments)
            .map(|index| &self.arguments[index + 1..])
            .unwrap_or_default();
        match self.name.as_str() {
            "remote" => arguments
                .iter()
                .any(|argument| matches!(argument.as_str(), "update" | "prune")),
            "archive" => arguments
                .iter()
                .any(|argument| argument == "--remote" || argument.starts_with("--remote=")),
            "maintenance" => arguments.iter().any(|argument| argument == "run"),
            _ => false,
        }
    }
}

fn resolve_git_operation(ctx: &AppContext, args: &[String], cwd: &Path) -> GitOperation {
    let mut arguments = git_subcommand_index(args)
        .map(|index| args[index..].to_vec())
        .unwrap_or_default();
    let initial = git_subcommand(&arguments).to_owned();
    if initial.is_empty() || is_known_git_command(&initial) {
        return GitOperation {
            name: initial,
            conservative_sensitive: false,
            uninspectable_alias: false,
            arguments,
        };
    }

    let command_line_aliases = command_line_git_config(args)
        .into_iter()
        .filter_map(|item| {
            item.key
                .strip_prefix("alias.")
                .map(|name| (name.to_owned(), item.value))
        })
        .collect::<BTreeMap<_, _>>();
    let directory = ctx
        .repository
        .as_ref()
        .map(Repository::command_dir)
        .unwrap_or(cwd);
    let mut seen = BTreeSet::new();
    let mut conservative_sensitive = false;
    for _ in 0..16 {
        let current = git_subcommand(&arguments).to_owned();
        if is_known_git_command(&current) {
            return GitOperation {
                name: current,
                conservative_sensitive,
                uninspectable_alias: false,
                arguments,
            };
        }
        if !seen.insert(current.clone()) {
            return GitOperation {
                name: current,
                conservative_sensitive: true,
                uninspectable_alias: true,
                arguments,
            };
        }
        let alias = command_line_aliases
            .get(&current.to_ascii_lowercase())
            .cloned()
            .or_else(|| configured_git_alias(directory, &current));
        let Some(alias) = alias else {
            return GitOperation {
                name: current,
                // Unknown `git-foo` programs can perform network operations.
                // Treat them conservatively so auth safety checks still run.
                conservative_sensitive: true,
                uninspectable_alias: false,
                arguments,
            };
        };
        if let Some(shell_command) = alias.trim_start().strip_prefix('!') {
            if let Some(mut expanded) = direct_shell_git_arguments(shell_command) {
                conservative_sensitive = true;
                append_alias_call_arguments(&mut expanded, &arguments);
                arguments = expanded;
                continue;
            }
            return GitOperation {
                name: current,
                conservative_sensitive: true,
                uninspectable_alias: true,
                arguments,
            };
        }
        let mut expanded = split_git_alias(&alias);
        append_alias_call_arguments(&mut expanded, &arguments);
        let next = git_subcommand(&expanded);
        if next.is_empty() {
            return GitOperation {
                name: current,
                conservative_sensitive: true,
                uninspectable_alias: true,
                arguments,
            };
        }
        arguments = expanded;
    }
    GitOperation {
        name: git_subcommand(&arguments).to_owned(),
        conservative_sensitive: true,
        uninspectable_alias: true,
        arguments,
    }
}

fn append_alias_call_arguments(expanded: &mut Vec<String>, invocation: &[String]) {
    let Some(index) = git_subcommand_index(invocation) else {
        return;
    };
    expanded.extend_from_slice(&invocation[index + 1..]);
}

fn direct_shell_git_arguments(command: &str) -> Option<Vec<String>> {
    let words = split_git_alias(command);
    let executable = words.first()?;
    if Path::new(executable).file_name()? != "git" {
        return None;
    }
    let arguments = words[1..].to_vec();
    (!git_subcommand(&arguments).is_empty()).then_some(arguments)
}

fn configured_git_alias(directory: &Path, name: &str) -> Option<String> {
    let key = format!("alias.{name}");
    let output = git::run_git(directory, ["config", "--get", key.as_str()]).ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned()
    })
}

fn is_known_git_command(name: &str) -> bool {
    matches!(
        name,
        "add"
            | "am"
            | "archive"
            | "bisect"
            | "blame"
            | "branch"
            | "bundle"
            | "checkout"
            | "cherry-pick"
            | "clean"
            | "clone"
            | "commit"
            | "config"
            | "describe"
            | "diff"
            | "difftool"
            | "fetch"
            | "format-patch"
            | "fsck"
            | "gc"
            | "grep"
            | "help"
            | "init"
            | "log"
            | "maintenance"
            | "merge"
            | "mergetool"
            | "mv"
            | "notes"
            | "pull"
            | "push"
            | "range-diff"
            | "rebase"
            | "reflog"
            | "remote"
            | "repack"
            | "replace"
            | "reset"
            | "restore"
            | "revert"
            | "rm"
            | "shortlog"
            | "show"
            | "show-branch"
            | "sparse-checkout"
            | "stash"
            | "status"
            | "submodule"
            | "switch"
            | "tag"
            | "verify-commit"
            | "verify-tag"
            | "version"
            | "whatchanged"
            | "worktree"
    )
}

fn split_git_alias(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        match (quote, character) {
            (Some('\''), '\'') => quote = None,
            (Some('"'), '"') => quote = None,
            (Some('\''), _) => current.push(character),
            (Some('"'), '\\') | (None, '\\') => escaped = true,
            (Some(_), _) => current.push(character),
            (None, '\'') | (None, '"') => quote = Some(character),
            (None, character) if character.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            (None, _) => current.push(character),
        }
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

#[derive(Debug)]
struct GitConfigOverride {
    key: String,
    value: String,
    source: String,
}

fn command_line_git_config(args: &[String]) -> Vec<GitConfigOverride> {
    let end = git_subcommand_index(args).unwrap_or(args.len());
    let mut values = Vec::new();
    let mut index = 0usize;
    while index < end {
        let arg = &args[index];
        if arg == "-c" {
            if let Some(value) = args.get(index + 1) {
                push_config_override(&mut values, value, "git -c");
            }
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("-c")
            && !value.is_empty()
        {
            push_config_override(&mut values, value, "git -c");
            index += 1;
            continue;
        }
        let config_env = if arg == "--config-env" {
            args.get(index + 1).map(String::as_str)
        } else {
            arg.strip_prefix("--config-env=")
        };
        if let Some(specification) = config_env
            && let Some((key, environment)) = specification.split_once('=')
            && let Some(value) = std::env::var_os(environment)
        {
            values.push(GitConfigOverride {
                key: key.to_ascii_lowercase(),
                value: value.to_string_lossy().into_owned(),
                source: format!("git --config-env {key}"),
            });
        }
        index += if arg == "--config-env" { 2 } else { 1 };
    }
    values
}

fn push_config_override(values: &mut Vec<GitConfigOverride>, assignment: &str, source: &str) {
    let (key, value) = assignment.split_once('=').unwrap_or((assignment, "true"));
    values.push(GitConfigOverride {
        key: key.to_ascii_lowercase(),
        value: value.to_owned(),
        source: format!("{source} {key}"),
    });
}

fn validate_commit_signing_argv(
    ctx: &AppContext,
    args: &[String],
    operation: &GitOperation,
) -> Result<()> {
    let signing_enabled = ctx
        .profile
        .as_ref()
        .is_some_and(|profile| profile.signing.enabled);
    validate_commit_signing_policy(signing_enabled, args, operation)
}

fn validate_commit_signing_policy(
    signing_enabled: bool,
    args: &[String],
    operation: &GitOperation,
) -> Result<()> {
    if !signing_enabled || operation.name != "commit" {
        return Ok(());
    }

    if commit_disables_signing(&operation.arguments) {
        return Err(AppError::Message(
            "当前 Profile 要求提交签名，不能使用 `commit --no-gpg-sign`；请移除该参数，或改用已禁用 signing 的 Profile"
                .into(),
        ));
    }

    let mut overrides = command_line_git_config(args);
    overrides.extend(command_line_git_config(&operation.arguments));
    if overrides
        .iter()
        .any(|item| item.key == "commit.gpgsign" && !git_boolean_is_enabled(&item.value))
    {
        return Err(AppError::Message(
            "当前 Profile 要求提交签名，命令行 Git 配置不能关闭 `commit.gpgSign`；请移除该配置覆盖，或改用已禁用 signing 的 Profile"
                .into(),
        ));
    }

    Ok(())
}

fn commit_disables_signing(args: &[String]) -> bool {
    let Some(command) = git_subcommand_index(args) else {
        return false;
    };
    let mut index = command + 1;
    while let Some(argument) = args.get(index) {
        if argument == "--" {
            break;
        }
        if argument == "--no-gpg-sign" {
            return true;
        }
        // Do not interpret the value of an option as another option. This list
        // covers commit options whose value can be supplied as the next argv.
        let takes_value = matches!(
            argument.as_str(),
            "-m" | "--message"
                | "-F"
                | "--file"
                | "-C"
                | "--reuse-message"
                | "-c"
                | "--reedit-message"
                | "--fixup"
                | "--squash"
                | "--author"
                | "--date"
                | "--cleanup"
                | "-t"
                | "--template"
                | "--trailer"
                | "--pathspec-from-file"
                | "--untracked-files"
        );
        index += if takes_value { 2 } else { 1 };
    }
    false
}

fn git_boolean_is_enabled(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "on" | "1"
    )
}

fn validate_git_auth_safety(
    ctx: &AppContext,
    args: &[String],
    operation: &GitOperation,
) -> Result<()> {
    let Some(profile) = ctx.profile.as_ref() else {
        return Ok(());
    };
    if operation.uninspectable_alias {
        return Err(AppError::Message(
            "当前 Git shell alias 无法安全解析，不能保证提交和网络身份；已停止操作。确需执行时请使用 GHIS_BYPASS=1"
                .into(),
        ));
    }

    let mut overrides = command_line_git_config(args);
    overrides.extend(command_line_git_config(&operation.arguments));
    for item in &overrides {
        if is_credential_helper_key(&item.key) {
            return Err(AppError::Message(
                "检测到命令行 credential helper 覆盖；它会绕过所选 Profile，已停止操作。确需自行管理凭据时请使用 GHIS_BYPASS=1"
                    .into(),
            ));
        }
    }

    if !operation.may_contact_remote() {
        return Ok(());
    }
    if args
        .iter()
        .chain(operation.arguments.iter())
        .any(|argument| contains_https_password_for_host(argument, Some(&profile.host)))
        || ctx.remote.as_ref().is_some_and(|remote| {
            contains_https_password_for_host(&remote.url, Some(&profile.host))
        })
    {
        return Err(unsafe_https_url_error());
    }
    for item in &overrides {
        if override_contains_https_password(item, &profile.host) {
            return Err(unsafe_https_url_error());
        }
        if is_http_extra_header_key(&item.key)
            && http_extra_header_applies_to_host(&item.key, &profile.host)
            && is_authorization_header(&item.value)
        {
            return Err(authorization_header_error());
        }
    }
    inspect_repository_auth_config(ctx)
}

fn is_credential_helper_key(key: &str) -> bool {
    key == "credential.helper" || (key.starts_with("credential.") && key.ends_with(".helper"))
}

fn is_http_extra_header_key(key: &str) -> bool {
    key == "http.extraheader" || (key.starts_with("http.") && key.ends_with(".extraheader"))
}

fn http_extra_header_applies_to_host(key: &str, profile_host: &str) -> bool {
    let key = key.to_ascii_lowercase();
    if key == "http.extraheader" {
        return true;
    }
    key.strip_prefix("http.")
        .and_then(|value| value.strip_suffix(".extraheader"))
        .and_then(gh_url_repository_host)
        .is_some_and(|host| network_hosts_match(&host, profile_host))
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

fn override_contains_https_password(item: &GitConfigOverride, profile_host: &str) -> bool {
    if item.key.starts_with("remote.")
        && (item.key.ends_with(".url") || item.key.ends_with(".pushurl"))
    {
        return contains_https_password_for_host(&item.value, Some(profile_host));
    }
    rewrite_base_url(&item.key)
        .is_some_and(|url| contains_https_password_for_host(url, Some(profile_host)))
}

fn rewrite_base_url(key: &str) -> Option<&str> {
    let key = key.strip_prefix("url.")?;
    key.strip_suffix(".insteadof")
        .or_else(|| key.strip_suffix(".pushinsteadof"))
}

fn contains_https_password_for_host(value: &str, profile_host: Option<&str>) -> bool {
    let bytes = value.as_bytes();
    let mut offset = 0usize;
    while let Some((scheme, scheme_len)) = find_http_scheme(bytes, offset) {
        let authority_start = scheme + scheme_len;
        let authority_end = bytes[authority_start..]
            .iter()
            .position(|byte| matches!(byte, b'/' | b'?' | b'#'))
            .map(|index| authority_start + index)
            .unwrap_or(bytes.len());
        let authority = &bytes[authority_start..authority_end];
        if authority
            .iter()
            .rposition(|byte| *byte == b'@')
            .is_some_and(|at| {
                authority[..at].contains(&b':')
                    && std::str::from_utf8(&authority[at + 1..]).is_ok_and(|host| {
                        profile_host
                            .is_none_or(|profile_host| network_hosts_match(host, profile_host))
                    })
            })
        {
            return true;
        }
        offset = authority_end.max(authority_start + 1);
        if offset >= bytes.len() {
            break;
        }
    }
    false
}

fn find_http_scheme(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    (start..bytes.len()).find_map(|index| {
        let remaining = &bytes[index..];
        if remaining
            .get(..8)
            .is_some_and(|value| value.eq_ignore_ascii_case(b"https://"))
        {
            Some((index, 8))
        } else if remaining
            .get(..7)
            .is_some_and(|value| value.eq_ignore_ascii_case(b"http://"))
        {
            Some((index, 7))
        } else {
            None
        }
    })
}

fn inspect_repository_auth_config(ctx: &AppContext) -> Result<()> {
    let (Some(repository), Some(profile)) = (ctx.repository.as_ref(), ctx.profile.as_ref()) else {
        return Ok(());
    };
    let output = git::run_git(
        repository.command_dir(),
        [
            "config",
            "--get-regexp",
            r"^(remote\..*\.(url|pushurl)|url\..*\.(insteadof|pushinsteadof)|http(\..*)?\.extraheader)$",
        ],
    )?;
    if !output.status.success() {
        if output.status.code() == Some(1) {
            return Ok(());
        }
        return Err(AppError::Message(
            "无法检查 Git remote 和 HTTP 认证配置，已停止联网操作".into(),
        ));
    }
    let stdout = Zeroizing::new(output.stdout);
    for line in String::from_utf8_lossy(&stdout).lines() {
        let Some((key, value)) = line.split_once([' ', '\t']) else {
            continue;
        };
        let key = key.to_ascii_lowercase();
        if key.starts_with("remote.")
            && (key.ends_with(".url") || key.ends_with(".pushurl"))
            && contains_https_password_for_host(value.trim_start(), Some(&profile.host))
        {
            return Err(unsafe_https_url_error());
        }
        if rewrite_base_url(&key)
            .is_some_and(|url| contains_https_password_for_host(url, Some(&profile.host)))
        {
            return Err(unsafe_https_url_error());
        }
        if is_http_extra_header_key(&key)
            && http_extra_header_applies_to_host(&key, &profile.host)
            && is_authorization_header(value.trim_start())
        {
            return Err(authorization_header_error());
        }
    }
    Ok(())
}

fn unsafe_https_url_error() -> AppError {
    AppError::Message(
        "检测到 HTTPS URL 内嵌用户名和密码；Git 会因此跳过所选 Profile 的 credential helper，已停止操作"
            .into(),
    )
}

fn authorization_header_error() -> AppError {
    AppError::Message(
        "检测到 Git HTTP Authorization extraHeader；它会绕过所选 Profile 的凭据，已停止操作".into(),
    )
}

fn identity_override_warning(
    ctx: &AppContext,
    args: &[String],
    operation: &GitOperation,
) -> Option<String> {
    if !operation.writes_identity() {
        return None;
    }
    let profile = ctx.profile.as_ref()?;
    let mut author_name = profile.git_name.clone();
    let mut author_email = profile.git_email.clone();
    let mut committer_name = profile.git_name.clone();
    let mut committer_email = profile.git_email.clone();
    let mut sources = Vec::new();

    let mut config_overrides = command_line_git_config(args);
    config_overrides.extend(command_line_git_config(&operation.arguments));
    for item in config_overrides {
        match item.key.as_str() {
            "user.name" => {
                author_name.clone_from(&item.value);
                committer_name = item.value;
                push_unique(&mut sources, item.source);
            }
            "user.email" => {
                author_email.clone_from(&item.value);
                committer_email = item.value;
                push_unique(&mut sources, item.source);
            }
            _ => {}
        }
    }

    apply_identity_environment("GIT_AUTHOR_NAME", &mut author_name, &mut sources);
    apply_identity_environment("GIT_AUTHOR_EMAIL", &mut author_email, &mut sources);
    apply_identity_environment("GIT_COMMITTER_NAME", &mut committer_name, &mut sources);
    apply_identity_environment("GIT_COMMITTER_EMAIL", &mut committer_email, &mut sources);

    let mut explicit_author = None;
    let mut reset_author = false;
    let mut reuses_author = false;
    if operation.name == "commit"
        && let Some(subcommand) = git_subcommand_index(&operation.arguments)
    {
        let mut index = subcommand + 1;
        while index < operation.arguments.len() {
            let argument = &operation.arguments[index];
            let author = if argument == "--author" {
                index += 1;
                operation.arguments.get(index).map(String::as_str)
            } else {
                argument.strip_prefix("--author=")
            };
            if let Some(author) = author {
                push_unique(&mut sources, "commit --author".into());
                explicit_author = Some(author.to_owned());
            } else if argument == "--reset-author" {
                push_unique(&mut sources, "commit --reset-author".into());
                reset_author = true;
            } else if is_commit_author_reuse_option(argument) {
                push_unique(&mut sources, format!("commit {argument}"));
                reuses_author = true;
            }
            index += 1;
        }
    }

    let mut unknown_author = false;
    let has_explicit_author = explicit_author.is_some();
    if let Some(author) = explicit_author.as_deref() {
        if let Ok(identity) = git::parse_identity(author) {
            author_name = identity.name;
            author_email = identity.email;
        } else {
            unknown_author = true;
        }
    } else if reset_author {
        author_name.clone_from(&committer_name);
        author_email.clone_from(&committer_email);
    } else if reuses_author {
        unknown_author = true;
    }

    if sources.is_empty() {
        return None;
    }
    let configured = format!("{} <{}>", profile.git_name, profile.git_email);
    let author = if unknown_author && reuses_author && !has_explicit_author {
        "由被复用的提交决定（无法预先解析）".into()
    } else if unknown_author {
        "由 commit --author 决定（无法预先解析）".into()
    } else {
        format!("{author_name} <{author_email}>")
    };
    let committer = format!("{committer_name} <{committer_email}>");
    Some(format!(
        "ghis: 警告：检测到显式身份覆盖（{}）；配置身份={configured}；推测实际身份：作者={author}，提交者={committer}；实际身份可能与配置身份不同",
        sources.join("、")
    ))
}

fn is_commit_author_reuse_option(argument: &str) -> bool {
    argument == "--amend"
        || argument == "-C"
        || argument == "-c"
        || (argument.starts_with("-C") && argument.len() > 2)
        || (argument.starts_with("-c") && argument.len() > 2)
        || argument == "--reuse-message"
        || argument.starts_with("--reuse-message=")
        || argument == "--reedit-message"
        || argument.starts_with("--reedit-message=")
}

fn apply_identity_environment(name: &str, target: &mut String, sources: &mut Vec<String>) {
    if let Some(value) = std::env::var_os(name) {
        *target = value.to_string_lossy().into_owned();
        push_unique(sources, format!("环境变量 {name}"));
    }
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::process::Command;
    use tempfile::tempdir;

    fn profile() -> Profile {
        Profile {
            host: "github.com".into(),
            login: "alice".into(),
            git_name: "Alice Example".into(),
            git_email: "alice@example.test".into(),
            ..Profile::default()
        }
    }

    fn test_context(display_identity: config::DisplayIdentity) -> AppContext {
        let mut config = Config::default();
        config.behavior.display_identity = display_identity;
        AppContext {
            paths: ConfigPaths::from_bases("config", "cache", "state"),
            config,
            repository: None,
            remote: None,
            resolution: ProfileResolution {
                profile: Some("personal".into()),
                source: ResolutionSource::Explicit,
                candidates: vec!["personal".into()],
                warnings: Vec::new(),
            },
            profile: Some(profile()),
            identities: None,
            warnings: Vec::new(),
        }
    }

    fn git_identity(name: &str, email: &str) -> git::GitIdentity {
        git::GitIdentity {
            name: name.into(),
            email: email.into(),
            raw: format!("{name} <{email}> 0 +0000"),
        }
    }

    #[test]
    fn identity_banner_policy_requires_stderr_terminal() {
        let sensitive = test_context(config::DisplayIdentity::SensitiveCommands);
        assert!(!should_display_identity_banner(&sensitive, true, false));
        assert!(!should_display_identity_banner(&sensitive, false, true));
        assert!(should_display_identity_banner(&sensitive, true, true));

        let always = test_context(config::DisplayIdentity::Always);
        assert!(!should_display_identity_banner(&always, true, false));
        assert!(should_display_identity_banner(&always, false, true));

        let never = test_context(config::DisplayIdentity::Never);
        assert!(!should_display_identity_banner(&never, true, false));
        assert!(!should_display_identity_banner(&never, true, true));
    }

    #[test]
    fn hook_identity_warning_is_separate_from_profile_banner() {
        let mut context = test_context(config::DisplayIdentity::SensitiveCommands);
        context.identities = Some(EffectiveIdentities {
            author: git_identity("Actual Author", "author@example.test"),
            committer: git_identity("Actual Committer", "committer@example.test"),
        });

        let warning = context.hook_identity_warning().expect("identity warning");
        assert!(warning.starts_with("ghis: 警告："));
        assert!(warning.contains("实际作者=Actual Author <author@example.test>"));
        assert!(warning.contains("实际提交者=Actual Committer <committer@example.test>"));
        assert!(!warning.contains("GHIS Profile:"));

        context.identities = Some(EffectiveIdentities {
            author: git_identity("Alice Example", "alice@example.test"),
            committer: git_identity("Alice Example", "alice@example.test"),
        });
        assert_eq!(context.hook_identity_warning(), None);
        context.identities = None;
        assert_eq!(context.hook_identity_warning(), None);
    }

    #[test]
    fn explicit_signing_key_does_not_reuse_ssh_authentication_fingerprint() {
        let mut profile = profile();
        profile.ssh = Some(config::SshProfile {
            mode: config::SshMode::OnePassword,
            public_key: Some(PathBuf::from("/keys/authentication.pub")),
            fingerprint: Some("SHA256:authentication".into()),
            agent_socket: Some(PathBuf::from("/tmp/agent.sock")),
            ..config::SshProfile::default()
        });
        profile.signing.enabled = true;

        assert_eq!(
            profile_signing_fingerprint(&profile).as_deref(),
            Some("SHA256:authentication")
        );
        profile.signing.signing_key = Some("/keys/signing.pub".into());
        assert_eq!(profile_signing_fingerprint(&profile), None);
    }

    #[test]
    fn runtime_signing_accepts_every_supported_inline_key_shape() {
        for value in [
            "ssh-ed25519 AAAA inline",
            "ecdsa-sha2-nistp256 AAAA inline",
            "sk-ssh-ed25519@openssh.com AAAA inline",
            "rsa-sha2-512 AAAA inline",
        ] {
            let mut profile = profile();
            profile.signing.signing_key = Some(format!("key::{value}"));
            assert_eq!(
                profile_public_key(&profile).unwrap().as_deref(),
                Some(value)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn binding_uses_fragment_without_global_identity_changes() {
        let temp = tempdir().expect("temporary directory");
        let repository_path = temp.path().join("repository");
        let init = Command::new("git")
            .args(["init", "-q"])
            .arg(&repository_path)
            .status()
            .expect("git init");
        assert!(init.success());
        let remote = Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/alice/example.git",
            ])
            .current_dir(&repository_path)
            .status()
            .expect("git remote");
        assert!(remote.success());
        let local_name = Command::new("git")
            .args(["config", "--local", "user.name", "Local Identity"])
            .current_dir(&repository_path)
            .status()
            .expect("set local identity");
        assert!(local_name.success());

        let paths = ConfigPaths::from_bases(
            temp.path().join("config"),
            temp.path().join("cache"),
            temp.path().join("state"),
        );
        let mut config = Config::default();
        config.profiles.insert("personal".into(), profile());
        let context = AppContext::from_config(paths, config, &repository_path, Some("personal"))
            .expect("context");
        bind_repository(&context, "personal").expect("bind");

        let name = Command::new("git")
            .args(["config", "--includes", "--get", "user.name"])
            .current_dir(&repository_path)
            .output()
            .expect("read identity");
        assert_eq!(
            String::from_utf8_lossy(&name.stdout).trim(),
            "Alice Example"
        );
        let profile_id = repo::local_config(
            context.repository.as_ref().expect("repository"),
            PROFILE_CONFIG_KEY,
        )
        .expect("config")
        .expect("binding");
        assert_eq!(profile_id, "personal");
        let helpers = Command::new("git")
            .args([
                "config",
                "--worktree",
                "--get-all",
                "credential.https://github.com.helper",
            ])
            .current_dir(&repository_path)
            .output()
            .expect("read helper");
        let helpers = String::from_utf8_lossy(&helpers.stdout);
        assert!(helpers.contains("credential-helper"));
        assert!(helpers.contains("--config"));
        assert!(helpers.contains(&context.paths.config_file.to_string_lossy().to_string()));
        if git::supports_named_hooks() {
            let hook = Command::new("git")
                .args([
                    "config",
                    "--worktree",
                    "--get",
                    "hook.ghis-pre-push.command",
                ])
                .current_dir(&repository_path)
                .output()
                .expect("read named hook");
            let hook = String::from_utf8_lossy(&hook.stdout);
            assert!(hook.contains("--config"));
            assert!(hook.contains(&context.paths.config_file.to_string_lossy().to_string()));
        }

        assert!(!repository_binding_needs_repair(&context).expect("complete binding"));
        let repository = context.repository.as_ref().expect("repository");
        git::remove_credential_helper(repository, "github.com").expect("remove helper");
        assert!(repository_binding_needs_repair(&context).expect("missing helper"));
        bind_repository(&context, "personal").expect("repair helper");
        assert!(!repository_binding_needs_repair(&context).expect("repaired helper"));
        if git::supports_named_hooks() {
            git::remove_named_hooks(repository).expect("remove hooks");
            assert!(repository_binding_needs_repair(&context).expect("missing hooks"));
            bind_repository(&context, "personal").expect("repair hooks");
            assert!(!repository_binding_needs_repair(&context).expect("repaired hooks"));
        }

        let mut updated_config = context.config.clone();
        updated_config
            .profiles
            .get_mut("personal")
            .expect("profile")
            .git_email = "updated@example.test".into();
        let updated_context = AppContext::from_config(
            context.paths.clone(),
            updated_config,
            &repository_path,
            None,
        )
        .expect("updated context");
        assert!(repository_binding_needs_repair(&updated_context).expect("stale fragment"));
        bind_repository(&updated_context, "personal").expect("repair stale fragment");
        assert!(!repository_binding_needs_repair(&updated_context).expect("current fragment"));
        let email = Command::new("git")
            .args(["config", "--includes", "--get", "user.email"])
            .current_dir(&repository_path)
            .output()
            .expect("read updated identity");
        assert_eq!(
            String::from_utf8_lossy(&email.stdout).trim(),
            "updated@example.test"
        );

        unbind_repository(&updated_context).expect("unbind");
        let name = Command::new("git")
            .args(["config", "--includes", "--get", "user.name"])
            .current_dir(&repository_path)
            .output()
            .expect("read restored identity");
        assert_eq!(
            String::from_utf8_lossy(&name.stdout).trim(),
            "Local Identity"
        );
        assert!(
            repo::local_config(
                context.repository.as_ref().expect("repository"),
                PROFILE_CONFIG_KEY,
            )
            .expect("config")
            .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn linked_worktrees_keep_profile_bindings_isolated() {
        let temp = tempdir().expect("temporary directory");
        let main = temp.path().join("main");
        let first_worktree = temp.path().join("first-worktree");
        let second_worktree = temp.path().join("second-worktree");
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&main)
                .status()
                .expect("git init")
                .success()
        );
        assert!(
            Command::new("git")
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.test",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    "initial",
                ])
                .current_dir(&main)
                .status()
                .expect("initial commit")
                .success()
        );
        for (branch, path) in [
            ("first-branch", &first_worktree),
            ("second-branch", &second_worktree),
        ] {
            assert!(
                Command::new("git")
                    .args(["worktree", "add", "-q", "-b", branch])
                    .arg(path)
                    .current_dir(&main)
                    .status()
                    .expect("add linked worktree")
                    .success()
            );
        }

        let paths = ConfigPaths::from_bases(
            temp.path().join("config"),
            temp.path().join("cache"),
            temp.path().join("state"),
        );
        let mut first_profile = profile();
        first_profile.git_name = "First Identity".into();
        first_profile.git_email = "first@example.test".into();
        let mut second_profile = profile();
        second_profile.login = "bob".into();
        second_profile.git_name = "Second Identity".into();
        second_profile.git_email = "second@example.test".into();
        let mut config = Config::default();
        config.profiles.insert("first".into(), first_profile);
        config.profiles.insert("second".into(), second_profile);

        let first_context = AppContext::from_config(
            paths.clone(),
            config.clone(),
            &first_worktree,
            Some("first"),
        )
        .expect("first context");
        bind_repository(&first_context, "first").expect("bind first worktree");
        let second_context = AppContext::from_config(
            paths.clone(),
            config.clone(),
            &second_worktree,
            Some("second"),
        )
        .expect("second context");
        bind_repository(&second_context, "second").expect("bind second worktree");

        let extension = Command::new("git")
            .args([
                "config",
                "--local",
                "--bool",
                "--get",
                "extensions.worktreeConfig",
            ])
            .current_dir(&main)
            .output()
            .expect("read worktreeConfig extension");
        assert!(extension.status.success());
        assert_eq!(String::from_utf8_lossy(&extension.stdout).trim(), "true");
        let shared_marker = Command::new("git")
            .args(["config", "--local", "--get", PROFILE_CONFIG_KEY])
            .current_dir(&main)
            .output()
            .expect("read shared profile marker");
        assert_eq!(shared_marker.status.code(), Some(1));
        let shared_include = Command::new("git")
            .args(["config", "--local", "--get-regexp", r"^includeIf\."])
            .current_dir(&main)
            .output()
            .expect("read shared profile includes");
        assert_eq!(shared_include.status.code(), Some(1));

        for (context, expected_id, expected_email) in [
            (&first_context, "first", "first@example.test"),
            (&second_context, "second", "second@example.test"),
        ] {
            let repository = context.repository.as_ref().expect("repository");
            assert_eq!(
                repo::local_config(repository, PROFILE_CONFIG_KEY)
                    .expect("read worktree binding")
                    .as_deref(),
                Some(expected_id)
            );
            let marker = Command::new("git")
                .args(["config", "--worktree", "--get", PROFILE_CONFIG_KEY])
                .current_dir(repository.command_dir())
                .output()
                .expect("read config.worktree marker");
            assert_eq!(String::from_utf8_lossy(&marker.stdout).trim(), expected_id);
            let include = Command::new("git")
                .args(["config", "--worktree", "--get-regexp", r"^includeIf\."])
                .current_dir(repository.command_dir())
                .output()
                .expect("read config.worktree include");
            assert!(include.status.success());
            assert!(String::from_utf8_lossy(&include.stdout).contains(".gitconfig"));
            let email = Command::new("git")
                .args(["config", "--includes", "--get", "user.email"])
                .current_dir(repository.command_dir())
                .output()
                .expect("read effective worktree identity");
            assert_eq!(
                String::from_utf8_lossy(&email.stdout).trim(),
                expected_email
            );
        }

        unbind_repository(&first_context).expect("unbind first worktree");
        assert!(
            repo::local_config(
                first_context.repository.as_ref().expect("first repository"),
                PROFILE_CONFIG_KEY,
            )
            .expect("read removed first binding")
            .is_none()
        );
        let removed_include = Command::new("git")
            .args(["config", "--worktree", "--get-regexp", r"^includeIf\."])
            .current_dir(&first_worktree)
            .output()
            .expect("read removed first include");
        assert_eq!(removed_include.status.code(), Some(1));
        assert_eq!(
            repo::local_config(
                second_context
                    .repository
                    .as_ref()
                    .expect("second repository"),
                PROFILE_CONFIG_KEY,
            )
            .expect("read retained second binding")
            .as_deref(),
            Some("second")
        );
        let second_email = Command::new("git")
            .args(["config", "--includes", "--get", "user.email"])
            .current_dir(&second_worktree)
            .output()
            .expect("read retained second identity");
        assert_eq!(
            String::from_utf8_lossy(&second_email.stdout).trim(),
            "second@example.test"
        );
    }

    #[test]
    fn git_subcommand_skips_global_options() {
        let args = vec![
            "-C".into(),
            "nested".into(),
            "-c".into(),
            "user.name=Test".into(),
            "commit".into(),
        ];
        assert_eq!(git_subcommand(&args), "commit");
    }

    #[test]
    fn commit_signing_policy_rejects_no_gpg_sign_before_launching_git() {
        let args = vec![
            "commit".into(),
            "--allow-empty".into(),
            "--no-gpg-sign".into(),
        ];
        let operation = GitOperation {
            name: "commit".into(),
            conservative_sensitive: false,
            uninspectable_alias: false,
            arguments: args.clone(),
        };

        let error = validate_commit_signing_policy(true, &args, &operation)
            .expect_err("enabled signing must reject --no-gpg-sign");
        let message = error.to_string();
        assert!(message.contains("--no-gpg-sign"));
        assert!(message.contains("移除"));
    }

    #[test]
    fn commit_signing_policy_rejects_false_global_config_overrides() {
        for value in ["false", "no", "off", "0", ""] {
            let args = vec![
                "-c".into(),
                format!("commit.gpgSign={value}"),
                "commit".into(),
            ];
            let operation = GitOperation {
                name: "commit".into(),
                conservative_sensitive: false,
                uninspectable_alias: false,
                arguments: vec!["commit".into()],
            };

            let error = validate_commit_signing_policy(true, &args, &operation)
                .expect_err("enabled signing must reject a false config override");
            let message = error.to_string();
            assert!(message.contains("commit.gpgSign"));
            assert!(!message.contains(value) || value.is_empty());
        }
    }

    #[test]
    fn commit_signing_policy_allows_disabled_profiles_and_non_commit_commands() {
        let bypass_args = vec![
            "-c".into(),
            "commit.gpgSign=false".into(),
            "commit".into(),
            "--no-gpg-sign".into(),
        ];
        let commit = GitOperation {
            name: "commit".into(),
            conservative_sensitive: false,
            uninspectable_alias: false,
            arguments: vec!["commit".into(), "--no-gpg-sign".into()],
        };
        validate_commit_signing_policy(false, &bypass_args, &commit)
            .expect("disabled signing keeps normal Git behavior");

        let status = GitOperation {
            name: "status".into(),
            conservative_sensitive: false,
            uninspectable_alias: false,
            arguments: vec!["status".into(), "--no-gpg-sign".into()],
        };
        validate_commit_signing_policy(true, &bypass_args, &status)
            .expect("non-commit commands are unaffected");
    }

    #[test]
    fn commit_signing_policy_ignores_option_values_and_pathspecs() {
        for args in [
            vec!["commit", "-m", "--no-gpg-sign"],
            vec!["commit", "--", "--no-gpg-sign"],
        ] {
            let arguments = args.into_iter().map(String::from).collect::<Vec<_>>();
            let operation = GitOperation {
                name: "commit".into(),
                conservative_sensitive: false,
                uninspectable_alias: false,
                arguments: arguments.clone(),
            };
            validate_commit_signing_policy(true, &arguments, &operation)
                .expect("non-option values must not be rejected");
        }
    }

    #[test]
    fn local_git_classifier_allows_only_proven_read_operations() {
        for args in [
            vec!["status"],
            vec!["log", "-1"],
            vec!["-C", "nested", "diff"],
            vec!["branch", "--list"],
            vec!["remote", "-v"],
            vec!["config", "--get", "user.name"],
            vec!["--", "show", "HEAD"],
        ] {
            let args = args.into_iter().map(String::from).collect::<Vec<_>>();
            assert!(is_local_read_only_git(&args), "{args:?}");
        }

        for args in [
            vec!["commit", "-m", "message"],
            vec!["push"],
            vec!["fetch"],
            vec!["submodule", "update"],
            vec!["branch", "new-branch"],
            vec!["branch", "--delete", "old-branch"],
            vec!["unknown-command"],
            vec!["-c", "alias.st=status", "st"],
        ] {
            let args = args.into_iter().map(String::from).collect::<Vec<_>>();
            assert!(!is_local_read_only_git(&args), "{args:?}");
        }
    }

    #[test]
    fn git_policy_is_inserted_after_global_options_and_before_double_dash() {
        let args = vec![
            "-C".into(),
            "nested".into(),
            "-c".into(),
            "credential.helper=!other".into(),
            "--".into(),
            "credential".into(),
            "fill".into(),
        ];
        assert_eq!(git_policy_insertion_index(&args), 4);
    }

    #[test]
    fn detects_https_passwords_without_confusing_usernames_or_unicode() {
        assert!(contains_https_password_for_host(
            "https://alice:secret@github.com/acme/project.git",
            None
        ));
        assert!(contains_https_password_for_host(
            "前缀 HTTPS://alice:@github.com/acme/project.git",
            None
        ));
        assert!(!contains_https_password_for_host(
            "https://alice@github.com/acme/project.git",
            None
        ));
        assert!(!contains_https_password_for_host(
            "ssh://git@github.com/acme/project.git",
            None
        ));
        assert!(!contains_https_password_for_host(
            "https://alice:secret@gitlab.example/acme/project.git",
            Some("github.com")
        ));
        assert!(contains_https_password_for_host(
            "https://alice:secret@github.com:443/acme/project.git",
            Some("github.com")
        ));
    }

    #[test]
    fn recognizes_authentication_config_overrides() {
        assert!(is_credential_helper_key("credential.helper"));
        assert!(is_credential_helper_key(
            "credential.https://github.com.helper"
        ));
        assert!(is_authorization_header("Authorization: Basic secret"));
        assert!(is_authorization_header(
            "proxy-authorization: Bearer secret"
        ));
        assert!(!is_authorization_header("User-Agent: ghis-test"));
        assert!(http_extra_header_applies_to_host(
            "http.https://github.com/.extraheader",
            "github.com"
        ));
        assert!(!http_extra_header_applies_to_host(
            "http.https://gitlab.example/.extraheader",
            "github.com"
        ));
    }

    #[test]
    fn gh_target_validation_rejects_cross_host_selectors() {
        let arguments = [
            vec![
                "api".into(),
                "--hostname=other.example".into(),
                "user".into(),
            ],
            vec![
                "pr".into(),
                "view".into(),
                "-R".into(),
                "other.example/acme/project".into(),
            ],
            vec![
                "repo".into(),
                "clone".into(),
                "https://other.example/acme/project".into(),
            ],
            vec![
                "pr".into(),
                "view".into(),
                "https://other.example/acme/project/pull/1".into(),
            ],
            vec![
                "repo".into(),
                "view".into(),
                "--web".into(),
                "other.example/acme/project".into(),
            ],
            vec![
                "repo".into(),
                "archive".into(),
                "--yes".into(),
                "enterprise/acme/project".into(),
            ],
            vec![
                "repo".into(),
                "clone".into(),
                "git+ssh://other.example/acme/project".into(),
            ],
            vec![
                "repo".into(),
                "clone".into(),
                "other.example:acme/project".into(),
            ],
            vec![
                "pr".into(),
                "revert".into(),
                "https://other.example/acme/project/pull/2".into(),
            ],
            vec![
                "co".into(),
                "https://other.example/acme/project/pull/3".into(),
            ],
            vec![
                "issue".into(),
                "transfer".into(),
                "1".into(),
                "other.example/acme/destination".into(),
            ],
            vec![
                "repo".into(),
                "sync".into(),
                "acme/destination".into(),
                "--source=other.example/acme/source".into(),
            ],
        ];
        for args in arguments {
            assert!(validate_gh_target_host("github.com", &args, None, None).is_err());
        }
        assert!(
            validate_gh_target_host(
                "github.com",
                &["pr".into(), "view".into()],
                Some(std::ffi::OsStr::new("other.example/acme/project")),
                None,
            )
            .is_err()
        );
        assert!(
            validate_gh_target_host(
                "github.com",
                &[
                    "pr".into(),
                    "comment".into(),
                    "1".into(),
                    "--body".into(),
                    "https://github.com/acme/other/pull/2".into(),
                ],
                Some(std::ffi::OsStr::new("other.example/acme/project")),
                Some("github.com"),
            )
            .is_err(),
            "正文 URL 不能覆盖真正生效的 GH_REPO"
        );
        assert!(
            validate_gh_target_host(
                "github.com",
                &[
                    "pr".into(),
                    "comment".into(),
                    "1".into(),
                    "--body".into(),
                    "-Rgithub.com/acme/other".into(),
                ],
                Some(std::ffi::OsStr::new("other.example/acme/project")),
                Some("github.com"),
            )
            .is_err(),
            "正文里的 -R 文本不能变成仓库选择器"
        );
    }

    #[test]
    fn gh_target_validation_accepts_the_profile_host_and_unqualified_repos() {
        let args = vec![
            "pr".into(),
            "view".into(),
            "--repo=GitHub.com/acme/project".into(),
        ];
        validate_gh_target_host("github.com", &args, None, None).expect("matching host");
        let inherited = validate_gh_target_host(
            "github.com",
            &["pr".into(), "view".into()],
            Some(std::ffi::OsStr::new("acme/project")),
            Some("other.example"),
        )
        .expect("unqualified GH_REPO");
        assert_eq!(
            inherited.repository_environment.as_deref(),
            Some("acme/project")
        );
        validate_gh_target_host(
            "git.example:8443",
            &[
                "repo".into(),
                "clone".into(),
                "https://git.example:8443/acme/project".into(),
            ],
            None,
            Some("other.example"),
        )
        .expect("matching URL host and port");
        assert_eq!(
            gh_repository_selector_host("https://git.example:8443/acme/project").as_deref(),
            Some("git.example:8443")
        );
        validate_gh_target_host(
            "github.com",
            &[
                "repo".into(),
                "edit".into(),
                "github.com/acme/project".into(),
                "--homepage".into(),
                "https://docs.other.example/acme/project".into(),
            ],
            None,
            Some("other.example"),
        )
        .expect("a homepage URL is content, not a repository target");

        assert!(
            validate_gh_target_host(
                "github.com",
                &["pr".into(), "list".into()],
                None,
                Some("enterprise.example"),
            )
            .is_err()
        );

        validate_gh_target_host(
            "github.com",
            &["api".into(), "--hostname=github.com".into(), "user".into()],
            Some(std::ffi::OsStr::new("enterprise.example/acme/project")),
            Some("enterprise.example"),
        )
        .expect("explicit hostname overrides repository context");
        validate_gh_target_host(
            "github.com",
            &[
                "pr".into(),
                "view".into(),
                "--comments".into(),
                "https://github.com/acme/project/pull/42".into(),
            ],
            Some(std::ffi::OsStr::new("enterprise.example/acme/project")),
            Some("enterprise.example"),
        )
        .expect("pull request URL overrides repository context");
        validate_gh_target_host(
            "github.com",
            &["api".into(), "user".into()],
            Some(std::ffi::OsStr::new("enterprise.example/acme/project")),
            Some("enterprise.example"),
        )
        .expect("api does not consume repository context");
        validate_gh_target_host(
            "github.com",
            &[
                "repo".into(),
                "clone".into(),
                "github.com/acme/project".into(),
                "docs.other.example/acme/project".into(),
            ],
            None,
            Some("enterprise.example"),
        )
        .expect("clone destination is a path, not a second repository");
        assert!(
            validate_gh_target_host(
                "github.com",
                &[
                    "issue".into(),
                    "edit".into(),
                    "--add-label".into(),
                    "bug".into(),
                    "https://enterprise.example/acme/project/issues/7".into(),
                ],
                None,
                Some("github.com"),
            )
            .is_err()
        );
    }

    #[test]
    fn managed_helper_cleanup_preserves_unowned_ordered_values() {
        let managed = "!'/usr/bin/ghis' --config '/tmp/config' credential-helper";
        let values = vec![
            String::new(),
            "!user-before".into(),
            String::new(),
            managed.into(),
            String::new(),
            "!user-after".into(),
        ];
        assert_eq!(
            remove_managed_helper_entries(&values),
            Some(vec![
                String::new(),
                "!user-before".into(),
                String::new(),
                "!user-after".into(),
            ])
        );
        assert_eq!(
            remove_managed_helper_entries(&[String::new(), "!user-only".into()]),
            None
        );
        assert!(!is_managed_credential_helper(
            "!'/opt/acme' --config '/tmp/acme' credential-helper"
        ));
        assert!(!is_managed_credential_helper(
            "!'/opt/ghis-helper' --config '/tmp/acme' credential-helper"
        ));
        assert!(!is_managed_credential_helper(
            "!'/usr/bin/ghis' --config credential-helper"
        ));
    }

    #[test]
    fn fragment_paths_are_collision_free_and_sync_removes_stale_files() {
        let temp = tempdir().expect("temporary directory");
        let paths = ConfigPaths::from_bases(
            temp.path().join("config"),
            temp.path().join("cache"),
            temp.path().join("state"),
        );
        assert_ne!(
            profile_fragment_path(&paths, "work/name"),
            profile_fragment_path(&paths, "work_name")
        );

        let mut config = Config::default();
        config.profiles.insert("personal".into(), profile());
        let written = sync_fragments(&paths, &config).expect("initial sync");
        assert_eq!(written.len(), 1);
        assert!(written[0].is_file());

        config.profiles.clear();
        assert!(
            sync_fragments(&paths, &config)
                .expect("remove stale fragment")
                .is_empty()
        );
        assert!(!written[0].exists());
    }

    #[test]
    fn stale_fragment_sync_uses_the_authoritative_saved_config() {
        let temp = tempdir().expect("temporary directory");
        let paths = ConfigPaths::from_bases(
            temp.path().join("config"),
            temp.path().join("cache"),
            temp.path().join("state"),
        );
        let mut stale = Config::default();
        stale.profiles.insert("personal".into(), profile());
        let mut current = stale.clone();
        current
            .profiles
            .get_mut("personal")
            .expect("profile")
            .git_email = "current@example.test".into();
        current
            .save(&paths.config_file)
            .expect("save current config");

        sync_fragments(&paths, &stale).expect("sync stale snapshot");
        let fragment =
            fs::read_to_string(profile_fragment_path(&paths, "personal")).expect("read fragment");
        assert!(fragment.contains("current@example.test"));
        assert!(!fragment.contains("alice@example.com"));
    }

    #[test]
    fn stale_single_fragment_writer_is_rejected_without_overwriting() {
        let temp = tempdir().expect("temporary directory");
        let paths = ConfigPaths::from_bases(
            temp.path().join("config"),
            temp.path().join("cache"),
            temp.path().join("state"),
        );
        let stale_profile = profile();
        let mut current_profile = stale_profile.clone();
        current_profile.git_email = "current@example.test".into();
        let mut current = Config::default();
        current.profiles.insert("personal".into(), current_profile);
        current
            .save(&paths.config_file)
            .expect("save current config");
        sync_fragments(&paths, &current).expect("initial sync");

        let error = write_profile_fragment(&paths, "personal", &stale_profile)
            .expect_err("stale profile must be rejected");
        assert!(error.to_string().contains("其他进程修改"));
        let fragment =
            fs::read_to_string(profile_fragment_path(&paths, "personal")).expect("read fragment");
        assert!(fragment.contains("current@example.test"));
    }

    #[test]
    fn removed_profile_cannot_be_recreated_by_a_stale_writer() {
        let temp = tempdir().expect("temporary directory");
        let paths = ConfigPaths::from_bases(
            temp.path().join("config"),
            temp.path().join("cache"),
            temp.path().join("state"),
        );
        let removed = profile();
        Config::default()
            .save(&paths.config_file)
            .expect("save config without profile");

        let error = write_profile_fragment(&paths, "personal", &removed)
            .expect_err("removed profile must not be recreated");
        assert!(error.to_string().contains("已被删除"));
        assert!(!profile_fragment_path(&paths, "personal").exists());
    }
}
