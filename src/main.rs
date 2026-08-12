use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use ghis::app::{self, AppContext};
use ghis::config::{
    Config, ConfigPaths, CredentialFailurePolicy, DisplayIdentity, Profile, Rule, SigningProfile,
    SshMode, SshProfile, SshUnmanagedPolicy, UnresolvedPolicy,
};
use ghis::{credential, diagnostics, github, shell, signing, tui};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

#[derive(Debug, Parser)]
#[command(
    name = "ghis",
    version = ghis::VERSION,
    long_version = ghis::LONG_VERSION,
    about = "按仓库切换 Git 和 GitHub 身份"
)]
struct Cli {
    /// 临时指定 profile，优先于仓库绑定和规则
    #[arg(long, global = true, env = "GHIS_PROFILE")]
    profile: Option<String>,
    /// 指定用户配置文件
    #[arg(long, global = true, env = "GHIS_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// 打开中文 Vim 风格终端界面
    Tui,
    /// 显示当前仓库和有效身份
    Status(StatusArgs),
    /// 从 gh CLI 发现已有账号
    Discover(JsonArgs),
    /// 管理 profile
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// 将 profile 绑定到仓库
    Use {
        profile: String,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// 解除当前仓库绑定
    Unbind {
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// 管理自动匹配规则
    Rule {
        #[command(subcommand)]
        command: RuleCommand,
    },
    /// 读取或修改常用 behavior 设置
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// 重建 profile Git 配置片段
    Sync,
    /// 检查依赖、账号、仓库和 1Password/SSH 状态
    Doctor(DoctorArgs),
    /// 安装 zsh 包装器
    Setup(SetupArgs),
    /// 从 .zshrc 移除 ghis 管理块
    Uninstall,
    /// 输出 shell 初始化脚本
    Init { shell: ShellKind },
    /// 输出 shell 补全脚本
    Completion { shell: ShellKind },
    /// 由 zsh wrapper 调用，透明执行真实 git
    #[command(trailing_var_arg = true)]
    Git(Passthrough),
    /// 由 zsh wrapper 调用，为真实 gh 注入当前 profile token
    #[command(trailing_var_arg = true)]
    Gh(Passthrough),
    /// Git credential helper 内部命令
    #[command(hide = true)]
    CredentialHelper { action: String },
    /// managed SSH 无可用 Agent 时使用的拒绝命令
    #[command(hide = true, trailing_var_arg = true)]
    SshGuard(Passthrough),
    /// Git hook 内部命令
    #[command(hide = true, trailing_var_arg = true)]
    Hook {
        #[arg(long)]
        hook: String,
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ShellKind {
    Zsh,
}

#[derive(Debug, Args, Default)]
struct JsonArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args, Default)]
struct StatusArgs {
    #[arg(long)]
    json: bool,
    /// 供 chpwd hook 使用：不访问网络，也不输出正文
    #[arg(long, hide = true)]
    shell: bool,
    #[arg(long, hide = true)]
    quiet: bool,
}

#[derive(Debug, Args, Default)]
struct DoctorArgs {
    #[arg(long)]
    json: bool,
    /// 显式访问 GitHub API，核对当前 Profile 的 SSH signing 公钥
    #[arg(long, visible_alias = "check-github-signing-keys")]
    check_github_signing_key: bool,
}

#[derive(Debug, Args)]
struct SetupArgs {
    /// 只打印将要写入的 zsh 初始化脚本
    #[arg(long)]
    print: bool,
    /// 跳过修改 zsh 启动文件前的确认，供脚本安装使用
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct Passthrough {
    #[arg(allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum ProfileCommand {
    List(JsonArgs),
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    Add(ProfileArgs),
    Edit(ProfileEditArgs),
    /// 查看一个 profile 可用的提交邮箱候选
    Mail(MailArgs),
    Remove {
        id: String,
    },
}

#[derive(Debug, Args)]
struct MailArgs {
    id: String,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ProfileArgs {
    id: String,
    #[arg(long, default_value = "github.com")]
    host: String,
    #[arg(long)]
    login: String,
    #[arg(long = "name")]
    git_name: String,
    #[arg(long = "email", conflicts_with = "noreply")]
    git_email: Option<String>,
    /// 使用 GitHub.com 的 ID-based noreply 邮箱
    #[arg(
        short = 'N',
        long = "noreply",
        visible_alias = "github-noreply",
        conflicts_with = "git_email"
    )]
    noreply: bool,
    #[arg(long, value_enum, default_value_t = SshModeArg::External)]
    ssh: SshModeArg,
    #[arg(long)]
    public_key: Option<PathBuf>,
    #[arg(long)]
    fingerprint: Option<String>,
    #[arg(long)]
    agent_socket: Option<PathBuf>,
    #[arg(long)]
    sign: bool,
    #[arg(long)]
    signing_key: Option<String>,
    #[arg(long)]
    signing_program: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ProfileEditArgs {
    id: String,
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    login: Option<String>,
    #[arg(long = "name")]
    git_name: Option<String>,
    #[arg(long = "email", conflicts_with = "noreply")]
    git_email: Option<String>,
    /// 使用 GitHub.com 的 ID-based noreply 邮箱
    #[arg(
        short = 'N',
        long = "noreply",
        visible_alias = "github-noreply",
        conflicts_with = "git_email"
    )]
    noreply: bool,
    #[arg(long, value_enum)]
    ssh: Option<SshModeArg>,
    #[arg(long)]
    public_key: Option<PathBuf>,
    #[arg(long)]
    fingerprint: Option<String>,
    #[arg(long)]
    agent_socket: Option<PathBuf>,
    /// 开启签名；使用 `--sign=false` 可关闭
    #[arg(
        long,
        action = clap::ArgAction::Set,
        num_args = 0..=1,
        default_missing_value = "true"
    )]
    sign: Option<bool>,
    #[arg(long)]
    signing_key: Option<String>,
    #[arg(long)]
    signing_program: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
enum SshModeArg {
    #[default]
    External,
    OnePassword,
    Managed,
}

#[derive(Debug, Subcommand)]
enum RuleCommand {
    List(JsonArgs),
    Add(RuleArgs),
    Edit(RuleArgs),
    Remove { id: String },
}

#[derive(Debug, Args)]
struct RuleArgs {
    id: String,
    #[arg(long)]
    profile: String,
    #[arg(long, default_value_t = 0)]
    priority: i32,
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    owner: Option<String>,
    #[arg(long)]
    repo: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    gitdir: Option<String>,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// 列出可读取和修改的 behavior 设置
    List(JsonArgs),
    Get {
        key: String,
    },
    Set {
        key: String,
        value: String,
    },
}

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("ghis: {error}");
            std::process::exit(2);
        }
    }
}

fn run(cli: Cli) -> app::Result<i32> {
    let command = match cli.command {
        Some(command) => command,
        None if io::stdout().is_terminal() => Commands::Tui,
        None => Commands::Status(StatusArgs::default()),
    };
    match command {
        Commands::Tui => run_tui(cli.config.as_deref(), cli.profile.as_deref()),
        Commands::Status(args) => status(cli.config.as_deref(), cli.profile.as_deref(), args),
        Commands::Discover(args) => discover(cli.config.as_deref(), args.json),
        Commands::Profile { command } => profile_command(cli.config.as_deref(), command),
        Commands::Use { profile, repo } => {
            use_profile(cli.config.as_deref(), &profile, repo.as_deref())
        }
        Commands::Unbind { repo } => unbind(cli.config.as_deref(), repo.as_deref()),
        Commands::Rule { command } => rule_command(cli.config.as_deref(), command),
        Commands::Config { command } => config_command(cli.config.as_deref(), command),
        Commands::Sync => sync(cli.config.as_deref()),
        Commands::Doctor(args) => doctor(
            cli.config.as_deref(),
            cli.profile.as_deref(),
            args.json,
            args.check_github_signing_key,
        ),
        Commands::Setup(args) => setup(args),
        Commands::Uninstall => uninstall(),
        Commands::Init {
            shell: ShellKind::Zsh,
        } => {
            print!("{}", shell::zsh_init_script("ghis"));
            Ok(0)
        }
        Commands::Completion {
            shell: ShellKind::Zsh,
        } => {
            let mut command = Cli::command();
            let mut completion = Vec::new();
            generate(Shell::Zsh, &mut command, "ghis", &mut completion);
            write_stdout(&completion)?;
            Ok(0)
        }
        Commands::Git(args) => {
            let cwd = git_working_directory(&args.args)?;
            app::run_git(
                &args.args,
                cli.config.as_deref(),
                cli.profile.as_deref(),
                &cwd,
            )
        }
        Commands::Gh(args) => app::run_gh(
            &args.args,
            cli.config.as_deref(),
            cli.profile.as_deref(),
            &std::env::current_dir()?,
        ),
        Commands::CredentialHelper { action } => {
            credential_helper(cli.config.as_deref(), cli.profile.as_deref(), &action)
        }
        Commands::SshGuard(_) => {
            eprintln!("ghis: 当前 Profile 没有可用的 SSH Agent；已阻止 SSH 操作");
            Ok(78)
        }
        Commands::Hook {
            hook: hook_name, ..
        } => hook(cli.config.as_deref(), cli.profile.as_deref(), &hook_name),
    }
}

fn load_config(path: Option<&Path>) -> app::Result<(ConfigPaths, Config)> {
    let mut paths = ConfigPaths::discover()?;
    if let Some(path) = path {
        paths.set_config_file(path)?;
    }
    let config = Config::load(&paths.config_file)?;
    Ok((paths, config))
}

fn update_config<T>(
    paths: &ConfigPaths,
    update: impl FnOnce(&mut Config) -> app::Result<T>,
) -> app::Result<(Config, T)> {
    Config::update(&paths.config_file, update)
}

fn context(
    path: Option<&Path>,
    explicit: Option<&str>,
    cwd: impl AsRef<Path>,
) -> app::Result<AppContext> {
    let (paths, config) = load_config(path)?;
    AppContext::from_config(paths, config, cwd, explicit)
}

fn status(path: Option<&Path>, explicit: Option<&str>, args: StatusArgs) -> app::Result<i32> {
    let ctx = context(path, explicit, std::env::current_dir()?)?;
    if args.quiet {
        return Ok(0);
    }
    let report = app::display_status(&ctx);
    if args.json {
        print_json(&report)?;
    } else if args.shell {
        println!(
            "GHIS_PROFILE={}",
            shell::shell_quote(ctx.profile_id().unwrap_or(""))
        );
    } else {
        println!("{}", ctx.identity_banner());
        for warning in &ctx.warnings {
            eprintln!("警告：{warning}");
        }
    }
    Ok(0)
}

fn discover(path: Option<&Path>, json: bool) -> app::Result<i32> {
    let discovery = github::discover_accounts(None)?;
    let cache_warning = match ConfigPaths::discover().and_then(|mut paths| {
        if let Some(path) = path {
            paths.set_config_file(path)?;
        }
        Ok(paths)
    }) {
        Ok(paths) => cache_discovery(&paths, &discovery),
        Err(error) => Some(format!("无法确定 gh 账号缓存位置：{error}")),
    };
    if json {
        #[derive(Serialize)]
        struct Output<'a> {
            schema_version: u32,
            command_succeeded: bool,
            offline: bool,
            accounts: Vec<Account<'a>>,
        }
        #[derive(Serialize)]
        struct Account<'a> {
            host: &'a str,
            login: &'a str,
            active: bool,
            verified: bool,
            error: Option<&'a str>,
        }
        print_json(&Output {
            schema_version: ghis::SCHEMA_VERSION,
            command_succeeded: discovery.command_succeeded,
            offline: discovery.offline,
            accounts: discovery
                .accounts
                .iter()
                .map(|account| Account {
                    host: &account.host,
                    login: &account.login,
                    active: account.active,
                    verified: account.verified,
                    error: account.error.as_deref(),
                })
                .collect(),
        })?;
    } else {
        for account in &discovery.accounts {
            let state = if account.verified {
                "可用"
            } else {
                "未验证"
            };
            let active = if account.active {
                "，gh 当前活动账号"
            } else {
                ""
            };
            println!("{}/{}：{state}{active}", account.host, account.login);
        }
        if discovery.offline {
            eprintln!("警告：当前无法联网验证账号；已保留 gh 中发现的身份。")
        }
    }
    if let Some(warning) = cache_warning {
        eprintln!("警告：{warning}");
    }
    Ok(if discovery.accounts.is_empty() { 1 } else { 0 })
}

fn discovery_cache_path(paths: &ConfigPaths) -> PathBuf {
    paths.cache_dir.join(github::DISCOVERY_CACHE_FILENAME)
}

fn cache_discovery(paths: &ConfigPaths, discovery: &github::GhDiscovery) -> Option<String> {
    github::save_discovery_cache(&discovery_cache_path(paths), discovery)
        .err()
        .map(|error| format!("无法更新 gh 账号缓存：{error}"))
}

fn profile_command(path: Option<&Path>, command: ProfileCommand) -> app::Result<i32> {
    let (paths, config) = load_config(path)?;
    match command {
        ProfileCommand::List(args) => {
            if args.json {
                print_json(&config.profiles)?;
            } else if config.profiles.is_empty() {
                println!("尚未配置 profile。可先运行 `ghis discover`。")
            } else {
                for (id, profile) in &config.profiles {
                    println!(
                        "{id}: {} <{}>，{}/{}",
                        profile.git_name, profile.git_email, profile.host, profile.login
                    );
                }
            }
        }
        ProfileCommand::Show { id, json } => {
            let profile = config
                .profiles
                .get(&id)
                .ok_or_else(|| app::AppError::Message(format!("profile `{id}` 不存在")))?;
            if json {
                print_json(profile)?;
            } else {
                println!(
                    "profile: {id}\n提交身份: {} <{}>\nGitHub: {}/{}\n签名: {}",
                    profile.git_name,
                    profile.git_email,
                    profile.host,
                    profile.login,
                    if profile.signing.enabled {
                        "开启"
                    } else {
                        "关闭"
                    }
                );
            }
        }
        ProfileCommand::Add(args) => {
            let id = args.id.clone();
            let email = resolve_profile_email(
                &args.host,
                &args.login,
                args.git_email.as_deref(),
                args.noreply,
            )?;
            let profile = profile_from_args(args, email);
            let (config, ()) = update_config(&paths, |config| {
                if config.profiles.contains_key(&id) {
                    return Err(app::AppError::Message(format!("profile `{id}` 已存在")));
                }
                config.profiles.insert(id.clone(), profile);
                Ok(())
            })?;
            app::sync_fragments(&paths, &config)?;
            println!("已添加 profile `{id}`。")
        }
        ProfileCommand::Edit(args) => {
            let id = args.id.clone();
            let mut args = args;
            if args.noreply {
                let profile = config
                    .profiles
                    .get(&id)
                    .ok_or_else(|| app::AppError::Message(format!("profile `{id}` 不存在")))?;
                let host = args.host.as_deref().unwrap_or(&profile.host);
                let login = args.login.as_deref().unwrap_or(&profile.login);
                args.git_email = Some(resolve_profile_email(host, login, None, true)?);
            }
            let (config, ()) = update_config(&paths, |config| {
                let profile = config
                    .profiles
                    .get_mut(&id)
                    .ok_or_else(|| app::AppError::Message(format!("profile `{id}` 不存在")))?;
                update_profile_from_args(profile, args);
                Ok(())
            })?;
            app::sync_fragments(&paths, &config)?;
            println!("已更新 profile `{id}`。")
        }
        ProfileCommand::Mail(args) => {
            let profile = config
                .profiles
                .get(&args.id)
                .ok_or_else(|| app::AppError::Message(format!("profile `{}` 不存在", args.id)))?;
            let candidates = github::profile_email_candidates(&profile.host, &profile.login)?;
            if args.json {
                print_json(&candidates)?;
            } else if candidates.is_empty() {
                println!("没有发现可用提交邮箱；请手工填写 `--email`。")
            } else {
                println!("{}/{} 的提交邮箱候选：", profile.host, profile.login);
                for candidate in candidates {
                    let mut labels = Vec::new();
                    if candidate.noreply {
                        labels.push("GitHub noreply");
                    }
                    if candidate.primary {
                        labels.push("首选");
                    }
                    if candidate.verified {
                        labels.push("已验证");
                    }
                    if labels.is_empty() {
                        println!("  {}", candidate.email);
                    } else {
                        println!("  {}（{}）", candidate.email, labels.join("，"));
                    }
                }
            }
        }
        ProfileCommand::Remove { id } => {
            let (config, ()) = update_config(&paths, |config| remove_profile(config, &id))?;
            app::sync_fragments(&paths, &config)?;
            println!(
                "已删除 profile `{id}` 及引用它的规则。已有仓库绑定会在下次状态检查时报告失效。"
            )
        }
    }
    Ok(0)
}

fn remove_profile(config: &mut Config, id: &str) -> app::Result<()> {
    config
        .profiles
        .remove(id)
        .ok_or_else(|| app::AppError::Message(format!("profile `{id}` 不存在")))?;
    config.rules.retain(|rule| rule.profile != id);
    if config.behavior.default_profile.as_deref() == Some(id) {
        config.behavior.default_profile = None;
    }
    Ok(())
}

fn resolve_profile_email(
    host: &str,
    login: &str,
    explicit: Option<&str>,
    noreply: bool,
) -> app::Result<String> {
    if noreply {
        return github::github_noreply_email(host, login)?.ok_or_else(|| {
            app::AppError::Message(
                "--noreply 目前只支持 github.com；Enterprise 请使用 --email".into(),
            )
        });
    }
    explicit
        .filter(|email| !email.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| app::AppError::Message("请提供 --email，或使用 --noreply（-N）".into()))
}

fn profile_from_args(args: ProfileArgs, git_email: String) -> Profile {
    let ssh_mode = match args.ssh {
        SshModeArg::External => SshMode::External,
        SshModeArg::OnePassword => SshMode::OnePassword,
        SshModeArg::Managed => SshMode::Managed,
    };
    Profile {
        host: github::normalize_host(&args.host),
        login: args.login,
        git_name: args.git_name,
        git_email,
        ssh: Some(SshProfile {
            mode: ssh_mode,
            public_key: args.public_key,
            fingerprint: args.fingerprint,
            agent_socket: args.agent_socket,
        }),
        signing: SigningProfile {
            enabled: args.sign,
            signing_key: args
                .signing_key
                .map(|value| signing::git_signing_key_value(&value)),
            program: args.signing_program,
        },
    }
}

fn update_profile_from_args(profile: &mut Profile, args: ProfileEditArgs) {
    if let Some(host) = args.host {
        profile.host = github::normalize_host(&host);
    }
    if let Some(login) = args.login {
        profile.login = login;
    }
    if let Some(git_name) = args.git_name {
        profile.git_name = git_name;
    }
    if let Some(git_email) = args.git_email {
        profile.git_email = git_email;
    }

    if args.ssh.is_some()
        || args.public_key.is_some()
        || args.fingerprint.is_some()
        || args.agent_socket.is_some()
    {
        let ssh = profile.ssh.get_or_insert_with(SshProfile::default);
        if let Some(mode) = args.ssh {
            ssh.mode = match mode {
                SshModeArg::External => SshMode::External,
                SshModeArg::OnePassword => SshMode::OnePassword,
                SshModeArg::Managed => SshMode::Managed,
            };
        }
        if let Some(public_key) = args.public_key {
            ssh.public_key = Some(public_key);
        }
        if let Some(fingerprint) = args.fingerprint {
            ssh.fingerprint = Some(fingerprint);
        }
        if let Some(agent_socket) = args.agent_socket {
            ssh.agent_socket = Some(agent_socket);
        }
    }

    if let Some(enabled) = args.sign {
        profile.signing.enabled = enabled;
    }
    if let Some(signing_key) = args.signing_key {
        profile.signing.signing_key = Some(signing::git_signing_key_value(&signing_key));
    }
    if let Some(signing_program) = args.signing_program {
        profile.signing.program = Some(signing_program);
    }
}

fn use_profile(path: Option<&Path>, id: &str, repo: Option<&Path>) -> app::Result<i32> {
    let cwd = repo
        .map(Path::to_path_buf)
        .unwrap_or(std::env::current_dir()?);
    let ctx = context(path, Some(id), cwd)?;
    app::bind_repository(&ctx, id)?;
    println!("已将仓库绑定到 `{id}`。\n{}", ctx.identity_banner());
    Ok(0)
}

fn unbind(path: Option<&Path>, repo: Option<&Path>) -> app::Result<i32> {
    let cwd = repo
        .map(Path::to_path_buf)
        .unwrap_or(std::env::current_dir()?);
    let ctx = context(path, None, cwd)?;
    app::unbind_repository(&ctx)?;
    println!("已解除仓库的 ghis profile 绑定。");
    Ok(0)
}

fn rule_command(path: Option<&Path>, command: RuleCommand) -> app::Result<i32> {
    let (paths, config) = load_config(path)?;
    match command {
        RuleCommand::List(args) => {
            if args.json {
                print_json(&config.rules)?;
            } else if config.rules.is_empty() {
                println!("尚未配置规则。")
            } else {
                for rule in &config.rules {
                    println!(
                        "{}: profile={} priority={}",
                        rule.id, rule.profile, rule.priority
                    );
                }
            }
        }
        RuleCommand::Add(args) => {
            let id = args.id.clone();
            let rule = rule_from_args(args);
            update_config(&paths, |config| {
                if config.rules.iter().any(|existing| existing.id == id) {
                    return Err(app::AppError::Message(format!("规则 `{id}` 已存在")));
                }
                config.rules.push(rule);
                Ok(())
            })?;
            println!("已添加规则 `{id}`。")
        }
        RuleCommand::Edit(args) => {
            let id = args.id.clone();
            let rule = rule_from_args(args);
            update_config(&paths, |config| {
                let position = config
                    .rules
                    .iter()
                    .position(|existing| existing.id == id)
                    .ok_or_else(|| app::AppError::Message(format!("规则 `{id}` 不存在")))?;
                config.rules[position] = rule;
                Ok(())
            })?;
            println!("已更新规则 `{id}`。")
        }
        RuleCommand::Remove { id } => {
            update_config(&paths, |config| {
                let before = config.rules.len();
                config.rules.retain(|rule| rule.id != id);
                if config.rules.len() == before {
                    return Err(app::AppError::Message(format!("规则 `{id}` 不存在")));
                }
                Ok(())
            })?;
            println!("已删除规则 `{id}`。")
        }
    }
    Ok(0)
}

fn rule_from_args(args: RuleArgs) -> Rule {
    Rule {
        id: args.id,
        profile: args.profile,
        priority: args.priority,
        host: args.host,
        owner: args.owner,
        repo: args.repo,
        remote: args.remote,
        gitdir: args.gitdir,
    }
}

fn config_command(path: Option<&Path>, command: ConfigCommand) -> app::Result<i32> {
    let (paths, config) = load_config(path)?;
    match command {
        ConfigCommand::List(args) => {
            let settings = behavior_settings(&config);
            if args.json {
                print_json(&settings)?;
            } else {
                for (key, value) in settings {
                    println!("{key}={value}");
                }
            }
        }
        ConfigCommand::Get { key } => println!("{}", get_behavior(&config, &key)?),
        ConfigCommand::Set { key, value } => {
            let key = normalize_behavior_key(&key)?;
            update_config(&paths, |config| set_behavior(config, &key, &value))?;
            println!("已设置 behavior.{key}={value}")
        }
    }
    Ok(0)
}

fn get_behavior(config: &Config, key: &str) -> app::Result<String> {
    Ok(match normalize_behavior_key(key)?.as_str() {
        "default_profile" => config.behavior.default_profile.clone().unwrap_or_default(),
        "auto_bind" => config.behavior.auto_bind.to_string(),
        "unresolved" => match config.behavior.unresolved {
            UnresolvedPolicy::WarnAndContinue => "warn-and-continue".into(),
            UnresolvedPolicy::Fail => "fail".into(),
        },
        "credential_failure" => match config.behavior.credential_failure {
            CredentialFailurePolicy::Fail => "fail".into(),
        },
        "ssh_unmanaged" => match config.behavior.ssh_unmanaged {
            SshUnmanagedPolicy::WarnAndContinue => "warn-and-continue".into(),
            SshUnmanagedPolicy::Fail => "fail".into(),
        },
        "display_identity" => match config.behavior.display_identity {
            DisplayIdentity::Always => "always".into(),
            DisplayIdentity::SensitiveCommands => "sensitive-commands".into(),
            DisplayIdentity::Never => "never".into(),
        },
        other => {
            return Err(unknown_behavior_key(other));
        }
    })
}

fn set_behavior(config: &mut Config, key: &str, value: &str) -> app::Result<()> {
    match normalize_behavior_key(key)?.as_str() {
        "default_profile" => {
            config.behavior.default_profile =
                (!value.is_empty() && value != "none").then(|| value.into())
        }
        "auto_bind" => config.behavior.auto_bind = parse_bool(value)?,
        "unresolved" => {
            config.behavior.unresolved = match value {
                "warn-and-continue" => UnresolvedPolicy::WarnAndContinue,
                "fail" => UnresolvedPolicy::Fail,
                _ => {
                    return Err(app::AppError::Message(
                        "unresolved 只能是 warn-and-continue 或 fail".into(),
                    ));
                }
            }
        }
        "credential_failure" => {
            if value != "fail" {
                return Err(app::AppError::Message(
                    "v1 为防止账号回退，credential_failure 固定为 fail".into(),
                ));
            }
            config.behavior.credential_failure = CredentialFailurePolicy::Fail;
        }
        "ssh_unmanaged" => {
            config.behavior.ssh_unmanaged = match value {
                "warn-and-continue" => SshUnmanagedPolicy::WarnAndContinue,
                "fail" => SshUnmanagedPolicy::Fail,
                _ => {
                    return Err(app::AppError::Message(
                        "ssh_unmanaged 只能是 warn-and-continue 或 fail".into(),
                    ));
                }
            }
        }
        "display_identity" => {
            config.behavior.display_identity = match value {
                "always" => DisplayIdentity::Always,
                "sensitive-commands" => DisplayIdentity::SensitiveCommands,
                "never" => DisplayIdentity::Never,
                _ => {
                    return Err(app::AppError::Message(
                        "display_identity 只能是 always、sensitive-commands 或 never".into(),
                    ));
                }
            }
        }
        other => {
            return Err(unknown_behavior_key(other));
        }
    }
    Ok(())
}

fn behavior_settings(config: &Config) -> Vec<(&'static str, String)> {
    KNOWN_BEHAVIOR_KEYS
        .iter()
        .map(|key| Ok((*key, get_behavior(config, key)?)))
        .collect::<app::Result<Vec<_>>>()
        .expect("known behavior settings must be readable")
}

const KNOWN_BEHAVIOR_KEYS: [&str; 6] = [
    "default_profile",
    "auto_bind",
    "unresolved",
    "credential_failure",
    "ssh_unmanaged",
    "display_identity",
];

fn normalize_behavior_key(key: &str) -> app::Result<String> {
    let key = key.trim().trim_start_matches("behavior.");
    let key = key.replace('-', "_");
    if KNOWN_BEHAVIOR_KEYS.contains(&key.as_str()) {
        Ok(key)
    } else {
        Err(unknown_behavior_key(&key))
    }
}

fn unknown_behavior_key(key: &str) -> app::AppError {
    app::AppError::Message(format!(
        "不支持的 behavior 设置 `{key}`；可用值：{}",
        KNOWN_BEHAVIOR_KEYS.join("、")
    ))
}

fn parse_bool(value: &str) -> app::Result<bool> {
    match value {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(app::AppError::Message(format!("`{value}` 不是布尔值"))),
    }
}

fn sync(path: Option<&Path>) -> app::Result<i32> {
    let (paths, config) = load_config(path)?;
    let written = app::sync_fragments(&paths, &config)?;
    let config = Config::load(&paths.config_file)?;
    let repositories = app::sync_registered_repositories(&paths, &config);
    println!(
        "已重建 {} 个 profile fragment；检查 {} 个已登记仓库，修复 {} 个，清理 {} 条失效记录。",
        written.len(),
        repositories.checked,
        repositories.repaired,
        repositories.forgotten
    );
    for warning in repositories.warnings {
        eprintln!("ghis: {warning}");
    }
    Ok(0)
}

#[derive(Serialize)]
struct DoctorReport {
    schema_version: u32,
    git: ToolStatus,
    gh: ToolStatus,
    zsh: ToolStatus,
    shell_integration: DoctorShellIntegration,
    accounts: Vec<DoctorAccount>,
    repository: Option<String>,
    profile: Option<String>,
    credential_available: Option<bool>,
    ssh_agent: Option<DoctorAgent>,
    signing_program: Option<DoctorSigningProgram>,
    github_signing_key: Option<DoctorGithubSigningKey>,
    git_config: diagnostics::GitConfigReport,
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct ToolStatus {
    available: bool,
    version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DoctorShellIntegrationState {
    WrapperLoaded,
    WrapperIncomplete,
    InstalledNotLoaded,
    RepositoryOnly,
    NotIntegrated,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorShellIntegration {
    state: DoctorShellIntegrationState,
    wrapper_loaded: bool,
    wrapper_healthy: bool,
    health_marker: Option<String>,
    setup_installed: bool,
    repository_bound: bool,
    advice: String,
}

#[derive(Serialize)]
struct DoctorAccount {
    host: String,
    login: String,
    verified: bool,
}

#[derive(Serialize)]
struct DoctorAgent {
    socket: String,
    source: &'static str,
    available: bool,
    key_count: usize,
    selected_key_fingerprint: Option<String>,
    error: Option<String>,
}

#[derive(Serialize)]
struct DoctorSigningProgram {
    path: String,
    onepassword: bool,
    available: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum DoctorGithubSigningKeyStatus {
    NotChecked,
    Matched,
    NotMatched,
    LocalKeyUnavailable,
    Unavailable,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorGithubSigningKey {
    checked: bool,
    status: DoctorGithubSigningKeyStatus,
    local_fingerprint: Option<String>,
    github_key_count: Option<usize>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct DoctorLocalSigningKey {
    material: String,
    fingerprint: Option<String>,
}

fn doctor(
    path: Option<&Path>,
    explicit: Option<&str>,
    json: bool,
    check_github_signing_key: bool,
) -> app::Result<i32> {
    let ctx = context(path, explicit, std::env::current_dir()?)?;
    let mut warnings = ctx.warnings.clone();
    let shell_integration = doctor_shell_integration(&ctx);
    let diagnostic_cwd = ctx
        .repository
        .as_ref()
        .map(|repository| repository.command_dir().to_path_buf())
        .unwrap_or(std::env::current_dir()?);
    let git_config = match diagnostics::scan_git_config(
        &diagnostic_cwd,
        ctx.profile.as_ref(),
        ctx.remote.as_ref(),
        ctx.identities.as_ref(),
    ) {
        Ok(report) => report,
        Err(error) => {
            warnings.push(format!("无法读取 Git 配置来源：{error}"));
            diagnostics::GitConfigReport::default()
        }
    };
    let discovery = match github::discover_accounts(None) {
        Ok(discovery) => {
            if let Some(warning) = cache_discovery(&ctx.paths, &discovery) {
                warnings.push(warning);
            }
            Some(discovery)
        }
        Err(error) => {
            warnings.push(format!("gh 账号发现失败：{error}"));
            None
        }
    };

    let credential_available =
        ctx.profile.as_ref().map(
            |profile| match github::token(&profile.host, &profile.login) {
                Ok(token) => {
                    drop(token);
                    true
                }
                Err(_) => {
                    warnings.push(format!(
                        "无法取得 {}/{} 的凭据；绑定仓库的 HTTPS 操作会停止",
                        profile.host, profile.login
                    ));
                    false
                }
            },
        );

    let socket = signing::discover_agent_socket(
        ctx.profile
            .as_ref()
            .and_then(|profile| profile.ssh.as_ref())
            .and_then(|ssh| ssh.agent_socket.as_deref()),
    );
    let agent_required = ctx.profile.as_ref().is_some_and(|profile| {
        profile.signing.enabled
            || profile
                .ssh
                .as_ref()
                .is_some_and(|ssh| matches!(ssh.mode, SshMode::OnePassword | SshMode::Managed))
    });
    let signing_required = ctx
        .profile
        .as_ref()
        .is_some_and(|profile| profile.signing.enabled);
    let (local_signing_key, local_signing_key_error) = if signing_required {
        match doctor_local_signing_key(ctx.profile.as_ref().expect("signing profile exists")) {
            Ok(key) => (Some(key), None),
            Err(error) => {
                warnings.push(error.clone());
                (None, Some(error))
            }
        }
    } else {
        (None, None)
    };
    let selector = doctor_key_selector(
        ctx.profile.as_ref(),
        local_signing_key.as_ref().map(|key| key.material.as_str()),
        &mut warnings,
    );
    let ssh_agent = match socket {
        Some(socket) => match signing::inspect_agent(&socket) {
            Ok(agent) => {
                if agent_required && !agent.available {
                    warnings.push(format!(
                        "SSH Agent 不可用：{}",
                        agent.error.as_deref().unwrap_or("未知错误")
                    ));
                }
                let selected_key = selector
                    .as_ref()
                    .and_then(|selector| signing::select_key(&agent.keys, selector));
                if agent_required && selector.is_some() && selected_key.is_none() {
                    warnings.push("配置的 SSH 公钥或指纹不在所选 Agent 中".into());
                }
                Some(DoctorAgent {
                    socket: agent.socket.display().to_string(),
                    source: match agent.source {
                        signing::AgentSource::OnePassword => "1password",
                        signing::AgentSource::System => "system",
                        signing::AgentSource::Unknown => "unknown",
                    },
                    available: agent.available,
                    key_count: agent.keys.len(),
                    selected_key_fingerprint: selected_key.and_then(|key| key.fingerprint),
                    error: agent.error,
                })
            }
            Err(error) => {
                if agent_required {
                    warnings.push(format!("SSH Agent 检查失败：{error}"));
                }
                Some(DoctorAgent {
                    socket: socket.display().to_string(),
                    source: if signing::is_onepassword_socket(&socket) {
                        "1password"
                    } else {
                        "unknown"
                    },
                    available: false,
                    key_count: 0,
                    selected_key_fingerprint: None,
                    error: Some(error.to_string()),
                })
            }
        },
        None => {
            if agent_required {
                warnings.push("未发现当前 Profile 需要的 SSH Agent socket".into());
            }
            None
        }
    };

    let program = signing::discover_signing_program(
        ctx.profile
            .as_ref()
            .and_then(|profile| profile.signing.program.as_deref()),
    );
    let signing_program = program.map(|program| DoctorSigningProgram {
        available: signing::signing_program_available(&program),
        path: program.path.display().to_string(),
        onepassword: program.onepassword,
    });
    if signing_required
        && signing_program
            .as_ref()
            .is_none_or(|program| !program.available)
    {
        warnings.push("已启用 SSH commit signing，但签名程序不可执行".into());
    }

    let github_signing_key = doctor_github_signing_key(
        check_github_signing_key,
        ctx.profile.as_ref(),
        local_signing_key.as_ref(),
        local_signing_key_error.as_deref(),
        &mut warnings,
    );

    let report = DoctorReport {
        schema_version: ghis::SCHEMA_VERSION,
        git: tool_status("git", &["--version"]),
        gh: tool_status("gh", &["--version"]),
        zsh: tool_status("zsh", &["--version"]),
        shell_integration,
        accounts: discovery
            .as_ref()
            .map(|item| {
                item.accounts
                    .iter()
                    .map(|account| DoctorAccount {
                        host: account.host.clone(),
                        login: account.login.clone(),
                        verified: account.verified,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        repository: ctx
            .repository
            .as_ref()
            .map(|repo| repo.command_dir().display().to_string()),
        profile: ctx.profile_id().map(str::to_owned),
        credential_available,
        ssh_agent,
        signing_program,
        github_signing_key,
        git_config,
        warnings,
    };
    if json {
        print_json(&report)?;
    } else {
        println!("Git: {}", tool_text(&report.git));
        println!("gh: {}", tool_text(&report.gh));
        println!("zsh: {}", tool_text(&report.zsh));
        println!(
            "Shell wrapper: {}",
            doctor_shell_integration_text(report.shell_integration.state)
        );
        println!("Shell 提示: {}", report.shell_integration.advice);
        println!("gh 账号: {}", report.accounts.len());
        println!(
            "当前 profile: {}",
            report.profile.as_deref().unwrap_or("未解析")
        );
        println!(
            "GitHub 凭据: {}",
            match report.credential_available {
                Some(true) => "可用",
                Some(false) => "不可用",
                None => "未检查（没有已解析的 Profile）",
            }
        );
        if let Some(agent) = &report.ssh_agent {
            println!(
                "SSH Agent: {}（{}，{}，{} 把 key）",
                agent.socket,
                agent.source,
                if agent.available {
                    "可用"
                } else {
                    "不可用"
                },
                agent.key_count
            );
            println!(
                "匹配 key: {}",
                agent
                    .selected_key_fingerprint
                    .as_deref()
                    .unwrap_or("未匹配")
            );
        } else {
            println!("SSH Agent: 未发现");
        }
        if let Some(program) = &report.signing_program {
            println!(
                "SSH 签名程序: {}（{}）",
                program.path,
                if program.available {
                    "可执行"
                } else {
                    "不可执行"
                }
            );
        } else {
            println!("SSH 签名程序: 未发现");
        }
        if let Some(check) = &report.github_signing_key {
            let count = check
                .github_key_count
                .map(|count| format!("，GitHub 登记 {count} 把"))
                .unwrap_or_default();
            let fingerprint = check
                .local_fingerprint
                .as_deref()
                .map(|fingerprint| format!("，本地 {fingerprint}"))
                .unwrap_or_default();
            println!(
                "GitHub SSH 签名公钥: {}{}{}",
                doctor_github_signing_key_text(check.status),
                count,
                fingerprint
            );
        } else {
            println!("GitHub SSH 签名公钥: 未启用");
        }
        println!(
            "Git 配置冲突: {}",
            diagnostics::summary_line(&report.git_config)
        );
        for item in &report.git_config.diagnostics {
            println!("{}", diagnostics::render_row(item));
        }
        for warning in &report.warnings {
            eprintln!("警告：{warning}");
        }
    }
    Ok(0)
}

fn doctor_shell_integration(ctx: &AppContext) -> DoctorShellIntegration {
    let health = shell::integration_health();
    let wrapper_loaded = shell::integration_is_loaded();
    let wrapper_healthy = health == shell::IntegrationHealth::Healthy;
    let health_marker = std::env::var_os(shell::HEALTH_ENV)
        .map(|value| diagnostics::sanitize_display_text(&value.to_string_lossy()));
    let init_file = ctx.paths.config_dir.join("init.zsh");
    let setup_installed = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| {
            let zshrc = shell::zshrc_path(&home, std::env::var_os("ZDOTDIR").as_deref());
            shell::integration_is_installed(&zshrc, &init_file, "ghis")
        })
        .unwrap_or(false);
    let repository_bound = ctx
        .repository
        .as_ref()
        .is_some_and(repository_has_ghis_persistence);
    let state = classify_shell_integration(health, setup_installed, repository_bound);
    DoctorShellIntegration {
        state,
        wrapper_loaded,
        wrapper_healthy,
        health_marker,
        setup_installed,
        repository_bound,
        advice: doctor_shell_integration_advice(state).into(),
    }
}

fn repository_has_ghis_persistence(repository: &ghis::repo::Repository) -> bool {
    if ghis::repo::local_config(repository, app::PROFILE_CONFIG_KEY)
        .ok()
        .flatten()
        .is_some()
    {
        return true;
    }

    let include_keys = ghis::repo::local_config_keys_matching(
        repository,
        r"^(include\.path|includeif\..*\.path)$",
    )
    .unwrap_or_default();
    if include_keys.iter().any(|key| {
        ghis::repo::local_config_values(repository, key)
            .unwrap_or_default()
            .iter()
            .any(|value| path_is_ghis_fragment(value))
    }) {
        return true;
    }

    let helper_keys =
        ghis::repo::local_config_keys_matching(repository, r"^credential\..*\.helper$")
            .unwrap_or_default();
    if helper_keys.iter().any(|key| {
        ghis::repo::local_config_values(repository, key)
            .unwrap_or_default()
            .iter()
            .any(|value| is_ghis_credential_helper_marker(value))
    }) {
        return true;
    }

    ghis::repo::local_config_keys_matching(
        repository,
        r"^hook\.ghis-[^.]+\.(command|event|enabled|parallel)$",
    )
    .map(|keys| !keys.is_empty())
    .unwrap_or(false)
}

fn path_is_ghis_fragment(value: &str) -> bool {
    value
        .replace('\\', "/")
        .to_ascii_lowercase()
        .contains("/ghis/fragments/")
}

fn is_ghis_credential_helper_marker(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("credential-helper") && value.contains("ghis")
}

fn classify_shell_integration(
    health: shell::IntegrationHealth,
    setup_installed: bool,
    repository_bound: bool,
) -> DoctorShellIntegrationState {
    if health == shell::IntegrationHealth::Healthy {
        DoctorShellIntegrationState::WrapperLoaded
    } else if health == shell::IntegrationHealth::Incomplete {
        DoctorShellIntegrationState::WrapperIncomplete
    } else if setup_installed {
        DoctorShellIntegrationState::InstalledNotLoaded
    } else if repository_bound {
        DoctorShellIntegrationState::RepositoryOnly
    } else {
        DoctorShellIntegrationState::NotIntegrated
    }
}

fn doctor_shell_integration_text(state: DoctorShellIntegrationState) -> &'static str {
    match state {
        DoctorShellIntegrationState::WrapperLoaded => "当前 shell 已完整加载",
        DoctorShellIntegrationState::WrapperIncomplete => {
            "当前 shell marker 存在，但函数依赖不完整"
        }
        DoctorShellIntegrationState::InstalledNotLoaded => "已安装，但当前 shell 未加载",
        DoctorShellIntegrationState::RepositoryOnly => "仅检测到仓库本地 ghis 配置",
        DoctorShellIntegrationState::NotIntegrated => "未安装或未接入",
    }
}

fn doctor_shell_integration_advice(state: DoctorShellIntegrationState) -> &'static str {
    match state {
        DoctorShellIntegrationState::WrapperLoaded => {
            "普通 git/gh 会经过 ghis；command git、绝对路径或 GHIS_BYPASS=1 仍可明确绕过 wrapper"
        }
        DoctorShellIntegrationState::WrapperIncomplete => {
            "当前 shell 只继承了旧版/不完整 marker；重新 source init.zsh、运行 exec zsh 或新开终端"
        }
        DoctorShellIntegrationState::InstalledNotLoaded => {
            "运行 exec zsh 或新开终端后再试；ghis 不会替换当前 shell"
        }
        DoctorShellIntegrationState::RepositoryOnly => {
            "普通 Git 仍会读取该仓库已有的 include/helper/hook，但没有 wrapper 的本次解析和注入"
        }
        DoctorShellIntegrationState::NotIntegrated => {
            "普通 git/gh 不会经过 ghis；需要时先运行 ghis setup 并重新加载 zsh"
        }
    }
}

fn doctor_local_signing_key(
    profile: &Profile,
) -> std::result::Result<DoctorLocalSigningKey, String> {
    let public_key = profile_public_key_for_doctor(profile)?
        .ok_or_else(|| "已启用签名，但未配置签名公钥".to_string())?;
    let material = signing::public_key_material(&public_key)
        .ok_or_else(|| "配置的签名公钥不包含有效 SSH 公钥".to_string())?;
    let fingerprint = signing::fingerprint(&material)
        .map_err(|error| format!("配置的签名公钥无法通过 ssh-keygen 校验：{error}"))?;
    Ok(DoctorLocalSigningKey {
        material,
        fingerprint: Some(fingerprint),
    })
}

fn doctor_github_signing_key(
    requested: bool,
    profile: Option<&Profile>,
    local_key: Option<&DoctorLocalSigningKey>,
    local_error: Option<&str>,
    warnings: &mut Vec<String>,
) -> Option<DoctorGithubSigningKey> {
    let profile = profile.filter(|profile| profile.signing.enabled)?;
    let local_fingerprint = local_key.and_then(|key| key.fingerprint.clone());
    let Some(local_key) = local_key else {
        return Some(DoctorGithubSigningKey {
            checked: false,
            status: DoctorGithubSigningKeyStatus::LocalKeyUnavailable,
            local_fingerprint,
            github_key_count: None,
            error: local_error.map(str::to_owned),
        });
    };
    if !requested {
        return Some(DoctorGithubSigningKey {
            checked: false,
            status: DoctorGithubSigningKeyStatus::NotChecked,
            local_fingerprint,
            github_key_count: None,
            error: None,
        });
    }

    match github::ssh_signing_keys(&profile.host, &profile.login) {
        Ok(keys) => {
            let materials = keys
                .iter()
                .map(|key| signing::public_key_material(&key.key))
                .collect::<Option<Vec<_>>>();
            let Some(materials) = materials else {
                let error = "GitHub 返回了无法识别的 SSH signing 公钥".to_string();
                warnings.push(format!("{error}；不会因此判定本地签名 key 失效"));
                return Some(DoctorGithubSigningKey {
                    checked: true,
                    status: DoctorGithubSigningKeyStatus::Unavailable,
                    local_fingerprint,
                    github_key_count: None,
                    error: Some(error),
                });
            };
            let matched = materials
                .iter()
                .any(|material| material == &local_key.material);
            if !matched {
                if let Some(error) = materials
                    .iter()
                    .find_map(|material| signing::fingerprint(material).err())
                {
                    let error = format!(
                        "GitHub 返回了无法通过 ssh-keygen 校验的 SSH signing 公钥：{error}"
                    );
                    warnings.push(format!("{error}；不会因此判定本地签名 key 失效"));
                    return Some(DoctorGithubSigningKey {
                        checked: true,
                        status: DoctorGithubSigningKeyStatus::Unavailable,
                        local_fingerprint,
                        github_key_count: None,
                        error: Some(error),
                    });
                }
                warnings.push(
                    "GitHub 未登记当前 Profile 的 SSH 签名公钥；本地 key 未被判为失效，但 GitHub 可能无法验证签名"
                        .into(),
                );
            }
            Some(DoctorGithubSigningKey {
                checked: true,
                status: if matched {
                    DoctorGithubSigningKeyStatus::Matched
                } else {
                    DoctorGithubSigningKeyStatus::NotMatched
                },
                local_fingerprint,
                github_key_count: Some(keys.len()),
                error: None,
            })
        }
        Err(error) => {
            let error = error.to_string();
            warnings.push(format!(
                "无法检查 GitHub SSH 签名公钥：{error}；不会因此判定本地签名 key 失效"
            ));
            Some(DoctorGithubSigningKey {
                checked: true,
                status: DoctorGithubSigningKeyStatus::Unavailable,
                local_fingerprint,
                github_key_count: None,
                error: Some(error),
            })
        }
    }
}

fn doctor_github_signing_key_text(status: DoctorGithubSigningKeyStatus) -> &'static str {
    match status {
        DoctorGithubSigningKeyStatus::NotChecked => {
            "未检查（使用 --check-github-signing-key 才会访问 GitHub API）"
        }
        DoctorGithubSigningKeyStatus::Matched => "已匹配",
        DoctorGithubSigningKeyStatus::NotMatched => "未匹配（GitHub 未登记当前公钥）",
        DoctorGithubSigningKeyStatus::LocalKeyUnavailable => "本地签名公钥不可用，未检查",
        DoctorGithubSigningKeyStatus::Unavailable => "无法检查（保留本地状态）",
    }
}

fn doctor_key_selector(
    profile: Option<&Profile>,
    signing_public_key: Option<&str>,
    warnings: &mut Vec<String>,
) -> Option<signing::SigningProfile> {
    let profile = profile?;
    let ssh = profile.ssh.as_ref();
    let key_required = profile.signing.enabled
        || ssh.is_some_and(|ssh| matches!(ssh.mode, SshMode::OnePassword | SshMode::Managed));
    if !key_required {
        return None;
    }

    if ssh.is_some_and(|ssh| {
        matches!(ssh.mode, SshMode::OnePassword | SshMode::Managed) && ssh.public_key.is_none()
    }) {
        warnings.push("当前 Profile 纳管 SSH，但未配置公钥文件".into());
    }

    let public_key = if profile.signing.enabled {
        let key = signing_public_key?;
        Some(key.to_owned())
    } else {
        ssh.and_then(|ssh| ssh.public_key.as_deref())
            .and_then(|path| {
                let path = signing::expand_user(path);
                match fs::read_to_string(&path) {
                    Ok(key) => Some(key),
                    Err(error) => {
                        warnings.push(format!("无法读取 SSH 公钥 {}：{error}", path.display()));
                        None
                    }
                }
            })
    };
    Some(signing::SigningProfile {
        enabled: key_required,
        public_key,
        fingerprint: app::profile_signing_fingerprint(profile),
        ..signing::SigningProfile::default()
    })
}

fn profile_public_key_for_doctor(profile: &Profile) -> std::result::Result<Option<String>, String> {
    let value = profile
        .signing
        .signing_key
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| profile.ssh.as_ref().and_then(|ssh| ssh.public_key.clone()));
    let Some(value) = value else {
        return Err("已启用签名，但未配置签名公钥".into());
    };
    let text = value.to_string_lossy();
    if signing::is_public_key_line(text.trim_start()) {
        return Ok(Some(text.trim_start().to_owned()));
    }
    if let Some(key) = text.trim_start().strip_prefix("key::") {
        return Ok(Some(key.to_owned()));
    }
    let path = signing::expand_user(&value);
    fs::read_to_string(&path)
        .map(Some)
        .map_err(|error| format!("无法读取签名公钥 {}：{error}", path.display()))
}

fn tool_status(program: &str, args: &[&str]) -> ToolStatus {
    match std::process::Command::new(program).args(args).output() {
        Ok(output) => ToolStatus {
            available: output.status.success(),
            version: Some(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .into(),
            ),
        },
        Err(_) => ToolStatus {
            available: false,
            version: None,
        },
    }
}

fn tool_text(status: &ToolStatus) -> String {
    if status.available {
        status.version.clone().unwrap_or_else(|| "可用".into())
    } else {
        "未安装".into()
    }
}

fn setup(args: SetupArgs) -> app::Result<i32> {
    let paths = ConfigPaths::discover()?;
    let init = paths.config_dir.join("init.zsh");
    if args.print {
        print!("{}", shell::zsh_init_script("ghis"));
        return Ok(0);
    }
    let home =
        std::env::var_os("HOME").ok_or_else(|| app::AppError::Message("HOME 未设置".into()))?;
    let zshrc = shell::zshrc_path(Path::new(&home), std::env::var_os("ZDOTDIR").as_deref());
    if !args.yes {
        if !io::stdin().is_terminal() {
            return Err(app::AppError::Message(format!(
                "非交互环境不会自动修改 {}；确认目标后重新运行 `ghis setup --yes`",
                zshrc.display()
            )));
        }
        eprint!("将备份并更新 {}，继续吗？[y/N] ", zshrc.display());
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("已取消，未修改任何文件。");
            return Ok(0);
        }
    }
    let report = shell::setup(zshrc, &init, "ghis")?;
    println!(
        "zsh 集成{}：{}",
        if report.changed {
            "已安装"
        } else {
            "无需更新"
        },
        report.zshrc.display()
    );
    if let Some(backup) = report.backup {
        println!("原文件备份：{}", backup.display());
    }
    if shell::integration_is_loaded() {
        println!("当前 shell 已加载 ghis wrapper。");
    } else {
        println!(
            "当前 shell 尚未加载；运行 `exec zsh` 或新开终端后生效，ghis 不会自动替换 shell。"
        );
    }
    Ok(0)
}

fn uninstall() -> app::Result<i32> {
    let home =
        std::env::var_os("HOME").ok_or_else(|| app::AppError::Message("HOME 未设置".into()))?;
    let zshrc = shell::zshrc_path(Path::new(&home), std::env::var_os("ZDOTDIR").as_deref());
    let changed = shell::uninstall(&zshrc)?;
    println!(
        "{}",
        if changed {
            "已从 .zshrc 移除 ghis 管理块，备份和 init 文件已保留。"
        } else {
            "未发现 ghis 管理块。"
        }
    );
    Ok(0)
}

fn credential_helper(
    path: Option<&Path>,
    explicit: Option<&str>,
    action: &str,
) -> app::Result<i32> {
    let ctx = match context(path, explicit, std::env::current_dir()?) {
        Ok(ctx) => ctx,
        Err(error) => {
            stop_credential_chain(action)?;
            return Err(error);
        }
    };
    if let Err(error) = ctx.ensure_selection_available() {
        stop_credential_chain(action)?;
        return Err(error);
    }
    let profile = match ctx.profile {
        Some(profile) => profile,
        None => {
            stop_credential_chain(action)?;
            return Err(app::AppError::Message(
                "credential helper 无法解析当前仓库 Profile".into(),
            ));
        }
    };
    credential::serve(
        &[action.to_owned()],
        BufReader::new(io::stdin()),
        io::stdout(),
        credential::CredentialProfile {
            host: profile.host,
            login: profile.login,
        },
    )
    .map_err(|error| app::AppError::Message(error.to_string()))?;
    Ok(0)
}

fn stop_credential_chain(action: &str) -> app::Result<()> {
    if action == "get" {
        io::stdout().write_all(b"quit=true\n\n")?;
        io::stdout().flush()?;
    }
    Ok(())
}

fn hook(path: Option<&Path>, explicit: Option<&str>, hook: &str) -> app::Result<i32> {
    let ctx = context(path, explicit, std::env::current_dir()?)?;
    if matches!(hook, "prepare-commit-msg" | "pre-push") {
        ctx.ensure_selection_available()?;
        if std::env::var_os("GHIS_BANNER_SHOWN").as_deref() != Some(std::ffi::OsStr::new("1")) {
            eprintln!("{}", ctx.hook_identity_banner());
        }
        if hook == "prepare-commit-msg"
            && let Some(profile) = ctx.profile.as_ref()
            && profile.signing.enabled
        {
            let status = app::inspect_profile_signing(profile)?;
            if !status.warnings.is_empty() {
                return Err(app::AppError::Message(format!(
                    "签名检查失败：{}",
                    status.warnings.join("；")
                )));
            }
        }
    }
    Ok(0)
}

fn run_tui(path: Option<&Path>, explicit: Option<&str>) -> app::Result<i32> {
    if !io::stdout().is_terminal() {
        return Err(app::AppError::Message("TUI 需要连接终端".into()));
    }
    let config_path = path.map(Path::to_path_buf);
    let explicit = explicit.map(str::to_owned);
    let cwd = std::env::current_dir()?;
    let (paths, config) = load_config(config_path.as_deref())?;
    let cache_path = discovery_cache_path(&paths);
    let (mut discovery, cache_warning) = match github::load_discovery_cache(&cache_path) {
        Ok(discovery) => (discovery, None),
        Err(error) => (
            None,
            Some(format!("无法读取 gh 账号缓存：{error}；可按 r 重新生成")),
        ),
    };
    let mut state = tui::AppState::default();
    initialize_tui_state(&mut state, &config, discovery.as_ref());
    if let Some(warning) = cache_warning {
        state.warnings.push(warning);
    }
    let mut pending = None;
    let workers = TuiWorkers::new()?;
    workers.local_refresh(config_path.clone(), explicit.clone(), cwd.clone())?;
    let mut active_workers = 1usize;
    state.loading = true;
    state.details = vec!["正在后台检查仓库、工具和签名状态".into()];
    state.status = "界面已就绪，正在后台检查本地状态".into();
    let action_context = TuiActionContext {
        workers: &workers,
        path: config_path.as_deref(),
        explicit: explicit.as_deref(),
        cwd: &cwd,
        cache_path: &cache_path,
    };
    tui::run_with_handler(state, |state, action| {
        let result = handle_tui_action(
            state,
            action,
            &mut pending,
            &mut discovery,
            &mut active_workers,
            &action_context,
        );
        if let Err(error) = result {
            state.status = format!("操作失败：{error}");
            state.warnings.push(error.to_string());
        }
    })?;
    Ok(0)
}

struct TuiWorkers {
    jobs: Sender<TuiWorkerJob>,
    #[cfg(test)]
    sender: Sender<TuiWorkerResult>,
    receiver: Receiver<TuiWorkerResult>,
}

impl TuiWorkers {
    fn new() -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let (jobs, job_receiver) = mpsc::channel();
        let worker_sender = sender.clone();
        thread::Builder::new()
            .name("ghis-tui-worker".into())
            .spawn(move || run_tui_worker(job_receiver, worker_sender))?;
        Ok(Self {
            jobs,
            #[cfg(test)]
            sender,
            receiver,
        })
    }

    fn discover(&self) -> io::Result<()> {
        self.send(TuiWorkerJob::Discovery)
    }

    fn local_refresh(
        &self,
        path: Option<PathBuf>,
        explicit: Option<String>,
        cwd: PathBuf,
    ) -> io::Result<()> {
        self.send(TuiWorkerJob::LocalRefresh {
            path,
            explicit,
            cwd,
        })
    }

    fn emails(&self, host: String, login: String) -> io::Result<()> {
        self.send(TuiWorkerJob::Emails { host, login })
    }

    fn inspect_agent(&self, socket: Option<PathBuf>) -> io::Result<()> {
        self.send(TuiWorkerJob::InspectAgent {
            socket,
            submitted_ssh: None,
        })
    }

    fn inspect_agent_for_ssh(&self, socket: PathBuf, submitted_ssh: String) -> io::Result<()> {
        self.send(TuiWorkerJob::InspectAgent {
            socket: Some(socket),
            submitted_ssh: Some(submitted_ssh),
        })
    }

    fn bind(
        &self,
        path: Option<PathBuf>,
        id: String,
        refresh_explicit: Option<String>,
        cwd: PathBuf,
    ) -> io::Result<()> {
        self.send(TuiWorkerJob::Bind {
            path,
            id,
            refresh_explicit,
            cwd,
        })
    }

    fn unbind(
        &self,
        path: Option<PathBuf>,
        explicit: Option<String>,
        cwd: PathBuf,
    ) -> io::Result<()> {
        self.send(TuiWorkerJob::Unbind {
            path,
            explicit,
            cwd,
        })
    }

    fn send(&self, job: TuiWorkerJob) -> io::Result<()> {
        self.jobs
            .send(job)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "TUI 后台 worker 已停止"))
    }
}

#[derive(Debug)]
enum TuiWorkerJob {
    LocalRefresh {
        path: Option<PathBuf>,
        explicit: Option<String>,
        cwd: PathBuf,
    },
    Discovery,
    Emails {
        host: String,
        login: String,
    },
    InspectAgent {
        socket: Option<PathBuf>,
        submitted_ssh: Option<String>,
    },
    Bind {
        path: Option<PathBuf>,
        id: String,
        refresh_explicit: Option<String>,
        cwd: PathBuf,
    },
    Unbind {
        path: Option<PathBuf>,
        explicit: Option<String>,
        cwd: PathBuf,
    },
}

fn run_tui_worker(receiver: Receiver<TuiWorkerJob>, sender: Sender<TuiWorkerResult>) {
    while let Ok(job) = receiver.recv() {
        let result = match job {
            TuiWorkerJob::LocalRefresh {
                path,
                explicit,
                cwd,
            } => TuiWorkerResult::LocalRefresh(build_tui_local_snapshot(
                path.as_deref(),
                explicit.as_deref(),
                &cwd,
            )),
            TuiWorkerJob::Discovery => TuiWorkerResult::Discovery(github::discover_accounts(None)),
            TuiWorkerJob::Emails { host, login } => {
                let result = github::profile_email_candidates(&host, &login);
                TuiWorkerResult::Emails {
                    host,
                    login,
                    result,
                }
            }
            TuiWorkerJob::InspectAgent {
                socket,
                submitted_ssh,
            } => {
                let result = (|| {
                    let socket = signing::discover_agent_socket(socket.as_deref());
                    let agent = match socket.as_ref() {
                        Some(socket) => Some(signing::inspect_agent(socket)?),
                        None => None,
                    };
                    Ok(TuiAgentDiscovery {
                        socket,
                        agent,
                        public_keys: signing::discover_public_key_files(),
                        signing_program: signing::discover_signing_program(None),
                    })
                })();
                TuiWorkerResult::Agent {
                    submitted_ssh,
                    result,
                }
            }
            TuiWorkerJob::Bind {
                path,
                id,
                refresh_explicit,
                cwd,
            } => {
                let result = (|| {
                    let ctx = context(path.as_deref(), Some(&id), &cwd)?;
                    app::bind_repository(&ctx, &id)?;
                    build_tui_local_snapshot(path.as_deref(), refresh_explicit.as_deref(), &cwd)
                })();
                TuiWorkerResult::RepositoryMutation {
                    operation: TuiRepositoryMutation::Bind(id),
                    result,
                }
            }
            TuiWorkerJob::Unbind {
                path,
                explicit,
                cwd,
            } => {
                let result = (|| {
                    let ctx = context(path.as_deref(), explicit.as_deref(), &cwd)?;
                    app::unbind_repository(&ctx)?;
                    build_tui_local_snapshot(path.as_deref(), explicit.as_deref(), &cwd)
                })();
                TuiWorkerResult::RepositoryMutation {
                    operation: TuiRepositoryMutation::Unbind,
                    result,
                }
            }
        };
        if sender.send(result).is_err() {
            break;
        }
    }
}

#[derive(Debug)]
enum TuiRepositoryMutation {
    Bind(String),
    Unbind,
}

#[derive(Debug)]
enum TuiWorkerResult {
    LocalRefresh(app::Result<TuiLocalSnapshot>),
    Discovery(github::Result<github::GhDiscovery>),
    Emails {
        host: String,
        login: String,
        result: github::Result<Vec<github::EmailCandidate>>,
    },
    Agent {
        submitted_ssh: Option<String>,
        result: app::Result<TuiAgentDiscovery>,
    },
    RepositoryMutation {
        operation: TuiRepositoryMutation,
        result: app::Result<TuiLocalSnapshot>,
    },
}

#[derive(Debug)]
struct TuiAgentDiscovery {
    socket: Option<PathBuf>,
    agent: Option<signing::AgentInfo>,
    public_keys: Vec<signing::PublicKeyFile>,
    signing_program: Option<signing::SigningProgram>,
}

#[derive(Debug)]
struct TuiLocalSnapshot {
    repository: String,
    profile: String,
    git_identity: String,
    github_identity: String,
    transport: String,
    signing: String,
    warnings: Vec<String>,
    status_items: Vec<String>,
    config: Config,
    diagnostics: Vec<String>,
}

#[derive(Debug, Clone)]
enum TuiPending {
    ProfileWizard(Box<TuiProfileWizard>),
    AddRule,
    EditRule(String),
    DeleteProfile(String),
    DeleteRule(String),
    LoadingDiscoveredProfile {
        host: String,
        login: String,
        bind_after_create: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiProfileWizardPhase {
    Account,
    Ssh,
    Signing,
}

/// Four-stage Profile editor state. The fourth stage is the existing confirm
/// mode; keeping the draft in memory means no partial profile is written when
/// the user cancels at any point.
#[derive(Debug, Clone)]
struct TuiProfileWizard {
    id: String,
    original_id: Option<String>,
    original_profile: Option<Profile>,
    host: String,
    login: String,
    git_name: String,
    git_email: String,
    ssh: Option<SshProfile>,
    signing: SigningProfile,
    bind_after_create: bool,
    phase: TuiProfileWizardPhase,
    agent: Option<signing::AgentInfo>,
    agent_socket: Option<PathBuf>,
    public_keys: Vec<signing::PublicKeyFile>,
    signing_program: Option<signing::SigningProgram>,
}

#[derive(Debug, Clone)]
struct TuiResolvedSigningKey {
    config_value: String,
    public_key: String,
    fingerprint: String,
}

impl TuiProfileWizard {
    fn new(
        host: impl Into<String>,
        login: impl Into<String>,
        git_email: impl Into<String>,
        bind_after_create: bool,
    ) -> Self {
        Self {
            id: String::new(),
            original_id: None,
            original_profile: None,
            host: github::normalize_host(&host.into()),
            login: login.into(),
            git_name: String::new(),
            git_email: git_email.into(),
            ssh: None,
            signing: SigningProfile::default(),
            bind_after_create,
            phase: TuiProfileWizardPhase::Account,
            agent: None,
            agent_socket: None,
            public_keys: Vec::new(),
            signing_program: None,
        }
    }

    fn from_profile(id: &str, profile: &Profile) -> Self {
        Self {
            id: id.into(),
            original_id: Some(id.into()),
            original_profile: Some(profile.clone()),
            host: profile.host.clone(),
            login: profile.login.clone(),
            git_name: profile.git_name.clone(),
            git_email: profile.git_email.clone(),
            ssh: profile.ssh.clone(),
            signing: profile.signing.clone(),
            bind_after_create: false,
            phase: TuiProfileWizardPhase::Account,
            agent: None,
            agent_socket: profile
                .ssh
                .as_ref()
                .and_then(|ssh| ssh.agent_socket.clone()),
            public_keys: Vec::new(),
            signing_program: None,
        }
    }

    fn account_input(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            self.id, self.host, self.login, self.git_name, self.git_email
        )
    }

    fn ssh_input(&self) -> String {
        let Some(ssh) = self.ssh.as_ref() else {
            return "external".into();
        };
        if ssh.mode == SshMode::External {
            return "external".into();
        }
        let mode = match ssh.mode {
            SshMode::External => "external",
            SshMode::OnePassword => "one-password",
            SshMode::Managed => "managed",
        };
        format!(
            "{}|{}|{}",
            mode,
            ssh.public_key
                .as_deref()
                .map(|path| path.to_string_lossy())
                .unwrap_or_default(),
            ssh.agent_socket
                .as_deref()
                .map(|path| path.to_string_lossy())
                .unwrap_or_default()
        )
    }

    fn signing_input(&self) -> String {
        if !self.signing.enabled {
            return "off".into();
        }
        let key = self
            .signing
            .signing_key
            .clone()
            .or_else(|| {
                self.ssh
                    .as_ref()
                    .and_then(|ssh| ssh.public_key.as_deref())
                    .map(|path| path.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        let program = self
            .signing
            .program
            .as_deref()
            .map(|path| path.to_string_lossy().into_owned())
            .or_else(|| {
                self.signing_program
                    .as_ref()
                    .map(|program| program.path.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        format!("on|{key}|{program}")
    }

    fn profile(&self) -> Profile {
        Profile {
            host: self.host.clone(),
            login: self.login.clone(),
            git_name: self.git_name.clone(),
            git_email: self.git_email.clone(),
            ssh: self.ssh.clone(),
            signing: self.signing.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TuiProfileSelection {
    Configured(String),
    Discovered { host: String, login: String },
}

#[derive(Clone, Copy)]
struct TuiActionContext<'a> {
    workers: &'a TuiWorkers,
    path: Option<&'a Path>,
    explicit: Option<&'a str>,
    cwd: &'a Path,
    cache_path: &'a Path,
}

fn build_tui_local_snapshot(
    path: Option<&Path>,
    explicit: Option<&str>,
    cwd: &Path,
) -> app::Result<TuiLocalSnapshot> {
    let ctx = context(path, explicit, cwd)?;
    let repository = ctx
        .repository
        .as_ref()
        .map(|repo| repo.command_dir().display().to_string())
        .unwrap_or_else(|| "未检测到 Git 仓库".into());
    let profile = ctx.profile_id().unwrap_or("未绑定").into();
    let mut git_identity = "未知".into();
    let mut github_identity = "未知".into();
    let mut signing = "未启用".into();
    let mut warnings = ctx.warnings.clone();
    if let Some(identities) = ctx.identities.as_ref() {
        git_identity = if identities.author.name == identities.committer.name
            && identities.author.email == identities.committer.email
        {
            format!("{} <{}>", identities.author.name, identities.author.email)
        } else {
            format!(
                "作者 {} <{}>；提交者 {} <{}>",
                identities.author.name,
                identities.author.email,
                identities.committer.name,
                identities.committer.email
            )
        };
        if let Some(profile) = ctx.profile.as_ref() {
            let check = identities.check(ghis::git::IdentityExpectation {
                name: &profile.git_name,
                email: &profile.git_email,
            });
            if !check.author_matches || !check.committer_matches {
                warnings.push(
                    "Git 实际 author/committer 覆盖了当前 Profile 的提交身份，请在操作前核对"
                        .into(),
                );
            }
        }
    } else if let Some(profile) = ctx.profile.as_ref() {
        git_identity = format!(
            "配置值 {} <{}>（Git 实际值不可用）",
            profile.git_name, profile.git_email
        );
    }
    if let Some(profile) = ctx.profile.as_ref() {
        github_identity = format!("{}/{}", profile.host, profile.login);
        signing = if profile.signing.enabled {
            "SSH（开启）"
        } else {
            "未启用"
        }
        .into();
    }
    let transport = ctx
        .remote
        .as_ref()
        .map(|remote| remote.transport.as_str().into())
        .unwrap_or_else(|| "未知".into());

    let report = app::display_status(&ctx);
    let mut status_items = vec![
        format!("仓库\t{}", report.repository.as_deref().unwrap_or("无")),
        format!(
            "身份配置\t{}",
            report.profile.as_deref().unwrap_or("未解析")
        ),
        format!("解析来源\t{}", report.resolution_source),
        format!("传输\t{}", report.transport.as_deref().unwrap_or("未知")),
    ];
    if let Some(author) = report.author.as_ref() {
        status_items.push(format!("实际 author\t{} <{}>", author.name, author.email));
    }
    if let Some(committer) = report.committer.as_ref() {
        status_items.push(format!(
            "实际 committer\t{} <{}>",
            committer.name, committer.email
        ));
    }
    let agent = signing::discover_agent_socket(
        ctx.profile
            .as_ref()
            .and_then(|profile| profile.ssh.as_ref())
            .and_then(|ssh| ssh.agent_socket.as_deref()),
    );
    let signing_program = signing::discover_signing_program(
        ctx.profile
            .as_ref()
            .and_then(|profile| profile.signing.program.as_deref()),
    );
    let shell_integration = doctor_shell_integration(&ctx);
    if !shell_integration.wrapper_loaded {
        warnings.push(shell_integration.advice.clone());
    }
    let mut diagnostics = vec![
        format!("Git\t{}", tool_text(&tool_status("git", &["--version"]))),
        format!("gh\t{}", tool_text(&tool_status("gh", &["--version"]))),
        format!("zsh\t{}", tool_text(&tool_status("zsh", &["--version"]))),
        format!(
            "Shell wrapper\t{}",
            doctor_shell_integration_text(shell_integration.state)
        ),
        format!(
            "SSH Agent\t{}",
            agent
                .as_deref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|| "未发现".into())
        ),
        format!(
            "SSH 签名程序\t{}",
            signing_program
                .map(|program| program.path.to_string_lossy().into_owned())
                .unwrap_or_else(|| "未发现".into())
        ),
    ];
    match diagnostics::scan_git_config(
        cwd,
        ctx.profile.as_ref(),
        ctx.remote.as_ref(),
        ctx.identities.as_ref(),
    ) {
        Ok(report) => {
            diagnostics.push(format!(
                "Git 配置冲突\t{}",
                diagnostics::summary_line(&report)
            ));
            diagnostics.extend(report.diagnostics.iter().map(diagnostics::render_row));
        }
        Err(error) => warnings.push(format!("无法读取 Git 配置来源：{error}")),
    }
    Ok(TuiLocalSnapshot {
        repository,
        profile,
        git_identity,
        github_identity,
        transport,
        signing,
        warnings,
        status_items,
        config: ctx.config,
        diagnostics,
    })
}

fn apply_tui_local_snapshot(
    state: &mut tui::AppState,
    snapshot: TuiLocalSnapshot,
    discovery: Option<&github::GhDiscovery>,
) {
    let cache_warnings = state
        .warnings
        .iter()
        .filter(|warning| warning.contains("gh 账号缓存"))
        .cloned()
        .collect::<Vec<_>>();
    state.repository = snapshot.repository;
    state.profile = snapshot.profile;
    state.git_identity = snapshot.git_identity;
    state.github_identity = snapshot.github_identity;
    state.transport = snapshot.transport;
    state.signing = snapshot.signing;
    state.warnings = snapshot.warnings;
    for warning in cache_warnings {
        if !state.warnings.contains(&warning) {
            state.warnings.push(warning);
        }
    }
    state.set_view_items(tui::View::Status, snapshot.status_items);
    apply_tui_config_views(state, &snapshot.config, discovery);
    state.set_view_items(
        tui::View::Diagnostics,
        tui_diagnostics_with_accounts(snapshot.diagnostics, discovery),
    );
}

fn initialize_tui_state(
    state: &mut tui::AppState,
    config: &Config,
    discovery: Option<&github::GhDiscovery>,
) {
    state.set_view_items(
        tui::View::Status,
        [
            "仓库\t正在后台检查".to_string(),
            "身份配置\t正在后台解析".to_string(),
        ],
    );
    apply_tui_config_views(state, config, discovery);
    state.set_view_items(
        tui::View::Diagnostics,
        tui_diagnostics_with_accounts(vec!["本地诊断\t正在后台检查".to_string()], discovery),
    );
}

fn apply_tui_config_views(
    state: &mut tui::AppState,
    config: &Config,
    discovery: Option<&github::GhDiscovery>,
) {
    state.set_view_items(tui::View::Profiles, tui_profile_rows(config, discovery));
    state.set_view_items(
        tui::View::Rules,
        config.rules.iter().map(|rule| {
            format!(
                "{}\t身份={}\t优先级={}",
                rule.id, rule.profile, rule.priority
            )
        }),
    );
    state.set_view_items(tui::View::Settings, tui_setting_rows(config));
}

fn tui_setting_rows(config: &Config) -> Vec<String> {
    vec![
        format!(
            "auto_bind\t{}\n  含义：唯一规则匹配后自动绑定当前仓库",
            if config.behavior.auto_bind {
                "开启"
            } else {
                "关闭"
            }
        ),
        format!(
            "default_profile\t{}\n  含义：无绑定且无规则匹配时使用的默认 Profile",
            config.behavior.default_profile.as_deref().unwrap_or("无")
        ),
        format!(
            "display_identity\t{}\n  含义：控制执行 Git 或 gh 操作前何时显示 Profile",
            display_identity_value(config.behavior.display_identity)
        ),
        format!(
            "unresolved\t{}\n  含义：无法唯一解析 Profile 时继续警告或停止操作",
            match config.behavior.unresolved {
                UnresolvedPolicy::WarnAndContinue => "warn-and-continue",
                UnresolvedPolicy::Fail => "fail",
            }
        ),
        "credential_failure\tfail（v1 固定）\n  含义：凭据不可用时停止，防止回退到其他账号".into(),
        format!(
            "ssh_unmanaged\t{}\n  含义：Profile 未纳管 SSH key 时继续警告或停止操作",
            match config.behavior.ssh_unmanaged {
                SshUnmanagedPolicy::WarnAndContinue => "warn-and-continue",
                SshUnmanagedPolicy::Fail => "fail",
            }
        ),
    ]
}

fn tui_diagnostics_with_accounts(
    mut diagnostics: Vec<String>,
    discovery: Option<&github::GhDiscovery>,
) -> Vec<String> {
    if let Some(discovery) = discovery {
        diagnostics.extend(discovery.accounts.iter().map(|account| {
            format!(
                "gh 账号\t{}/{}\t{}",
                account.host,
                account.login,
                tui_account_status(account)
            )
        }));
    } else {
        diagnostics.push("gh 账号缓存\t暂无（按 r 后台刷新）".into());
    }
    diagnostics
}

fn refresh_tui_discovery_views(
    state: &mut tui::AppState,
    config: &Config,
    discovery: Option<&github::GhDiscovery>,
) {
    state.set_view_items(tui::View::Profiles, tui_profile_rows(config, discovery));
    let diagnostics = state
        .view_items(tui::View::Diagnostics)
        .iter()
        .filter(|item| !item.starts_with("gh 账号\t") && !item.starts_with("gh 账号缓存\t"))
        .cloned()
        .collect::<Vec<_>>();
    state.set_view_items(
        tui::View::Diagnostics,
        tui_diagnostics_with_accounts(diagnostics, discovery),
    );
}

const TUI_DISCOVERED_ACCOUNT_MARKER: &str = "gh 已发现账号";

fn tui_profile_rows(config: &Config, discovery: Option<&github::GhDiscovery>) -> Vec<String> {
    let mut rows = config
        .profiles
        .iter()
        .map(|(id, profile)| {
            format!(
                "{id}\t{} <{}>\t{}/{}",
                profile.git_name, profile.git_email, profile.host, profile.login
            )
        })
        .collect::<Vec<_>>();
    let configured = config
        .profiles
        .values()
        .map(|profile| {
            (
                github::normalize_host(&profile.host),
                profile.login.to_ascii_lowercase(),
            )
        })
        .collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    if let Some(discovery) = discovery {
        let mut accounts = discovery.accounts.iter().collect::<Vec<_>>();
        accounts.sort_by_key(|account| {
            (
                github::normalize_host(&account.host),
                account.login.to_ascii_lowercase(),
            )
        });
        for account in accounts {
            let key = (
                github::normalize_host(&account.host),
                account.login.to_ascii_lowercase(),
            );
            if configured.contains(&key) || !seen.insert(key) {
                continue;
            }
            rows.push(format!(
                "{TUI_DISCOVERED_ACCOUNT_MARKER}\t{}\t{}\t{}（尚未创建身份配置）",
                account.host,
                account.login,
                tui_account_status(account)
            ));
        }
    }
    rows
}

fn tui_account_status(account: &github::GhAccount) -> String {
    if account.verified {
        if account.active {
            "可用，gh 当前活动账号".into()
        } else {
            "可用".into()
        }
    } else if let Some(error) = account.error.as_deref() {
        format!("未验证：{}", error.replace(['\t', '\n', '\r'], " "))
    } else {
        "未验证（可能离线）".into()
    }
}

fn schedule_tui_local_refresh(
    state: &mut tui::AppState,
    active_workers: &mut usize,
    action_context: &TuiActionContext<'_>,
) -> app::Result<()> {
    action_context.workers.local_refresh(
        action_context.path.map(Path::to_path_buf),
        action_context.explicit.map(str::to_owned),
        action_context.cwd.to_path_buf(),
    )?;
    *active_workers += 1;
    state.loading = true;
    Ok(())
}

fn reload_tui_config_views(
    state: &mut tui::AppState,
    path: Option<&Path>,
    discovery: Option<&github::GhDiscovery>,
) -> app::Result<()> {
    let (_, config) = load_config(path)?;
    apply_tui_config_views(state, &config, discovery);
    Ok(())
}

fn handle_tui_action(
    state: &mut tui::AppState,
    action: tui::Action,
    pending: &mut Option<TuiPending>,
    discovery: &mut Option<github::GhDiscovery>,
    active_workers: &mut usize,
    action_context: &TuiActionContext<'_>,
) -> app::Result<()> {
    let TuiActionContext {
        workers,
        path,
        explicit,
        cwd,
        cache_path: _,
    } = *action_context;
    if state.loading
        && !matches!(
            action,
            tui::Action::None
                | tui::Action::Tick
                | tui::Action::Quit
                | tui::Action::Help
                | tui::Action::Cancel
        )
    {
        state.status = "后台任务尚未完成；仍可浏览、搜索或退出".into();
        return Ok(());
    }
    match action {
        tui::Action::None | tui::Action::Quit => {}
        tui::Action::Tick => {
            receive_tui_worker_results(state, pending, discovery, active_workers, action_context)?;
        }
        tui::Action::Refresh => {
            schedule_tui_local_refresh(state, active_workers, action_context)?;
            workers.discover()?;
            *active_workers += 1;
            state.details = vec!["正在后台刷新本地状态和 gh 登录状态".into()];
            state.status = "刷新任务已在后台启动；界面仍可操作".into();
        }
        tui::Action::Bind | tui::Action::Activate if state.view == tui::View::Profiles => {
            let (_, config) = load_config(path)?;
            match selected_tui_profile(state, &config)? {
                TuiProfileSelection::Configured(id) => {
                    workers.bind(
                        path.map(Path::to_path_buf),
                        id.clone(),
                        explicit.map(str::to_owned),
                        cwd.to_path_buf(),
                    )?;
                    *active_workers += 1;
                    state.loading = true;
                    state.details = vec![format!("正在后台绑定身份配置 `{id}`")];
                    state.status = "绑定任务已在后台启动；界面仍可响应".into();
                }
                TuiProfileSelection::Discovered { host, login } => {
                    workers.emails(host.clone(), login.clone())?;
                    *pending = Some(TuiPending::LoadingDiscoveredProfile {
                        host: host.clone(),
                        login: login.clone(),
                        bind_after_create: action == tui::Action::Bind,
                    });
                    *active_workers += 1;
                    state.loading = true;
                    state.details = vec![
                        format!("待创建账号：{host}/{login}"),
                        "正在后台读取可用提交邮箱".into(),
                    ];
                    state.status = "正在准备身份配置；界面仍可响应".into();
                }
            }
        }
        tui::Action::Unbind => {
            workers.unbind(
                path.map(Path::to_path_buf),
                explicit.map(str::to_owned),
                cwd.to_path_buf(),
            )?;
            *active_workers += 1;
            state.loading = true;
            state.details = vec!["正在后台解除仓库绑定".into()];
            state.status = "解绑任务已在后台启动；界面仍可响应".into();
        }
        tui::Action::Activate if state.view == tui::View::Settings => {
            update_tui_setting(state, path)?;
            reload_tui_config_views(state, path, discovery.as_ref())?;
            schedule_tui_local_refresh(state, active_workers, action_context)?;
            state.status = "设置已更新".into();
        }
        tui::Action::Add if state.view == tui::View::Profiles => {
            let mut wizard = TuiProfileWizard::new("github.com", "", "", false);
            wizard.id = "新身份".into();
            *pending = Some(TuiPending::ProfileWizard(Box::new(wizard.clone())));
            state.details.clear();
            begin_tui_input(
                state,
                "第一段：ID|主机|GitHub 登录名|提交姓名|提交邮箱",
                &wizard.account_input(),
            );
        }
        tui::Action::Edit if state.view == tui::View::Profiles => {
            let (_, config) = load_config(path)?;
            let TuiProfileSelection::Configured(id) = selected_tui_profile(state, &config)? else {
                return Err(app::AppError::Message(
                    "该 gh 账号尚未创建身份配置；请按 Enter 开始创建".into(),
                ));
            };
            let profile = config
                .profiles
                .get(&id)
                .ok_or_else(|| app::AppError::Message(format!("身份配置 `{id}` 不存在")))?;
            let wizard = TuiProfileWizard::from_profile(&id, profile);
            *pending = Some(TuiPending::ProfileWizard(Box::new(wizard.clone())));
            begin_tui_input(
                state,
                "第一段：ID|主机|GitHub 登录名|提交姓名|提交邮箱",
                &wizard.account_input(),
            );
        }
        tui::Action::Add if state.view == tui::View::Rules => {
            *pending = Some(TuiPending::AddRule);
            begin_tui_input(
                state,
                "规则 ID|身份 ID|优先级|主机|所有者|仓库|远端地址模式|Git 目录模式",
                "",
            );
        }
        tui::Action::Edit if state.view == tui::View::Rules => {
            let id = selected_tui_id(state)?;
            let (_, config) = load_config(path)?;
            let rule = config
                .rules
                .iter()
                .find(|rule| rule.id == id)
                .ok_or_else(|| app::AppError::Message(format!("规则 `{id}` 不存在")))?;
            let initial = format!(
                "{}|{}|{}|{}|{}|{}|{}|{}",
                rule.id,
                rule.profile,
                rule.priority,
                rule.host.as_deref().unwrap_or(""),
                rule.owner.as_deref().unwrap_or(""),
                rule.repo.as_deref().unwrap_or(""),
                rule.remote.as_deref().unwrap_or(""),
                rule.gitdir.as_deref().unwrap_or("")
            );
            *pending = Some(TuiPending::EditRule(id));
            begin_tui_input(
                state,
                "规则 ID|身份 ID|优先级|主机|所有者|仓库|远端地址模式|Git 目录模式",
                &initial,
            );
        }
        tui::Action::Delete if state.view == tui::View::Rules => {
            let id = selected_tui_id(state)?;
            *pending = Some(TuiPending::DeleteRule(id.clone()));
            state.input = format!("删除规则 `{id}`");
            state.mode = tui::Mode::Confirm;
        }
        tui::Action::Delete if state.view == tui::View::Profiles => {
            let (_, config) = load_config(path)?;
            let TuiProfileSelection::Configured(id) = selected_tui_profile(state, &config)? else {
                return Err(app::AppError::Message(
                    "缓存中的 gh 账号还不是身份配置，无需删除".into(),
                ));
            };
            *pending = Some(TuiPending::DeleteProfile(id.clone()));
            state.input = format!("删除身份配置 `{id}` 及引用它的规则");
            state.mode = tui::Mode::Confirm;
        }
        tui::Action::SubmitInput => {
            let operation = pending.take();
            match operation {
                Some(TuiPending::ProfileWizard(wizard)) => {
                    submit_tui_profile_wizard(state, *wizard, pending, workers, active_workers)?;
                }
                operation => {
                    apply_tui_input(operation, &state.input, path)?;
                    state.input.clear();
                    reload_tui_config_views(state, path, discovery.as_ref())?;
                    schedule_tui_local_refresh(state, active_workers, action_context)?;
                    state.status = "修改已保存".into();
                }
            }
        }
        tui::Action::Confirm => match pending.take() {
            Some(TuiPending::DeleteRule(id)) => {
                let (paths, _) = load_config(path)?;
                update_config(&paths, |config| {
                    let before = config.rules.len();
                    config.rules.retain(|rule| rule.id != id);
                    if config.rules.len() == before {
                        return Err(app::AppError::Message(format!("规则 `{id}` 已不存在")));
                    }
                    Ok(())
                })?;
                reload_tui_config_views(state, path, discovery.as_ref())?;
                schedule_tui_local_refresh(state, active_workers, action_context)?;
                state.status = format!("已删除规则 `{id}`");
            }
            Some(TuiPending::DeleteProfile(id)) => {
                let (paths, _) = load_config(path)?;
                let (config, ()) = update_config(&paths, |config| remove_profile(config, &id))?;
                app::sync_fragments(&paths, &config)?;
                reload_tui_config_views(state, path, discovery.as_ref())?;
                schedule_tui_local_refresh(state, active_workers, action_context)?;
                state.status = format!("已删除身份配置 `{id}`");
            }
            Some(TuiPending::ProfileWizard(wizard)) => {
                save_tui_profile_wizard(path, &wizard)?;
                if wizard.bind_after_create {
                    let ctx = context(path, Some(&wizard.id), cwd)?;
                    app::bind_repository(&ctx, &wizard.id)?;
                }
                reload_tui_config_views(state, path, discovery.as_ref())?;
                schedule_tui_local_refresh(state, active_workers, action_context)?;
                state.details.clear();
                state.input.clear();
                state.status = if wizard.bind_after_create {
                    format!("已创建并绑定身份配置 `{}`", wizard.id)
                } else if wizard.original_id.is_some() {
                    format!("已更新身份配置 `{}`", wizard.id)
                } else {
                    format!("已创建身份配置 `{}`", wizard.id)
                };
            }
            _ => {}
        },
        tui::Action::Cancel => {
            *pending = None;
            state.details.clear();
            state.status = if state.loading {
                "已关闭当前流程；后台命令仍在运行"
            } else {
                "已取消"
            }
            .into();
        }
        tui::Action::Help => {
            state.status =
                "h/l 切换视图 · j/k 选择 · Enter 操作 · a/e/D 修改 · b/U 绑定 · r 刷新".into();
        }
        _ => {}
    }
    Ok(())
}

fn submit_tui_profile_wizard(
    state: &mut tui::AppState,
    mut wizard: TuiProfileWizard,
    pending: &mut Option<TuiPending>,
    workers: &TuiWorkers,
    active_workers: &mut usize,
) -> app::Result<()> {
    match wizard.phase {
        TuiProfileWizardPhase::Account => {
            if let Err(error) = parse_tui_wizard_account(&mut wizard, &state.input) {
                *pending = Some(TuiPending::ProfileWizard(Box::new(wizard)));
                state.mode = tui::Mode::Insert;
                return Err(error);
            }
            let socket = wizard
                .ssh
                .as_ref()
                .and_then(|ssh| ssh.agent_socket.clone())
                .or_else(|| wizard.agent_socket.clone());
            workers.inspect_agent(socket)?;
            *active_workers += 1;
            state.loading = true;
            state.details = vec![
                format!("第一段完成：{}/{}", wizard.host, wizard.login),
                "正在后台发现 SSH Agent、公钥文件和签名程序".into(),
            ];
            state.status = "第二段准备中；完成后可选择 SSH key，或输入 external 跳过".into();
            wizard.phase = TuiProfileWizardPhase::Ssh;
            *pending = Some(TuiPending::ProfileWizard(Box::new(wizard)));
        }
        TuiProfileWizardPhase::Ssh => {
            if let Some(socket) = tui_agent_socket_needing_inspection(&wizard, &state.input) {
                let submitted_ssh = state.input.clone();
                workers.inspect_agent_for_ssh(socket.clone(), submitted_ssh)?;
                *active_workers += 1;
                state.loading = true;
                state.status = format!(
                    "正在后台检查自定义 SSH Agent socket `{}`",
                    diagnostics::sanitize_display_text(&socket.display().to_string())
                );
                state.details = vec![
                    "自定义 Agent socket 尚未检查；检查完成后会继续第三段".into(),
                    format!(
                        "socket：{}",
                        diagnostics::sanitize_display_text(&socket.display().to_string())
                    ),
                ];
                *pending = Some(TuiPending::ProfileWizard(Box::new(wizard)));
                return Ok(());
            }
            if let Err(error) = parse_tui_wizard_ssh(&mut wizard, &state.input) {
                *pending = Some(TuiPending::ProfileWizard(Box::new(wizard)));
                state.mode = tui::Mode::Insert;
                return Err(error);
            }
            begin_tui_signing_stage(state, &mut wizard);
            *pending = Some(TuiPending::ProfileWizard(Box::new(wizard)));
        }
        TuiProfileWizardPhase::Signing => {
            if let Err(error) = parse_tui_wizard_signing(&mut wizard, &state.input) {
                *pending = Some(TuiPending::ProfileWizard(Box::new(wizard)));
                state.mode = tui::Mode::Insert;
                return Err(error);
            }
            state.details = tui_profile_wizard_preview(&wizard);
            state.set_input(format!("保存身份配置 `{}`", wizard.id));
            state.mode = tui::Mode::Confirm;
            state.status = "第四段：检查完整变更；按 y 或 Enter 保存，按 n 或 Esc 取消".into();
            *pending = Some(TuiPending::ProfileWizard(Box::new(wizard)));
        }
    }
    Ok(())
}

fn begin_tui_signing_stage(state: &mut tui::AppState, wizard: &mut TuiProfileWizard) {
    wizard.phase = TuiProfileWizardPhase::Signing;
    let initial = wizard.signing_input();
    begin_tui_input(
        state,
        "第三段：off，或 on|签名公钥序号/路径|签名程序（程序留空则自动发现）",
        &initial,
    );
    state.details = tui_profile_wizard_key_details(wizard);
}

fn tui_agent_socket_needing_inspection(wizard: &TuiProfileWizard, input: &str) -> Option<PathBuf> {
    if wizard
        .original_profile
        .as_ref()
        .is_some_and(|original| original.ssh == wizard.ssh && input.trim() == wizard.ssh_input())
    {
        return None;
    }
    let fields = input.split('|').map(str::trim).collect::<Vec<_>>();
    let mode = fields.first().copied().unwrap_or_default();
    if !matches!(mode, "one-password" | "1password" | "op" | "managed") {
        return None;
    }
    let requested = fields
        .get(2)
        .filter(|value| !value.is_empty())
        .map(|value| signing::expand_user(Path::new(value)))?;
    let inspected = wizard
        .agent
        .as_ref()
        .filter(|agent| agent.available)
        .map(|agent| signing::expand_user(&agent.socket));
    (inspected.as_ref() != Some(&requested)).then_some(requested)
}

fn parse_tui_wizard_account(wizard: &mut TuiProfileWizard, input: &str) -> app::Result<()> {
    let fields = input.split('|').map(str::trim).collect::<Vec<_>>();
    if fields.len() != 5 || fields.iter().any(|field| field.is_empty()) {
        return Err(app::AppError::Message(
            "第一段需要 ID|主机|GitHub 登录名|提交姓名|提交邮箱，五项都不能为空".into(),
        ));
    }
    if wizard
        .original_id
        .as_deref()
        .is_some_and(|id| id != fields[0])
    {
        return Err(app::AppError::Message(
            "为避免已有仓库绑定失效，编辑时不能修改 Profile ID".into(),
        ));
    }
    wizard.id = fields[0].into();
    wizard.host = github::normalize_host(fields[1]);
    wizard.login = fields[2].into();
    wizard.git_name = fields[3].into();
    wizard.git_email = fields[4].into();
    Ok(())
}

fn parse_tui_wizard_ssh(wizard: &mut TuiProfileWizard, input: &str) -> app::Result<()> {
    if wizard
        .original_profile
        .as_ref()
        .is_some_and(|original| original.ssh == wizard.ssh && input.trim() == wizard.ssh_input())
    {
        return Ok(());
    }
    let fields = input.split('|').map(str::trim).collect::<Vec<_>>();
    let mode = fields.first().copied().unwrap_or_default();
    if matches!(mode, "" | "skip" | "external") {
        if fields.len() != 1 {
            return Err(app::AppError::Message(
                "第二段选择 external 时不需要填写公钥或 Agent socket".into(),
            ));
        }
        wizard.ssh = None;
        return Ok(());
    }
    if !(2..=3).contains(&fields.len()) {
        return Err(app::AppError::Message(
            "第二段使用 external 跳过，或填写 one-password|公钥序号/路径[|Agent socket]".into(),
        ));
    }
    let mode = match mode {
        "one-password" | "1password" | "op" => SshMode::OnePassword,
        "managed" => SshMode::Managed,
        _ => {
            return Err(app::AppError::Message(
                "SSH 模式只支持 external、one-password 或 managed".into(),
            ));
        }
    };
    let socket = if fields.get(2).is_none_or(|value| value.is_empty()) {
        wizard.agent_socket.clone()
    } else {
        Some(signing::expand_user(Path::new(fields[2])))
    }
    .ok_or_else(|| app::AppError::Message("没有发现 SSH Agent socket".into()))?;
    let agent = wizard
        .agent
        .as_ref()
        .filter(|agent| agent.available)
        .ok_or_else(|| app::AppError::Message("所选 SSH Agent 不可用".into()))?;
    if signing::expand_user(&agent.socket) != socket {
        return Err(app::AppError::Message(format!(
            "Agent socket `{}` 尚未经过后台检查；请先等待检查完成后重试",
            diagnostics::sanitize_display_text(&socket.display().to_string())
        )));
    }
    let public_key = resolve_tui_public_key_file(wizard, fields[1])?;
    let fingerprint = public_key
        .fingerprint
        .as_deref()
        .ok_or_else(|| app::AppError::Message("所选公钥没有可验证的 SHA-256 指纹".into()))?;
    ensure_tui_key_matches_agent(agent, &public_key.public_key, fingerprint)?;
    wizard.agent_socket = Some(socket.clone());
    wizard.ssh = Some(SshProfile {
        mode,
        public_key: Some(public_key.path.clone()),
        fingerprint: public_key.fingerprint.clone(),
        agent_socket: Some(socket),
    });
    Ok(())
}

fn resolve_tui_public_key_file(
    wizard: &TuiProfileWizard,
    value: &str,
) -> app::Result<signing::PublicKeyFile> {
    if value.is_empty() {
        return Err(app::AppError::Message("请选择一个 .pub 公钥文件".into()));
    }
    if let Ok(index) = value.parse::<usize>() {
        if index == 0 {
            return Err(app::AppError::Message("公钥序号从 1 开始".into()));
        }
        return wizard
            .public_keys
            .get(index - 1)
            .cloned()
            .ok_or_else(|| app::AppError::Message(format!("公钥序号 `{value}` 不存在")));
    }
    let path = signing::expand_user(Path::new(value));
    signing::load_public_key_file(&path).map_err(|error| {
        app::AppError::Message(format!("无法使用公钥文件 {}：{error}", path.display()))
    })
}

fn parse_tui_wizard_signing(wizard: &mut TuiProfileWizard, input: &str) -> app::Result<()> {
    if wizard.original_profile.as_ref().is_some_and(|original| {
        original.signing == wizard.signing && input.trim() == wizard.signing_input()
    }) {
        return Ok(());
    }
    let fields = input.split('|').map(str::trim).collect::<Vec<_>>();
    match fields.first().copied().unwrap_or_default() {
        "" | "off" | "false" => {
            if fields.len() != 1 {
                return Err(app::AppError::Message(
                    "第三段关闭签名时只需填写 off".into(),
                ));
            }
            wizard.signing = SigningProfile::default();
            Ok(())
        }
        "on" | "true" => {
            if !(2..=3).contains(&fields.len()) {
                return Err(app::AppError::Message(
                    "第三段使用 off，或填写 on|签名公钥序号/路径|签名程序".into(),
                ));
            }
            let signing_key = resolve_tui_signing_key(wizard, fields[1])?;
            let agent = wizard
                .agent
                .as_ref()
                .filter(|agent| agent.available)
                .ok_or_else(|| app::AppError::Message("SSH Agent 不可用，不能开启签名".into()))?;
            ensure_tui_key_matches_agent(agent, &signing_key.public_key, &signing_key.fingerprint)?;
            let program = fields
                .get(2)
                .filter(|value| !value.is_empty())
                .map(|value| signing::SigningProgram {
                    path: signing::expand_user(Path::new(value)),
                    onepassword: value.to_ascii_lowercase().contains("op-ssh-sign"),
                })
                .or_else(|| {
                    wizard
                        .signing
                        .program
                        .clone()
                        .map(|path| signing::SigningProgram {
                            onepassword: path
                                .to_string_lossy()
                                .to_ascii_lowercase()
                                .contains("op-ssh-sign"),
                            path,
                        })
                })
                .or_else(|| wizard.signing_program.clone())
                .ok_or_else(|| app::AppError::Message("未发现 SSH signing program".into()))?;
            if !signing::signing_program_available(&program) {
                return Err(app::AppError::Message(format!(
                    "签名程序 {} 不可执行",
                    program.path.display()
                )));
            }
            wizard.signing = SigningProfile {
                enabled: true,
                signing_key: Some(signing_key.config_value),
                program: Some(program.path),
            };
            Ok(())
        }
        _ => Err(app::AppError::Message(
            "第三段只接受 off，或 on|签名公钥序号/路径|签名程序".into(),
        )),
    }
}

fn resolve_tui_signing_key(
    wizard: &TuiProfileWizard,
    value: &str,
) -> app::Result<TuiResolvedSigningKey> {
    if value.is_empty() {
        return Err(app::AppError::Message("请选择一个签名公钥".into()));
    }
    let inline = value.strip_prefix("key::").unwrap_or(value);
    if inline.starts_with("ssh-")
        || inline.starts_with("ecdsa-")
        || inline.starts_with("sk-")
        || inline.starts_with("rsa-sha2-")
    {
        let fingerprint = signing::fingerprint(inline)
            .map_err(|error| app::AppError::Message(format!("内联签名公钥无效：{error}")))?;
        return Ok(TuiResolvedSigningKey {
            config_value: signing::git_signing_key_value(value),
            public_key: inline.into(),
            fingerprint,
        });
    }
    let key = resolve_tui_public_key_file(wizard, value)?;
    Ok(TuiResolvedSigningKey {
        config_value: key.path.to_string_lossy().into_owned(),
        public_key: key.public_key,
        fingerprint: key
            .fingerprint
            .expect("validated public-key files always have a fingerprint"),
    })
}

fn ensure_tui_key_matches_agent(
    agent: &signing::AgentInfo,
    public_key: &str,
    fingerprint: &str,
) -> app::Result<()> {
    let selector = signing::SigningProfile {
        public_key: Some(public_key.into()),
        fingerprint: Some(fingerprint.into()),
        ..signing::SigningProfile::default()
    };
    if signing::select_key(&agent.keys, &selector).is_none() {
        return Err(app::AppError::Message(
            "所选 .pub 文件与 SSH Agent 中的 key 不匹配".into(),
        ));
    }
    Ok(())
}

fn tui_profile_wizard_key_details(wizard: &TuiProfileWizard) -> Vec<String> {
    let mut details = vec![format!(
        "第二段结果：SSH 远程连接={}；第三段单独选择签名公钥",
        wizard
            .ssh
            .as_ref()
            .map(|ssh| match ssh.mode {
                SshMode::External => "external",
                SshMode::OnePassword => "one-password",
                SshMode::Managed => "managed",
            })
            .unwrap_or("external")
    )];
    details.extend(tui_agent_discovery_details(wizard));
    if !wizard.signing.enabled
        && let Some(path) = wizard
            .ssh
            .as_ref()
            .and_then(|ssh| ssh.public_key.as_deref())
    {
        details.push(format!(
            "复用第二段公钥开启签名：on|{}|",
            diagnostics::sanitize_display_text(&path.display().to_string())
        ));
    }
    if let Some(program) = wizard.signing_program.as_ref() {
        details.push(format!(
            "自动发现签名程序：{}",
            diagnostics::sanitize_display_text(&program.path.display().to_string())
        ));
    } else {
        details.push("自动发现签名程序：未找到".into());
    }
    details
}

fn tui_profile_wizard_preview(wizard: &TuiProfileWizard) -> Vec<String> {
    let ssh = wizard.ssh.as_ref();
    let mut preview = vec![
        format!(
            "账号：{}/{}",
            diagnostics::sanitize_display_text(&wizard.host),
            diagnostics::sanitize_display_text(&wizard.login)
        ),
        format!(
            "提交：{} <{}>",
            diagnostics::sanitize_display_text(&wizard.git_name),
            diagnostics::sanitize_display_text(&wizard.git_email)
        ),
        format!(
            "SSH 连接：{}",
            ssh.map(|ssh| match ssh.mode {
                SshMode::External => "external",
                SshMode::OnePassword => "one-password",
                SshMode::Managed => "managed",
            })
            .unwrap_or("external")
        ),
        format!(
            "SSH 公钥：{}",
            ssh.and_then(|ssh| ssh.public_key.as_deref())
                .map(|path| diagnostics::sanitize_display_text(&path.to_string_lossy()))
                .unwrap_or_else(|| "不纳管".into())
        ),
        format!(
            "提交签名：{}",
            if wizard.signing.enabled {
                "SSH signing 已开启"
            } else {
                "关闭"
            }
        ),
    ];
    if wizard.signing.enabled {
        preview.push(format!(
            "签名公钥：{}",
            diagnostics::sanitize_display_text(
                wizard.signing.signing_key.as_deref().unwrap_or("未配置")
            )
        ));
        preview.push(format!(
            "签名程序：{}",
            wizard
                .signing
                .program
                .as_deref()
                .map(|path| diagnostics::sanitize_display_text(&path.to_string_lossy()))
                .unwrap_or_else(|| "未配置".into())
        ));
    }
    preview.push("尚未写入配置；确认后将原子保存并同步 Profile fragment".into());
    preview
}

fn receive_tui_worker_results(
    state: &mut tui::AppState,
    pending: &mut Option<TuiPending>,
    discovery: &mut Option<github::GhDiscovery>,
    active_workers: &mut usize,
    action_context: &TuiActionContext<'_>,
) -> app::Result<()> {
    let TuiActionContext {
        workers,
        path,
        explicit: _,
        cwd: _,
        cache_path,
    } = *action_context;
    while let Ok(message) = workers.receiver.try_recv() {
        *active_workers = active_workers.saturating_sub(1);
        state.loading = *active_workers > 0;
        match message {
            TuiWorkerResult::LocalRefresh(result) => match result {
                Ok(snapshot) => {
                    apply_tui_local_snapshot(state, snapshot, discovery.as_ref());
                    state.status = if state.loading {
                        "本地状态刷新完成，仍有后台任务在运行"
                    } else {
                        "本地状态刷新完成"
                    }
                    .into();
                    if !state.loading {
                        state.details.clear();
                    }
                }
                Err(error) => {
                    state.status = format!("本地状态后台刷新失败：{error}");
                    state.warnings.push(format!("本地状态刷新失败：{error}"));
                    if !state.loading {
                        state.details.clear();
                    }
                }
            },
            TuiWorkerResult::Discovery(result) => match result {
                Ok(updated) => {
                    let count = updated.accounts.len();
                    let offline = updated.offline;
                    let cache_warning = github::save_discovery_cache(cache_path, &updated)
                        .err()
                        .map(|error| format!("无法更新 gh 账号缓存：{error}"));
                    state
                        .warnings
                        .retain(|warning| !warning.contains("gh 账号缓存"));
                    *discovery = Some(updated);
                    let (_, config) = load_config(path)?;
                    refresh_tui_discovery_views(state, &config, discovery.as_ref());
                    if !state.loading {
                        state.details.clear();
                    }
                    state.status = format!(
                        "gh 账号刷新完成：发现 {count} 个账号{}",
                        if offline {
                            "（当前有账号未联网验证）"
                        } else {
                            ""
                        }
                    );
                    if let Some(warning) = cache_warning {
                        state.warnings.push(warning);
                    }
                }
                Err(error) => {
                    if !state.loading {
                        state.details.clear();
                    }
                    state.status = format!("gh 账号后台刷新失败：{error}");
                    state.warnings.push(format!("gh 账号刷新失败：{error}"));
                }
            },
            TuiWorkerResult::Emails {
                host,
                login,
                result,
            } => {
                let operation = pending.take();
                let Some(TuiPending::LoadingDiscoveredProfile {
                    host: expected_host,
                    login: expected_login,
                    bind_after_create,
                }) = operation
                else {
                    continue;
                };
                if !host.eq_ignore_ascii_case(&expected_host)
                    || !login.eq_ignore_ascii_case(&expected_login)
                {
                    state
                        .warnings
                        .push("收到不匹配的后台邮箱查询结果，已忽略".into());
                    continue;
                }

                let (candidates, warning) = match result {
                    Ok(candidates) => (candidates, None),
                    Err(error) => (
                        Vec::new(),
                        Some(format!("无法读取 {host}/{login} 的邮箱候选：{error}")),
                    ),
                };
                let (_, config) = load_config(path)?;
                let id = suggested_tui_profile_id(&config, &host, &login);
                let email = candidates
                    .first()
                    .map(|candidate| candidate.email.as_str())
                    .unwrap_or_default();
                let mut wizard = TuiProfileWizard::new(&host, &login, email, bind_after_create);
                wizard.id = id;
                wizard.git_name = login.clone();
                state.details = tui_email_candidate_details(&host, &login, &candidates);
                begin_tui_input(
                    state,
                    "第一段：ID|主机|GitHub 登录名|提交姓名|提交邮箱",
                    &wizard.account_input(),
                );
                state.status = if candidates.is_empty() {
                    "没有可预填的邮箱；请补全提交邮箱后继续四段式配置"
                } else if candidates
                    .first()
                    .is_some_and(|candidate| candidate.noreply)
                {
                    "已预填 GitHub noreply 邮箱；可编辑后继续 SSH 与签名设置"
                } else {
                    "已预填首选邮箱；可编辑后继续 SSH 与签名设置"
                }
                .into();
                if let Some(warning) = warning {
                    state.warnings.push(warning);
                }
                *pending = Some(TuiPending::ProfileWizard(Box::new(wizard)));
            }
            TuiWorkerResult::Agent {
                submitted_ssh,
                result,
            } => {
                let Some(TuiPending::ProfileWizard(wizard)) = pending.take() else {
                    continue;
                };
                let mut wizard = *wizard;
                match result {
                    Ok(discovery_result) => {
                        wizard.agent_socket = discovery_result.socket;
                        wizard.agent = discovery_result.agent;
                        wizard.public_keys = discovery_result.public_keys;
                        wizard.signing_program = discovery_result.signing_program;
                        if let Some(input) = submitted_ssh {
                            match parse_tui_wizard_ssh(&mut wizard, &input) {
                                Ok(()) => {
                                    begin_tui_signing_stage(state, &mut wizard);
                                    state.status =
                                        "自定义 SSH Agent 检查完成；进入第三段签名设置".into();
                                }
                                Err(error) => {
                                    let error_text =
                                        diagnostics::sanitize_display_text(&error.to_string());
                                    state.details = tui_agent_discovery_details(&wizard);
                                    begin_tui_input(
                                        state,
                                        "第二段：external，或 one-password|公钥序号/路径|Agent socket",
                                        &input,
                                    );
                                    state.status = format!("第二段校验失败：{error_text}");
                                    state.warnings.push(error_text);
                                }
                            }
                        } else {
                            state.details = tui_agent_discovery_details(&wizard);
                            let initial = wizard.ssh_input();
                            begin_tui_input(
                                state,
                                "第二段：external，或 one-password|公钥序号/路径|Agent socket",
                                &initial,
                            );
                            state.status =
                                "已完成只读发现；选择列表中的 .pub，或输入 external 跳过 SSH"
                                    .into();
                        }
                    }
                    Err(error) => {
                        let error_text = diagnostics::sanitize_display_text(&error.to_string());
                        state.warnings.push(format!("SSH 发现失败：{error_text}"));
                        state.details =
                            vec!["SSH Agent/.pub 发现失败；仍可选择 external 跳过 SSH".into()];
                        let initial = submitted_ssh.unwrap_or_else(|| wizard.ssh_input());
                        begin_tui_input(
                            state,
                            "第二段：external，或 one-password|公钥序号/路径|Agent socket",
                            &initial,
                        );
                    }
                }
                *pending = Some(TuiPending::ProfileWizard(Box::new(wizard)));
            }
            TuiWorkerResult::RepositoryMutation { operation, result } => match result {
                Ok(snapshot) => {
                    apply_tui_local_snapshot(state, snapshot, discovery.as_ref());
                    state.status = match operation {
                        TuiRepositoryMutation::Bind(id) => {
                            format!("已绑定身份配置 `{id}`")
                        }
                        TuiRepositoryMutation::Unbind => "已解除仓库绑定".into(),
                    };
                    if !state.loading {
                        state.details.clear();
                    }
                }
                Err(error) => {
                    let action = match operation {
                        TuiRepositoryMutation::Bind(_) => "绑定",
                        TuiRepositoryMutation::Unbind => "解绑",
                    };
                    state.status = format!("{action}后台任务失败：{error}");
                    state.warnings.push(format!("{action}失败：{error}"));
                    if !state.loading {
                        state.details.clear();
                    }
                }
            },
        }
    }
    Ok(())
}

fn tui_email_candidate_details(
    host: &str,
    login: &str,
    candidates: &[github::EmailCandidate],
) -> Vec<String> {
    let mut details = vec![
        format!(
            "待创建账号：{}/{}",
            diagnostics::sanitize_display_text(host),
            diagnostics::sanitize_display_text(login)
        ),
        "提交邮箱候选：".into(),
    ];
    if candidates.is_empty() {
        details.push("  未找到，请手动填写".into());
    } else {
        details.extend(candidates.iter().map(|candidate| {
            let mut labels = Vec::new();
            if candidate.noreply {
                labels.push("GitHub noreply");
            }
            if candidate.primary {
                labels.push("首选");
            }
            if candidate.verified {
                labels.push("已验证");
            }
            if labels.is_empty() {
                format!("  {}", diagnostics::sanitize_display_text(&candidate.email))
            } else {
                format!(
                    "  {}（{}）",
                    diagnostics::sanitize_display_text(&candidate.email),
                    labels.join("，")
                )
            }
        }));
    }
    details
}

fn tui_agent_discovery_details(wizard: &TuiProfileWizard) -> Vec<String> {
    let socket = wizard
        .agent_socket
        .as_deref()
        .map(|path| diagnostics::sanitize_display_text(&path.to_string_lossy()))
        .unwrap_or_else(|| "未发现".into());
    let mut details = vec![format!("SSH Agent：{socket}")];
    if let Some(agent) = wizard.agent.as_ref() {
        details.push(format!(
            "Agent 状态：{}，{} 把 key",
            if agent.available {
                "可用"
            } else {
                "不可用"
            },
            agent.keys.len()
        ));
        if let Some(error) = agent.error.as_deref() {
            details.push(format!(
                "Agent 错误：{}",
                diagnostics::sanitize_display_text(error)
            ));
        }
        if !agent.keys.is_empty() {
            details.push("Agent 中的公钥：".into());
            details.extend(agent.keys.iter().map(|key| {
                format!(
                    "  {} · {} · {}",
                    diagnostics::sanitize_display_text(&key.key_type),
                    diagnostics::sanitize_display_text(key.comment.as_deref().unwrap_or("无注释")),
                    diagnostics::sanitize_display_text(
                        key.fingerprint.as_deref().unwrap_or("无指纹")
                    )
                )
            }));
        }
    }
    if wizard.public_keys.is_empty() {
        details.push("~/.ssh 中未发现有效 .pub 文件".into());
    } else {
        details.push("可选公钥（输入序号或完整路径）：".into());
        details.extend(wizard.public_keys.iter().enumerate().map(|(index, key)| {
            let matched = wizard.agent.as_ref().is_some_and(|agent| {
                signing::select_key(
                    &agent.keys,
                    &signing::SigningProfile {
                        public_key: Some(key.public_key.clone()),
                        fingerprint: key.fingerprint.clone(),
                        ..signing::SigningProfile::default()
                    },
                )
                .is_some()
            });
            format!(
                "  {}. {} · {} · {}",
                index + 1,
                diagnostics::sanitize_display_text(&key.path.display().to_string()),
                diagnostics::sanitize_display_text(key.fingerprint.as_deref().unwrap_or("无指纹")),
                if matched {
                    "Agent 已匹配"
                } else {
                    "Agent 未匹配"
                }
            )
        }));
    }
    details
}

fn selected_tui_profile(
    state: &tui::AppState,
    config: &Config,
) -> app::Result<TuiProfileSelection> {
    let item = state
        .selected_item()
        .ok_or_else(|| app::AppError::Message("当前没有可操作的 Profile 或 gh 账号".into()))?;
    let fields = item.split('\t').collect::<Vec<_>>();
    if fields.len() >= 4 && fields[0] == TUI_DISCOVERED_ACCOUNT_MARKER {
        return Ok(TuiProfileSelection::Discovered {
            host: fields[1].to_owned(),
            login: fields[2].to_owned(),
        });
    }
    let id = fields.first().copied().unwrap_or_default();
    if config.profiles.contains_key(id) {
        Ok(TuiProfileSelection::Configured(id.to_owned()))
    } else {
        Err(app::AppError::Message(format!(
            "Profile `{id}` 不存在；请按 r 更新账号列表"
        )))
    }
}

fn suggested_tui_profile_id(config: &Config, host: &str, login: &str) -> String {
    if !config.profiles.contains_key(login) {
        return login.to_owned();
    }
    let base = format!("{login}@{}", github::normalize_host(host));
    if !config.profiles.contains_key(&base) {
        return base;
    }
    for suffix in 2usize.. {
        let candidate = format!("{base}-{suffix}");
        if !config.profiles.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!("有限配置不可能耗尽所有数字后缀")
}

fn save_tui_profile_wizard(path: Option<&Path>, wizard: &TuiProfileWizard) -> app::Result<()> {
    let (paths, _) = load_config(path)?;
    save_tui_profile_wizard_at_paths(&paths, wizard)
}

fn save_tui_profile_wizard_at_paths(
    paths: &ConfigPaths,
    wizard: &TuiProfileWizard,
) -> app::Result<()> {
    let profile = wizard.profile();
    let (config, ()) = update_config(paths, |config| {
        if let Some(original) = wizard.original_id.as_deref() {
            let current = config
                .profiles
                .get_mut(original)
                .ok_or_else(|| app::AppError::Message(format!("Profile `{original}` 已不存在")))?;
            if wizard
                .original_profile
                .as_ref()
                .is_some_and(|expected| current != expected)
            {
                return Err(app::AppError::Message(format!(
                    "Profile `{original}` 在编辑期间已被其他操作修改；未覆盖，请重新打开向导"
                )));
            }
            *current = profile;
        } else {
            if config.profiles.contains_key(&wizard.id) {
                return Err(app::AppError::Message(format!(
                    "Profile `{}` 已存在，请返回后换一个 ID",
                    wizard.id
                )));
            }
            config.profiles.insert(wizard.id.clone(), profile);
        }
        Ok(())
    })?;
    app::sync_fragments(paths, &config)?;
    Ok(())
}

fn selected_tui_id(state: &tui::AppState) -> app::Result<String> {
    state
        .selected_item()
        .and_then(|item| item.split('\t').next())
        .map(str::to_owned)
        .ok_or_else(|| app::AppError::Message("当前没有可操作的项目".into()))
}

fn begin_tui_input(state: &mut tui::AppState, label: &str, initial: &str) {
    state.mode = tui::Mode::Insert;
    state.status = label.into();
    state.set_input(initial);
}

fn apply_tui_input(
    operation: Option<TuiPending>,
    input: &str,
    path: Option<&Path>,
) -> app::Result<()> {
    let Some(operation) = operation else {
        return Ok(());
    };
    if matches!(
        &operation,
        TuiPending::ProfileWizard(_)
            | TuiPending::DeleteRule(_)
            | TuiPending::DeleteProfile(_)
            | TuiPending::LoadingDiscoveredProfile { .. }
    ) {
        return Ok(());
    }
    let (paths, _) = load_config(path)?;
    apply_tui_input_at_paths(operation, input, &paths)
}

/// Apply a TUI edit after the caller has resolved the configuration paths.
///
/// Keeping path resolution outside this mutation makes in-process callers
/// (especially tests) able to provide an isolated XDG root without changing
/// the environment of the whole test process.
fn apply_tui_input_at_paths(
    operation: TuiPending,
    input: &str,
    paths: &ConfigPaths,
) -> app::Result<()> {
    let (config, ()) = update_config(paths, |config| {
        match operation {
            TuiPending::AddRule => {
                let rule = parse_tui_rule(input)?;
                if config.rules.iter().any(|existing| existing.id == rule.id) {
                    return Err(app::AppError::Message(format!("规则 `{}` 已存在", rule.id)));
                }
                config.rules.push(rule);
            }
            TuiPending::EditRule(previous) => {
                let updated = parse_tui_rule(input)?;
                if updated.id != previous && config.rules.iter().any(|rule| rule.id == updated.id) {
                    return Err(app::AppError::Message(format!(
                        "规则 `{}` 已存在",
                        updated.id
                    )));
                }
                let rule = config
                    .rules
                    .iter_mut()
                    .find(|rule| rule.id == previous)
                    .ok_or_else(|| app::AppError::Message(format!("规则 `{previous}` 不存在")))?;
                *rule = updated;
            }
            _ => unreachable!("non-input TUI operations returned before the transaction"),
        }
        Ok(())
    })?;
    app::sync_fragments(paths, &config)?;
    Ok(())
}

fn parse_tui_rule(input: &str) -> app::Result<Rule> {
    let fields = input.split('|').map(str::trim).collect::<Vec<_>>();
    if fields.len() != 8 || fields[0].is_empty() || fields[1].is_empty() {
        return Err(app::AppError::Message(
            "规则输入需要 ID|身份 ID|优先级|主机|所有者|仓库|远端地址模式|Git 目录模式".into(),
        ));
    }
    let priority = fields[2]
        .parse::<i32>()
        .map_err(|_| app::AppError::Message("规则优先级必须是整数".into()))?;
    let optional = |index: usize| (!fields[index].is_empty()).then(|| fields[index].into());
    Ok(Rule {
        id: fields[0].into(),
        profile: fields[1].into(),
        priority,
        host: optional(3),
        owner: optional(4),
        repo: optional(5),
        remote: optional(6),
        gitdir: optional(7),
    })
}

fn update_tui_setting(state: &tui::AppState, path: Option<&Path>) -> app::Result<()> {
    let selected = state
        .selected_item()
        .ok_or_else(|| app::AppError::Message("当前没有可修改的设置".into()))?;
    let key = selected.split_whitespace().next().unwrap_or_default();
    let (paths, _) = load_config(path)?;
    update_config(&paths, |config| {
        match key {
            "auto_bind" => config.behavior.auto_bind = !config.behavior.auto_bind,
            "display_identity" => {
                config.behavior.display_identity = match config.behavior.display_identity {
                    DisplayIdentity::Always => DisplayIdentity::SensitiveCommands,
                    DisplayIdentity::SensitiveCommands => DisplayIdentity::Never,
                    DisplayIdentity::Never => DisplayIdentity::Always,
                }
            }
            "unresolved" => {
                config.behavior.unresolved = match config.behavior.unresolved {
                    UnresolvedPolicy::WarnAndContinue => UnresolvedPolicy::Fail,
                    UnresolvedPolicy::Fail => UnresolvedPolicy::WarnAndContinue,
                }
            }
            "ssh_unmanaged" => {
                config.behavior.ssh_unmanaged = match config.behavior.ssh_unmanaged {
                    SshUnmanagedPolicy::WarnAndContinue => SshUnmanagedPolicy::Fail,
                    SshUnmanagedPolicy::Fail => SshUnmanagedPolicy::WarnAndContinue,
                }
            }
            "credential_failure" => {
                return Err(app::AppError::Message(
                    "v1 为防止账号回退，credential_failure 固定为 fail".into(),
                ));
            }
            "default_profile" => {
                let mut values = std::iter::once(None)
                    .chain(config.profiles.keys().cloned().map(Some))
                    .collect::<Vec<_>>();
                if values.is_empty() {
                    values.push(None);
                }
                let current = values
                    .iter()
                    .position(|value| value.as_ref() == config.behavior.default_profile.as_ref())
                    .unwrap_or(0);
                config.behavior.default_profile = values[(current + 1) % values.len()].clone();
            }
            _ => return Err(app::AppError::Message(format!("设置 `{key}` 不可修改"))),
        }
        Ok(())
    })?;
    Ok(())
}

fn display_identity_value(value: DisplayIdentity) -> &'static str {
    match value {
        DisplayIdentity::Always => "always",
        DisplayIdentity::SensitiveCommands => "sensitive-commands",
        DisplayIdentity::Never => "never",
    }
}

fn git_working_directory(args: &[String]) -> app::Result<PathBuf> {
    let mut directory = std::env::current_dir()?;
    let mut git_dir = None;
    let mut work_tree = None;
    let mut index = 0usize;
    while index < args.len() {
        if args[index] == "-C" {
            let path = args
                .get(index + 1)
                .ok_or_else(|| app::AppError::Message("git -C 缺少路径".into()))?;
            directory = join_git_c_directory(&directory, Path::new(path));
            index += 2;
            continue;
        }
        if let Some(path) = args[index].strip_prefix("-C")
            && !path.is_empty()
        {
            directory = join_git_c_directory(&directory, Path::new(path));
            index += 1;
            continue;
        }
        if let Some(value) = args[index].strip_prefix("--git-dir=") {
            git_dir = Some(PathBuf::from(value));
            index += 1;
            continue;
        }
        if let Some(value) = args[index].strip_prefix("--work-tree=") {
            work_tree = Some(PathBuf::from(value));
            index += 1;
            continue;
        }
        if args[index].starts_with("--namespace=")
            || args[index].starts_with("--super-prefix=")
            || args[index].starts_with("--config-env=")
        {
            index += 1;
            continue;
        }
        if args[index] == "--" || !args[index].starts_with('-') {
            break;
        }
        if matches!(args[index].as_str(), "--git-dir" | "--work-tree") {
            let value = args
                .get(index + 1)
                .ok_or_else(|| app::AppError::Message(format!("git {} 缺少路径", args[index])))?;
            if args[index] == "--git-dir" {
                git_dir = Some(PathBuf::from(value));
            } else {
                work_tree = Some(PathBuf::from(value));
            }
            index += 2;
            continue;
        }
        if matches!(
            args[index].as_str(),
            "-c" | "--namespace" | "--super-prefix" | "--config-env"
        ) {
            if args.get(index + 1).is_none() {
                return Err(app::AppError::Message(format!(
                    "git {} 缺少参数",
                    args[index]
                )));
            }
            index += 2;
            continue;
        }
        index += 1;
    }

    // Git applies every `-C` first; relative --git-dir/--work-tree values are
    // then interpreted from that final directory, regardless of option order.
    if let Some(git_dir) = git_dir {
        let git_dir = join_git_c_directory(&directory, &git_dir);
        return Ok(repository_hint_from_git_dir(&git_dir));
    }
    if let Some(work_tree) = work_tree {
        return Ok(join_git_c_directory(&directory, &work_tree));
    }
    Ok(directory)
}

fn repository_hint_from_git_dir(git_dir: &Path) -> PathBuf {
    if git_dir.file_name().is_some_and(|name| name == ".git") {
        return git_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| git_dir.to_path_buf());
    }

    // A linked worktree's private git directory contains a `gitdir` file that
    // points back to `<worktree>/.git`.
    if let Ok(pointer) = fs::read_to_string(git_dir.join("gitdir")) {
        let worktree_dot_git = PathBuf::from(pointer.trim());
        if worktree_dot_git
            .file_name()
            .is_some_and(|name| name == ".git")
            && let Some(worktree) = worktree_dot_git.parent()
        {
            return worktree.to_path_buf();
        }
    }
    git_dir.to_path_buf()
}

fn join_git_c_directory(base: &Path, next: &Path) -> PathBuf {
    if next.is_absolute() {
        next.to_path_buf()
    } else {
        base.join(next)
    }
}

fn print_json<T: Serialize>(value: &T) -> app::Result<()> {
    serde_json::to_writer_pretty(io::stdout(), value)
        .map_err(|error| app::AppError::Message(error.to_string()))?;
    println!();
    Ok(())
}

fn write_stdout(bytes: &[u8]) -> app::Result<()> {
    match io::stdout().write_all(bytes) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_profile_email_rejects_blank_input() {
        let error = resolve_profile_email("github.com", "alice", Some("   "), false)
            .expect_err("blank email must be rejected");
        assert!(error.to_string().contains("请提供 --email"));
    }

    #[test]
    fn shell_integration_state_prefers_loaded_then_installed_then_repository() {
        assert_eq!(
            classify_shell_integration(shell::IntegrationHealth::Healthy, true, true,),
            DoctorShellIntegrationState::WrapperLoaded
        );
        assert_eq!(
            classify_shell_integration(shell::IntegrationHealth::Incomplete, true, true,),
            DoctorShellIntegrationState::WrapperIncomplete
        );
        assert_eq!(
            classify_shell_integration(shell::IntegrationHealth::NotLoaded, true, true,),
            DoctorShellIntegrationState::InstalledNotLoaded
        );
        assert_eq!(
            classify_shell_integration(shell::IntegrationHealth::NotLoaded, false, true,),
            DoctorShellIntegrationState::RepositoryOnly
        );
        assert_eq!(
            classify_shell_integration(shell::IntegrationHealth::NotLoaded, false, false,),
            DoctorShellIntegrationState::NotIntegrated
        );
    }

    #[test]
    fn repository_integration_detects_each_persistent_ghis_artifact() {
        for (key, value) in [
            (
                "includeIf.gitdir:/tmp/other.path",
                "/tmp/config/ghis/fragments/work.gitconfig",
            ),
            (
                "credential.https://github.com.helper",
                "!'/usr/bin/ghis' --config '/tmp/config.toml' credential-helper",
            ),
            ("hook.ghis-pre-push.event", "pre-push"),
        ] {
            let directory = tempfile::tempdir().expect("temporary repository");
            let init = std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(directory.path())
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .expect("git init");
            assert!(init.success());
            let repository = ghis::repo::discover(directory.path()).expect("discover repository");
            assert!(!repository_has_ghis_persistence(&repository));

            let configured = std::process::Command::new("git")
                .args(["config", "--local", key, value])
                .current_dir(directory.path())
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .expect("write local config");
            assert!(configured.success(), "failed to write {key}");
            assert!(
                repository_has_ghis_persistence(&repository),
                "failed to detect {key}"
            );
        }
    }

    #[test]
    fn behavior_keys_accept_prefix_and_hyphen_aliases() {
        assert_eq!(
            normalize_behavior_key("behavior.auto-bind").unwrap(),
            "auto_bind"
        );
        assert_eq!(
            normalize_behavior_key("display-identity").unwrap(),
            "display_identity"
        );
        let error = normalize_behavior_key("unknown").unwrap_err().to_string();
        assert!(error.contains("可用值"));
        assert!(error.contains("default_profile"));
    }

    #[test]
    fn every_listed_behavior_setting_is_readable() {
        let config = Config::default();
        let settings = behavior_settings(&config);
        assert_eq!(settings.len(), KNOWN_BEHAVIOR_KEYS.len());
        assert_eq!(
            settings.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
            KNOWN_BEHAVIOR_KEYS
        );
    }

    #[test]
    fn tui_settings_keep_descriptions_inside_each_selectable_row() {
        let mut config = Config::default();
        config.behavior.default_profile = Some("personal".into());
        let rows = tui_setting_rows(&config);

        assert_eq!(rows.len(), KNOWN_BEHAVIOR_KEYS.len());
        assert!(rows.iter().all(|row| row.contains("\n  含义：")));
        assert!(rows[0].starts_with("auto_bind\t开启\n"));
        assert!(rows[1].starts_with("default_profile\tpersonal\n"));
        assert!(rows[4].contains("防止回退到其他账号"));
    }

    fn executable_test_program(path: &Path) -> signing::SigningProgram {
        fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
        signing::SigningProgram {
            path: path.into(),
            onepassword: true,
        }
    }

    fn wizard_key(
        path: impl Into<PathBuf>,
        material: &str,
        fingerprint: &str,
    ) -> signing::PublicKeyFile {
        signing::PublicKeyFile {
            path: path.into(),
            public_key: material.into(),
            fingerprint: Some(fingerprint.into()),
        }
    }

    fn agent_key(material: &str, fingerprint: &str) -> signing::AgentKey {
        let mut fields = material.split_whitespace();
        signing::AgentKey {
            key_type: fields.next().unwrap().into(),
            public_key: material.into(),
            comment: Some("同名签名 key".into()),
            fingerprint: Some(fingerprint.into()),
        }
    }

    fn wizard_agent(keys: Vec<signing::AgentKey>) -> signing::AgentInfo {
        signing::AgentInfo {
            socket: PathBuf::from("/tmp/ghis-test-agent.sock"),
            source: signing::AgentSource::OnePassword,
            available: true,
            keys,
            error: None,
        }
    }

    fn account(host: &str, login: &str) -> github::GhAccount {
        github::GhAccount {
            host: host.into(),
            login: login.into(),
            verified: true,
            ..github::GhAccount::default()
        }
    }

    #[test]
    fn cached_accounts_merge_with_profiles_and_keep_enterprise_selection() {
        let mut config = Config::default();
        config.profiles.insert(
            "personal profile".into(),
            Profile {
                host: "github.com".into(),
                login: "alice".into(),
                git_name: "Alice".into(),
                git_email: "alice@example.test".into(),
                ..Profile::default()
            },
        );
        let discovery = github::GhDiscovery {
            accounts: vec![
                account("github.com", "ALICE"),
                account("git.example.com", "worker"),
                account("git.example.com", "worker"),
            ],
            command_succeeded: true,
            offline: false,
        };

        let rows = tui_profile_rows(&config, Some(&discovery));
        assert_eq!(rows.len(), 2);
        assert!(rows[0].starts_with("personal profile\t"));
        assert!(rows[1].contains("git.example.com\tworker"));

        let mut state = tui::AppState::default();
        state.set_items(rows);
        state.query = "git.example.com".into();
        assert_eq!(
            selected_tui_profile(&state, &config).unwrap(),
            TuiProfileSelection::Discovered {
                host: "git.example.com".into(),
                login: "worker".into(),
            }
        );
    }

    #[test]
    fn discovered_profile_draft_preserves_account_host_and_user_edits() {
        let mut wizard = TuiProfileWizard::new("Git.Example.Com.", "worker", "", true);
        parse_tui_wizard_account(
            &mut wizard,
            "work identity|Git.Example.Com.|worker|Work Name|work@example.test",
        )
        .unwrap();
        assert_eq!(wizard.id, "work identity");
        assert_eq!(wizard.host, "git.example.com");
        assert_eq!(wizard.login, "worker");
        assert_eq!(wizard.git_name, "Work Name");
        assert_eq!(wizard.git_email, "work@example.test");
        assert!(wizard.bind_after_create);
    }

    #[test]
    fn signing_stage_uses_a_separate_key_when_ssh_is_external() {
        let directory = tempfile::tempdir().unwrap();
        let first = "ssh-ed25519 AAAAFIRST 同名签名 key";
        let second = "ssh-ed25519 AAAASECOND 同名签名 key";
        let mut wizard = TuiProfileWizard::new("github.com", "alice", "a@example.test", false);
        wizard.agent = Some(wizard_agent(vec![
            agent_key(first, "SHA256:first"),
            agent_key(second, "SHA256:second"),
        ]));
        wizard.agent_socket = wizard.agent.as_ref().map(|agent| agent.socket.clone());
        wizard.public_keys = vec![
            wizard_key(
                directory.path().join("authentication.pub"),
                first,
                "SHA256:first",
            ),
            wizard_key(
                directory.path().join("signing.pub"),
                second,
                "SHA256:second",
            ),
        ];
        wizard.signing_program = Some(executable_test_program(
            &directory.path().join("op-ssh-sign"),
        ));

        parse_tui_wizard_ssh(&mut wizard, "external").unwrap();
        parse_tui_wizard_signing(&mut wizard, "on|2|").unwrap();

        assert!(wizard.ssh.is_none(), "HTTPS profile must stay SSH-external");
        assert!(wizard.signing.enabled);
        assert_eq!(
            wizard.signing.signing_key.as_deref(),
            Some(
                directory
                    .path()
                    .join("signing.pub")
                    .to_string_lossy()
                    .as_ref()
            )
        );
    }

    #[test]
    fn external_ssh_profile_round_trips_through_tui_input() {
        let profile = Profile {
            login: "alice".into(),
            git_name: "Alice".into(),
            git_email: "a@example.test".into(),
            ssh: Some(SshProfile {
                mode: SshMode::External,
                ..SshProfile::default()
            }),
            ..Profile::default()
        };
        let mut wizard = TuiProfileWizard::from_profile("personal", &profile);
        let input = wizard.ssh_input();
        assert_eq!(input, "external");
        parse_tui_wizard_ssh(&mut wizard, &input).unwrap();
        assert_eq!(wizard.ssh, profile.ssh);
    }

    #[test]
    fn unchanged_ssh_and_signing_settings_can_be_kept_without_live_agent() {
        let profile = Profile {
            login: "alice".into(),
            git_name: "Alice".into(),
            git_email: "a@example.test".into(),
            ssh: Some(SshProfile {
                mode: SshMode::OnePassword,
                public_key: Some("/keys/authentication.pub".into()),
                fingerprint: Some("SHA256:authentication".into()),
                agent_socket: Some("/tmp/op-agent.sock".into()),
            }),
            signing: SigningProfile {
                enabled: true,
                signing_key: Some("/keys/signing.pub".into()),
                program: Some("/opt/1Password/op-ssh-sign".into()),
            },
            ..Profile::default()
        };
        let mut wizard = TuiProfileWizard::from_profile("personal", &profile);
        let ssh_input = wizard.ssh_input();
        let signing_input = wizard.signing_input();
        wizard.agent = Some(signing::AgentInfo {
            socket: "/tmp/op-agent.sock".into(),
            source: signing::AgentSource::OnePassword,
            available: false,
            keys: Vec::new(),
            error: Some("agent offline".into()),
        });

        parse_tui_wizard_ssh(&mut wizard, &ssh_input).unwrap();
        parse_tui_wizard_signing(&mut wizard, &signing_input).unwrap();
        assert_eq!(wizard.ssh, profile.ssh);
        assert_eq!(wizard.signing, profile.signing);
    }

    #[test]
    fn custom_agent_socket_is_marked_for_background_inspection() {
        let mut wizard = TuiProfileWizard::new("github.com", "alice", "a@example.test", false);
        let socket = PathBuf::from("/tmp/custom-agent.sock");
        assert_eq!(
            tui_agent_socket_needing_inspection(
                &wizard,
                "one-password|/keys/authentication.pub|/tmp/custom-agent.sock"
            ),
            Some(socket.clone())
        );
        wizard.agent = Some(signing::AgentInfo {
            socket,
            source: signing::AgentSource::OnePassword,
            available: true,
            keys: Vec::new(),
            error: None,
        });
        assert_eq!(
            tui_agent_socket_needing_inspection(
                &wizard,
                "one-password|/keys/authentication.pub|/tmp/custom-agent.sock"
            ),
            None
        );
        wizard.agent.as_mut().unwrap().available = false;
        assert_eq!(
            tui_agent_socket_needing_inspection(
                &wizard,
                "one-password|/keys/authentication.pub|/tmp/custom-agent.sock"
            ),
            Some(PathBuf::from("/tmp/custom-agent.sock"))
        );
    }

    #[test]
    fn tui_agent_details_escape_external_control_characters() {
        let mut wizard = TuiProfileWizard::new("github.com", "alice", "a@example.test", false);
        wizard.agent_socket = Some(PathBuf::from("/tmp/agent\n.sock"));
        wizard.agent = Some(signing::AgentInfo {
            socket: PathBuf::from("/tmp/agent\n.sock"),
            source: signing::AgentSource::OnePassword,
            available: false,
            keys: vec![signing::AgentKey {
                key_type: "ssh-ed25519\u{1b}[31m".into(),
                public_key: "ssh-ed25519 AAAA".into(),
                comment: Some("comment\nwith-break".into()),
                fingerprint: Some("SHA256:fingerprint".into()),
            }],
            error: Some("agent\u{1b}[2J failed".into()),
        });
        let rendered = tui_agent_discovery_details(&wizard).join("\n");
        assert!(!rendered.contains('\u{1b}'));
        assert!(rendered.contains("\\u{001B}"));
        assert!(rendered.contains("\\n"));
    }

    #[test]
    fn signing_stage_handles_zero_one_and_mismatched_agent_keys() {
        let directory = tempfile::tempdir().unwrap();
        let program = executable_test_program(&directory.path().join("op-ssh-sign"));
        let selected = wizard_key(
            directory.path().join("signing.pub"),
            "ssh-ed25519 AAAASELECTED signing",
            "SHA256:selected",
        );
        let mut wizard = TuiProfileWizard::new("github.com", "alice", "a@example.test", false);
        wizard.public_keys = vec![selected.clone()];
        wizard.signing_program = Some(program.clone());
        wizard.agent = Some(wizard_agent(Vec::new()));
        assert!(parse_tui_wizard_signing(&mut wizard, "on|1|").is_err());

        wizard.agent = Some(wizard_agent(vec![agent_key(
            "ssh-ed25519 AAAADIFFERENT signing",
            "SHA256:different",
        )]));
        assert!(parse_tui_wizard_signing(&mut wizard, "on|1|").is_err());

        wizard.agent = Some(wizard_agent(vec![agent_key(
            &selected.public_key,
            selected.fingerprint.as_deref().unwrap(),
        )]));
        parse_tui_wizard_signing(&mut wizard, "on|1|").unwrap();
        assert!(wizard.signing.enabled);
    }

    #[test]
    fn signing_stage_rejects_an_unavailable_program() {
        let directory = tempfile::tempdir().unwrap();
        let key = wizard_key(
            directory.path().join("signing.pub"),
            "ssh-ed25519 AAAASELECTED signing",
            "SHA256:selected",
        );
        let mut wizard = TuiProfileWizard::new("github.com", "alice", "a@example.test", false);
        wizard.agent = Some(wizard_agent(vec![agent_key(
            &key.public_key,
            key.fingerprint.as_deref().unwrap(),
        )]));
        wizard.public_keys = vec![key];
        let missing = directory.path().join("missing-op-ssh-sign");

        let error = parse_tui_wizard_signing(&mut wizard, &format!("on|1|{}", missing.display()))
            .unwrap_err();
        assert!(error.to_string().contains("不可执行"));
        assert!(!wizard.signing.enabled);
    }

    #[test]
    fn managed_ssh_key_is_only_a_default_for_existing_signing_config() {
        let profile = Profile {
            login: "alice".into(),
            git_name: "Alice".into(),
            git_email: "a@example.test".into(),
            ssh: Some(SshProfile {
                mode: SshMode::OnePassword,
                public_key: Some(PathBuf::from("/keys/authentication.pub")),
                fingerprint: Some("SHA256:authentication".into()),
                agent_socket: Some(PathBuf::from("/tmp/agent.sock")),
            }),
            signing: SigningProfile {
                enabled: true,
                signing_key: None,
                program: Some(PathBuf::from("/opt/1Password/op-ssh-sign")),
            },
            ..Profile::default()
        };
        let wizard = TuiProfileWizard::from_profile("personal", &profile);

        assert_eq!(
            wizard.signing_input(),
            "on|/keys/authentication.pub|/opt/1Password/op-ssh-sign"
        );
    }

    #[test]
    fn doctor_recognizes_supported_inline_signing_key_prefixes() {
        for value in [
            "ssh-ed25519 AAAA inline",
            "ecdsa-sha2-nistp256 AAAA inline",
            "sk-ssh-ed25519@openssh.com AAAA inline",
            "rsa-sha2-512 AAAA inline",
        ] {
            let profile = Profile {
                signing: SigningProfile {
                    enabled: true,
                    signing_key: Some(value.into()),
                    ..SigningProfile::default()
                },
                ..Profile::default()
            };
            assert_eq!(
                profile_public_key_for_doctor(&profile).unwrap(),
                Some(value.into())
            );
        }
    }

    #[test]
    fn cancelling_profile_wizard_does_not_write_config() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        Config::default().save(&config_path).unwrap();
        let workers = TuiWorkers::new().unwrap();
        let action_context = TuiActionContext {
            workers: &workers,
            path: Some(&config_path),
            explicit: None,
            cwd: directory.path(),
            cache_path: &directory.path().join("accounts.json"),
        };
        let mut state = tui::AppState::default();
        state.mode = tui::Mode::Insert;
        let mut pending = Some(TuiPending::ProfileWizard(Box::new(TuiProfileWizard::new(
            "github.com",
            "alice",
            "a@example.test",
            false,
        ))));
        let mut discovery = None;
        let mut active_workers = 0;

        handle_tui_action(
            &mut state,
            tui::Action::Cancel,
            &mut pending,
            &mut discovery,
            &mut active_workers,
            &action_context,
        )
        .unwrap();

        assert!(pending.is_none());
        assert!(Config::load(&config_path).unwrap().profiles.is_empty());
    }

    #[test]
    fn saving_edited_wizard_preserves_profiles_added_after_it_opened() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let mut paths = ConfigPaths::from_bases(
            directory.path().join("xdg-config"),
            directory.path().join("xdg-cache"),
            directory.path().join("xdg-state"),
        );
        paths.set_config_file(&config_path).unwrap();
        let original = Profile {
            login: "alice".into(),
            git_name: "Alice".into(),
            git_email: "a@example.test".into(),
            ..Profile::default()
        };
        let mut config = Config::default();
        config.profiles.insert("personal".into(), original.clone());
        config.save(&config_path).unwrap();
        let mut wizard = TuiProfileWizard::from_profile("personal", &original);
        wizard.git_name = "Alice Updated".into();

        let mut changed_while_open = Config::load(&config_path).unwrap();
        changed_while_open.profiles.insert(
            "work".into(),
            Profile {
                login: "worker".into(),
                git_name: "Worker".into(),
                git_email: "worker@example.test".into(),
                ..Profile::default()
            },
        );
        changed_while_open.save(&config_path).unwrap();

        save_tui_profile_wizard_at_paths(&paths, &wizard).unwrap();
        let saved = Config::load(&config_path).unwrap();
        assert_eq!(saved.profiles.len(), 2);
        assert_eq!(saved.profiles["personal"].git_name, "Alice Updated");
        assert_eq!(saved.profiles["work"].login, "worker");
    }

    #[test]
    fn saving_edited_wizard_rejects_same_profile_changes_after_open() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let mut paths = ConfigPaths::from_bases(
            directory.path().join("xdg-config"),
            directory.path().join("xdg-cache"),
            directory.path().join("xdg-state"),
        );
        paths.set_config_file(&config_path).unwrap();
        let original = Profile {
            login: "alice".into(),
            git_name: "Alice".into(),
            git_email: "a@example.test".into(),
            ..Profile::default()
        };
        let mut config = Config::default();
        config.profiles.insert("personal".into(), original.clone());
        config.save(&config_path).unwrap();
        let mut wizard = TuiProfileWizard::from_profile("personal", &original);
        wizard.git_name = "Alice from wizard".into();

        let mut changed = Config::load(&config_path).unwrap();
        changed.profiles.get_mut("personal").unwrap().git_email =
            "alice-changed@example.test".into();
        changed.save(&config_path).unwrap();

        let error = save_tui_profile_wizard_at_paths(&paths, &wizard).unwrap_err();
        assert!(error.to_string().contains("在编辑期间已被其他操作修改"));
        let saved = Config::load(&config_path).unwrap();
        assert_eq!(
            saved.profiles["personal"].git_email,
            "alice-changed@example.test"
        );
        assert_eq!(saved.profiles["personal"].git_name, "Alice");
    }

    #[test]
    fn suggested_profile_id_avoids_existing_ids() {
        let mut config = Config::default();
        for id in ["worker", "worker@git.example.com"] {
            config.profiles.insert(id.into(), Profile::default());
        }
        assert_eq!(
            suggested_tui_profile_id(&config, "git.example.com", "worker"),
            "worker@git.example.com-2"
        );
    }

    #[test]
    fn worker_completion_updates_cache_and_visible_profile_rows() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        Config::default().save(&config_path).unwrap();
        let cache_path = directory.path().join("accounts.json");
        let workers = TuiWorkers::new().expect("TUI worker");
        workers
            .sender
            .send(TuiWorkerResult::Discovery(Ok(github::GhDiscovery {
                accounts: vec![account("git.example.com", "worker")],
                command_succeeded: true,
                offline: false,
            })))
            .unwrap();
        let action_context = TuiActionContext {
            workers: &workers,
            path: Some(&config_path),
            explicit: None,
            cwd: directory.path(),
            cache_path: &cache_path,
        };
        let mut state = tui::AppState::default();
        state.view = tui::View::Profiles;
        state.loading = true;
        let mut pending = None;
        let mut discovery = None;
        let mut active_workers = 1;

        receive_tui_worker_results(
            &mut state,
            &mut pending,
            &mut discovery,
            &mut active_workers,
            &action_context,
        )
        .unwrap();

        assert!(!state.loading);
        assert_eq!(active_workers, 0);
        assert!(
            state
                .items
                .iter()
                .any(|row| row.contains("git.example.com"))
        );
        assert_eq!(
            github::load_discovery_cache(&cache_path)
                .unwrap()
                .unwrap()
                .accounts[0]
                .login,
            "worker"
        );
    }

    #[test]
    fn initial_tui_state_uses_config_and_cache_before_local_refresh() {
        let mut config = Config::default();
        config.profiles.insert(
            "personal".into(),
            Profile {
                login: "alice".into(),
                git_name: "Alice".into(),
                git_email: "alice@example.test".into(),
                ..Profile::default()
            },
        );
        let discovery = github::GhDiscovery {
            accounts: vec![account("git.example.com", "worker")],
            command_succeeded: true,
            offline: false,
        };
        let mut state = tui::AppState::default();

        initialize_tui_state(&mut state, &config, Some(&discovery));

        assert!(
            state
                .view_items(tui::View::Status)
                .iter()
                .any(|item| item.contains("正在后台检查"))
        );
        assert_eq!(state.view_items(tui::View::Profiles).len(), 2);
        assert!(
            state
                .view_items(tui::View::Diagnostics)
                .iter()
                .any(|item| item.contains("git.example.com/worker"))
        );
    }

    #[test]
    fn local_worker_completion_applies_snapshot_and_finishes_loading() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        Config::default().save(&config_path).unwrap();
        let cache_path = directory.path().join("accounts.json");
        let workers = TuiWorkers::new().expect("TUI worker");
        workers
            .sender
            .send(TuiWorkerResult::LocalRefresh(Ok(TuiLocalSnapshot {
                repository: "/src/project".into(),
                profile: "work".into(),
                git_identity: "Work <work@example.test>".into(),
                github_identity: "git.example.com/worker".into(),
                transport: "https".into(),
                signing: "未启用".into(),
                warnings: Vec::new(),
                status_items: vec!["仓库\t/src/project".into()],
                config: Config::default(),
                diagnostics: vec!["Git\tgit version test".into()],
            })))
            .unwrap();
        let action_context = TuiActionContext {
            workers: &workers,
            path: Some(&config_path),
            explicit: None,
            cwd: directory.path(),
            cache_path: &cache_path,
        };
        let mut state = tui::AppState::default();
        state.loading = true;
        let mut pending = None;
        let mut discovery = None;
        let mut active_workers = 1;

        receive_tui_worker_results(
            &mut state,
            &mut pending,
            &mut discovery,
            &mut active_workers,
            &action_context,
        )
        .unwrap();

        assert_eq!(active_workers, 0);
        assert!(!state.loading);
        assert_eq!(state.repository, "/src/project");
        assert_eq!(state.github_identity, "git.example.com/worker");
        assert_eq!(
            state.view_items(tui::View::Diagnostics)[0],
            "Git\tgit version test"
        );
    }

    #[test]
    fn tui_rule_edit_preserves_match_fields_not_shown_in_editor() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let mut config = Config::default();
        for (id, login) in [("personal", "alice"), ("work", "worker")] {
            config.profiles.insert(
                id.into(),
                Profile {
                    login: login.into(),
                    git_name: login.into(),
                    git_email: format!("{login}@example.test"),
                    ..Profile::default()
                },
            );
        }
        config.rules.push(Rule {
            id: "all-fields".into(),
            profile: "personal".into(),
            priority: 10,
            host: Some("github.com".into()),
            owner: Some("before".into()),
            repo: Some("project".into()),
            remote: Some("https://github.com/**".into()),
            gitdir: Some("/src/**".into()),
        });
        config.save(&config_path).unwrap();

        let mut paths = ConfigPaths::from_bases(
            directory.path().join("xdg-config"),
            directory.path().join("xdg-cache"),
            directory.path().join("xdg-state"),
        );
        paths.set_config_file(&config_path).unwrap();
        apply_tui_input_at_paths(
            TuiPending::EditRule("all-fields".into()),
            "renamed|work|25|git.example.com|after|new-project|ssh://git.example.com/**|/work/**",
            &paths,
        )
        .unwrap();

        let saved = Config::load(&config_path).unwrap();
        let rule = &saved.rules[0];
        assert_eq!(rule.id, "renamed");
        assert_eq!(rule.profile, "work");
        assert_eq!(rule.priority, 25);
        assert_eq!(rule.owner.as_deref(), Some("after"));
        assert_eq!(rule.host.as_deref(), Some("git.example.com"));
        assert_eq!(rule.repo.as_deref(), Some("new-project"));
        assert_eq!(rule.remote.as_deref(), Some("ssh://git.example.com/**"));
        assert_eq!(rule.gitdir.as_deref(), Some("/work/**"));
        assert!(paths.fragments_dir.join("personal.gitconfig").is_file());
        assert!(paths.fragments_dir.join("work.gitconfig").is_file());
    }

    #[test]
    fn tui_rule_parser_maps_empty_match_fields_to_none() {
        let rule = parse_tui_rule("minimal|personal|0|||||").unwrap();
        assert_eq!(rule.id, "minimal");
        assert_eq!(rule.profile, "personal");
        assert_eq!(rule.priority, 0);
        assert_eq!(rule.host, None);
        assert_eq!(rule.owner, None);
        assert_eq!(rule.repo, None);
        assert_eq!(rule.remote, None);
        assert_eq!(rule.gitdir, None);
    }
}
