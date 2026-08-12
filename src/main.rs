use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use ghis::app::{self, AppContext};
use ghis::config::{
    Config, ConfigPaths, CredentialFailurePolicy, DisplayIdentity, Profile, Rule, SigningProfile,
    SshMode, SshProfile, SshUnmanagedPolicy, UnresolvedPolicy,
};
use ghis::{credential, github, shell, signing, tui};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

#[derive(Debug, Parser)]
#[command(name = "ghis", version, about = "按仓库切换 Git 和 GitHub 身份")]
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
    Doctor(JsonArgs),
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
        Commands::Doctor(args) => doctor(cli.config.as_deref(), cli.profile.as_deref(), args.json),
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
            } else {
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
                    println!(
                        "{}{}",
                        candidate.email,
                        (!labels.is_empty())
                            .then(|| format!("（{}）", labels.join("，")))
                            .unwrap_or_default()
                    );
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
    accounts: Vec<DoctorAccount>,
    repository: Option<String>,
    profile: Option<String>,
    credential_available: Option<bool>,
    ssh_agent: Option<DoctorAgent>,
    signing_program: Option<DoctorSigningProgram>,
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct ToolStatus {
    available: bool,
    version: Option<String>,
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

fn doctor(path: Option<&Path>, explicit: Option<&str>, json: bool) -> app::Result<i32> {
    let ctx = context(path, explicit, std::env::current_dir()?)?;
    let mut warnings = ctx.warnings.clone();
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
    let selector = doctor_key_selector(ctx.profile.as_ref(), &mut warnings);
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
    let signing_required = ctx
        .profile
        .as_ref()
        .is_some_and(|profile| profile.signing.enabled);
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

    let report = DoctorReport {
        schema_version: ghis::SCHEMA_VERSION,
        git: tool_status("git", &["--version"]),
        gh: tool_status("gh", &["--version"]),
        zsh: tool_status("zsh", &["--version"]),
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
        warnings,
    };
    if json {
        print_json(&report)?;
    } else {
        println!("Git: {}", tool_text(&report.git));
        println!("gh: {}", tool_text(&report.gh));
        println!("zsh: {}", tool_text(&report.zsh));
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
        for warning in &report.warnings {
            eprintln!("警告：{warning}");
        }
    }
    Ok(0)
}

fn doctor_key_selector(
    profile: Option<&Profile>,
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
        match profile_public_key_for_doctor(profile) {
            Ok(key) => key,
            Err(error) => {
                warnings.push(error);
                None
            }
        }
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
        return Ok(Some(text.into_owned()));
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
    RepositoryMutation {
        operation: TuiRepositoryMutation,
        result: app::Result<TuiLocalSnapshot>,
    },
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
    AddProfile,
    EditProfile(String),
    AddRule,
    EditRule(String),
    DeleteProfile(String),
    DeleteRule(String),
    LoadingDiscoveredProfile {
        host: String,
        login: String,
        bind_after_create: bool,
    },
    InputDiscoveredProfile {
        host: String,
        login: String,
        bind_after_create: bool,
    },
    ConfirmDiscoveredProfile(TuiProfileDraft),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TuiProfileDraft {
    id: String,
    host: String,
    login: String,
    git_name: String,
    git_email: String,
    bind_after_create: bool,
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
    let diagnostics = vec![
        format!("Git\t{}", tool_text(&tool_status("git", &["--version"]))),
        format!("gh\t{}", tool_text(&tool_status("gh", &["--version"]))),
        format!("zsh\t{}", tool_text(&tool_status("zsh", &["--version"]))),
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
    state.set_view_items(
        tui::View::Settings,
        [
            format!(
                "auto_bind\t{}",
                if config.behavior.auto_bind {
                    "开启"
                } else {
                    "关闭"
                }
            ),
            format!(
                "default_profile\t{}",
                config.behavior.default_profile.as_deref().unwrap_or("无")
            ),
            format!(
                "display_identity\t{}",
                display_identity_value(config.behavior.display_identity)
            ),
            format!(
                "unresolved\t{}",
                match config.behavior.unresolved {
                    UnresolvedPolicy::WarnAndContinue => "warn-and-continue",
                    UnresolvedPolicy::Fail => "fail",
                }
            ),
            "credential_failure\tfail（v1 固定）".into(),
            format!(
                "ssh_unmanaged\t{}",
                match config.behavior.ssh_unmanaged {
                    SshUnmanagedPolicy::WarnAndContinue => "warn-and-continue",
                    SshUnmanagedPolicy::Fail => "fail",
                }
            ),
        ],
    );
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
            *pending = Some(TuiPending::AddProfile);
            state.details.clear();
            begin_tui_input(state, "ID|GitHub 登录名|提交姓名|提交邮箱", "");
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
            let initial = format!(
                "{id}|{}|{}|{}",
                profile.login, profile.git_name, profile.git_email
            );
            *pending = Some(TuiPending::EditProfile(id));
            begin_tui_input(state, "ID|GitHub 登录名|提交姓名|提交邮箱", &initial);
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
                Some(TuiPending::InputDiscoveredProfile {
                    host,
                    login,
                    bind_after_create,
                }) => {
                    let draft = match parse_tui_profile_draft(
                        &state.input,
                        &host,
                        &login,
                        bind_after_create,
                    ) {
                        Ok(draft) => draft,
                        Err(error) => {
                            *pending = Some(TuiPending::InputDiscoveredProfile {
                                host,
                                login,
                                bind_after_create,
                            });
                            state.mode = tui::Mode::Insert;
                            return Err(error);
                        }
                    };
                    state.set_input(format!(
                        "创建 `{}`：{} <{}>，账号 {}/{}{}",
                        draft.id,
                        draft.git_name,
                        draft.git_email,
                        draft.host,
                        draft.login,
                        if draft.bind_after_create {
                            "，并绑定当前仓库"
                        } else {
                            ""
                        }
                    ));
                    state.mode = tui::Mode::Confirm;
                    state.status = "请检查身份信息，按 y 或 Enter 确认创建".into();
                    *pending = Some(TuiPending::ConfirmDiscoveredProfile(draft));
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
            Some(TuiPending::ConfirmDiscoveredProfile(draft)) => {
                save_tui_profile_draft(path, &draft)?;
                if draft.bind_after_create {
                    let ctx = context(path, Some(&draft.id), cwd)?;
                    app::bind_repository(&ctx, &draft.id)?;
                }
                reload_tui_config_views(state, path, discovery.as_ref())?;
                schedule_tui_local_refresh(state, active_workers, action_context)?;
                state.details.clear();
                state.input.clear();
                state.status = if draft.bind_after_create {
                    format!("已创建并绑定身份配置 `{}`", draft.id)
                } else {
                    format!("已创建身份配置 `{}`", draft.id)
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
                state.details = tui_email_candidate_details(&host, &login, &candidates);
                begin_tui_input(
                    state,
                    "编辑 id|提交姓名|提交邮箱，回车后再次确认",
                    &format!("{id}|{login}|{email}"),
                );
                state.status = if candidates.is_empty() {
                    "没有可预填的邮箱；请手动填写提交邮箱，回车后确认"
                } else if candidates
                    .first()
                    .is_some_and(|candidate| candidate.noreply)
                {
                    "已预填 GitHub noreply 邮箱；可继续编辑，回车后确认"
                } else {
                    "已预填首选邮箱；可继续编辑，回车后确认"
                }
                .into();
                if let Some(warning) = warning {
                    state.warnings.push(warning);
                }
                *pending = Some(TuiPending::InputDiscoveredProfile {
                    host,
                    login,
                    bind_after_create,
                });
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
        format!("待创建账号：{host}/{login}"),
        "提交邮箱候选：".into(),
    ];
    if candidates.is_empty() {
        details.push("  未找到，请手动填写".into());
    } else {
        details.extend(candidates.iter().map(|candidate| {
            let mut labels = Vec::new();
            if candidate.primary {
                labels.push("首选");
            }
            if candidate.noreply {
                labels.push("GitHub noreply");
            }
            if candidate.verified {
                labels.push("已验证");
            }
            if labels.is_empty() {
                format!("  {}", candidate.email)
            } else {
                format!("  {}（{}）", candidate.email, labels.join("，"))
            }
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

fn parse_tui_profile_draft(
    input: &str,
    host: &str,
    login: &str,
    bind_after_create: bool,
) -> app::Result<TuiProfileDraft> {
    let fields = input.split('|').map(str::trim).collect::<Vec<_>>();
    if fields.len() != 3 || fields.iter().any(|field| field.is_empty()) {
        return Err(app::AppError::Message(
            "Profile 输入需要 id|提交姓名|提交邮箱，三项都不能为空".into(),
        ));
    }
    Ok(TuiProfileDraft {
        id: fields[0].into(),
        host: github::normalize_host(host),
        login: login.into(),
        git_name: fields[1].into(),
        git_email: fields[2].into(),
        bind_after_create,
    })
}

fn save_tui_profile_draft(path: Option<&Path>, draft: &TuiProfileDraft) -> app::Result<()> {
    let (paths, _) = load_config(path)?;
    let (config, ()) = update_config(&paths, |config| {
        if config.profiles.contains_key(&draft.id) {
            return Err(app::AppError::Message(format!(
                "Profile `{}` 已存在，请返回后换一个 ID",
                draft.id
            )));
        }
        config.profiles.insert(
            draft.id.clone(),
            Profile {
                host: draft.host.clone(),
                login: draft.login.clone(),
                git_name: draft.git_name.clone(),
                git_email: draft.git_email.clone(),
                ..Profile::default()
            },
        );
        Ok(())
    })?;
    app::sync_fragments(&paths, &config)?;
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
        TuiPending::DeleteRule(_)
            | TuiPending::DeleteProfile(_)
            | TuiPending::LoadingDiscoveredProfile { .. }
            | TuiPending::InputDiscoveredProfile { .. }
            | TuiPending::ConfirmDiscoveredProfile(_)
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
            TuiPending::AddProfile => {
                let fields = input.split('|').map(str::trim).collect::<Vec<_>>();
                if fields.len() != 4 || fields.iter().any(|field| field.is_empty()) {
                    return Err(app::AppError::Message(
                        "Profile 输入需要 id|login|name|email".into(),
                    ));
                }
                if config.profiles.contains_key(fields[0]) {
                    return Err(app::AppError::Message(format!(
                        "Profile `{}` 已存在",
                        fields[0]
                    )));
                }
                config.profiles.insert(
                    fields[0].into(),
                    Profile {
                        host: "github.com".into(),
                        login: fields[1].into(),
                        git_name: fields[2].into(),
                        git_email: fields[3].into(),
                        ..Profile::default()
                    },
                );
            }
            TuiPending::EditProfile(previous) => {
                let fields = input.split('|').map(str::trim).collect::<Vec<_>>();
                if fields.len() != 4 || fields.iter().any(|field| field.is_empty()) {
                    return Err(app::AppError::Message(
                        "Profile 输入需要 id|login|name|email".into(),
                    ));
                }
                if previous != fields[0] {
                    return Err(app::AppError::Message(
                        "为避免仓库绑定失效，TUI 编辑时不能修改 Profile ID".into(),
                    ));
                }
                let profile = config.profiles.get_mut(&previous).ok_or_else(|| {
                    app::AppError::Message(format!("Profile `{previous}` 不存在"))
                })?;
                profile.login = fields[1].into();
                profile.git_name = fields[2].into();
                profile.git_email = fields[3].into();
            }
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
        let draft = parse_tui_profile_draft(
            "work identity|Work Name|work@example.test",
            "Git.Example.Com.",
            "worker",
            true,
        )
        .unwrap();
        assert_eq!(draft.id, "work identity");
        assert_eq!(draft.host, "git.example.com");
        assert_eq!(draft.login, "worker");
        assert_eq!(draft.git_name, "Work Name");
        assert_eq!(draft.git_email, "work@example.test");
        assert!(draft.bind_after_create);
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
