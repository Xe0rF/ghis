use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use ghis::agent_context::{AgentContext, AgentContextFormat};
use ghis::app::{self, AppContext};
use ghis::config::{
    Config, ConfigPaths, CredentialFailurePolicy, DisplayIdentity, Profile, ResolutionSource, Rule,
    SigningProfile, SshMode, SshProfile, SshUnmanagedPolicy, UnresolvedPolicy,
};
use ghis::{credential, diagnostics, github, shell, signing};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};

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
    /// 显示当前仓库和有效身份
    Status(StatusArgs),
    /// 输出供 coding agent 使用的最小、脱敏仓库身份上下文
    Context(ContextArgs),
    /// 从 gh CLI 发现已有账号
    Discover(JsonArgs),
    /// 通过普通行式提示创建 Profile 并可选初始化当前仓库
    Onboard(OnboardArgs),
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
    /// 在执行 git/gh 前即时返回结构化诊断（不注入 agent context）
    Check(CheckArgs),
    /// 启动或配置 Claude Code/Codex 的 ghis 上下文集成
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
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

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
enum ContextFormatArg {
    #[default]
    Json,
    Claude,
    Codex,
}

#[derive(Debug, Args, Default)]
struct ContextArgs {
    #[arg(long, value_enum, default_value_t = ContextFormatArg::Json)]
    format: ContextFormatArg,
    /// 在指定目录解析仓库上下文
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// 未解析、歧义或失效选择时返回错误
    #[arg(long)]
    require_resolved: bool,
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
struct CheckArgs {
    #[arg(long, value_enum)]
    operation: CheckOperation,
    #[arg(long)]
    json: bool,
    #[arg(last = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
enum CheckOperation {
    Git,
    Gh,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum AgentTarget {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum AgentScope {
    User,
    Project,
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
    /// 主动注入当前 ghis 上下文并启动 coding agent
    #[command(trailing_var_arg = true)]
    Run {
        #[arg(value_enum)]
        target: AgentTarget,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// 安装 Claude Hook 或检查 Codex launcher 能力
    Setup {
        #[arg(value_enum)]
        target: AgentTarget,
        #[arg(long, value_enum, default_value_t = AgentScope::User)]
        scope: AgentScope,
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long)]
        yes: bool,
    },
    /// 移除 ghis 管理的 Claude Hook
    Uninstall {
        #[arg(value_enum)]
        target: AgentTarget,
        #[arg(long, value_enum, default_value_t = AgentScope::User)]
        scope: AgentScope,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// 检查 agent CLI 与上下文集成状态
    Status {
        #[arg(value_enum)]
        target: AgentTarget,
        #[arg(long, value_enum, default_value_t = AgentScope::User)]
        scope: AgentScope,
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Claude Code Hook 内部入口
    #[command(hide = true)]
    Hook {
        #[arg(value_enum)]
        target: AgentTarget,
    },
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

#[derive(Debug, Args, Default)]
struct OnboardArgs {
    /// 评估并可选绑定的仓库路径，默认使用当前目录
    #[arg(long)]
    repo: Option<PathBuf>,
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
    /// 用于状态和列表展示的人类可读说明
    #[arg(long)]
    description: Option<String>,
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
    /// 设置用于状态和列表展示的人类可读说明
    #[arg(long, conflicts_with = "clear_description")]
    description: Option<String>,
    /// 清除 Profile 的人类可读说明
    #[arg(long, conflicts_with = "description")]
    clear_description: bool,
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
    let command = cli
        .command
        .unwrap_or_else(|| Commands::Status(StatusArgs::default()));
    match command {
        Commands::Status(args) => status(cli.config.as_deref(), cli.profile.as_deref(), args),
        Commands::Context(args) => {
            agent_context(cli.config.as_deref(), cli.profile.as_deref(), args)
        }
        Commands::Discover(args) => discover(cli.config.as_deref(), args.json),
        Commands::Onboard(args) => onboard(cli.config.as_deref(), args),
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
        Commands::Check(args) => check(cli.config.as_deref(), cli.profile.as_deref(), args),
        Commands::Agent { command } => {
            agent_command(cli.config.as_deref(), cli.profile.as_deref(), command)
        }
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

fn agent_command(
    path: Option<&Path>,
    explicit: Option<&str>,
    command: AgentCommand,
) -> app::Result<i32> {
    match command {
        AgentCommand::Run { target, cwd, args } => {
            let cwd = cwd.unwrap_or(std::env::current_dir()?);
            let context = AgentContext::from_app(&context(path, explicit, &cwd)?);
            let paths = ConfigPaths::discover()?;
            let shim_dir = paths.cache_dir.join("agent-shims");
            let binary = PathBuf::from(agent_binary());
            ghis::agent::prepare_session_shims(&shim_dir, &binary)?;
            let spec = match target {
                AgentTarget::Claude => {
                    ghis::agent::claude::run_spec(&context, "claude", args, &cwd)
                }
                AgentTarget::Codex => ghis::agent::codex::run_spec(&context, "codex", args, &cwd),
            }
            .map_err(|error| app::AppError::Message(error.to_string()))?;
            let spec = ghis::agent::prepend_path(spec, &shim_dir)?;
            ghis::agent::launch(&ghis::process::SystemCommandRunner::new(), &spec)
                .map_err(|error| app::AppError::Message(error.to_string()))
        }
        AgentCommand::Hook {
            target: AgentTarget::Claude,
        } => {
            let cwd = std::env::current_dir()?;
            let context = AgentContext::from_app(&context(path, explicit, &cwd)?);
            let output = ghis::agent::claude::handle_hook(&context, io::stdin())
                .map_err(|error| app::AppError::Message(error.to_string()))?;
            ghis::agent::claude::write_hook_output(&output, io::stdout())
                .map_err(|error| app::AppError::Message(error.to_string()))?;
            Ok(0)
        }
        AgentCommand::Hook {
            target: AgentTarget::Codex,
        } => Err(app::AppError::Message(
            "Codex 没有 Claude Hook 兼容入口；请使用 `ghis agent run codex`".into(),
        )),
        AgentCommand::Setup {
            target: AgentTarget::Claude,
            scope,
            project,
            yes,
        } => {
            let path = claude_settings_path(scope, project)?;
            if !yes && !io::stdin().is_terminal() {
                return Err(app::AppError::Message(
                    "非交互环境请明确使用 `ghis agent setup claude --yes`".into(),
                ));
            }
            let changed =
                ghis::agent::claude::setup_settings(&path, Path::new(&agent_binary()), None)
                    .map_err(|error| app::AppError::Message(error.to_string()))?;
            println!(
                "Claude Code Hook {}：{}",
                if changed { "已安装" } else { "无需更新" },
                path.display()
            );
            Ok(0)
        }
        AgentCommand::Setup {
            target: AgentTarget::Codex,
            ..
        } => {
            if !tool_status("codex", &["--version"]).available {
                return Err(app::AppError::Message("未找到 Codex CLI".into()));
            }
            println!(
                "Codex 使用 `ghis agent run codex` 在启动时注入 developer instructions；未修改 MCP、skill 或项目指令文件。\n"
            );
            Ok(0)
        }
        AgentCommand::Uninstall {
            target: AgentTarget::Claude,
            scope,
            project,
        } => {
            let path = claude_settings_path(scope, project)?;
            let changed = ghis::agent::claude::uninstall_settings(&path)
                .map_err(|error| app::AppError::Message(error.to_string()))?;
            println!(
                "Claude Code Hook {}：{}",
                if changed { "已移除" } else { "未发现" },
                path.display()
            );
            Ok(0)
        }
        AgentCommand::Uninstall {
            target: AgentTarget::Codex,
            ..
        } => {
            println!("Codex 没有 ghis 管理的持久文件；无需卸载。\n");
            Ok(0)
        }
        AgentCommand::Status {
            target,
            scope,
            project,
            json,
        } => {
            let available = match target {
                AgentTarget::Claude => tool_status("claude", &["--version"]),
                AgentTarget::Codex => tool_status("codex", &["--version"]),
            };
            let settings = if matches!(target, AgentTarget::Claude) {
                Some(ghis::agent::claude::settings_status(&claude_settings_path(
                    scope, project,
                )?))
            } else {
                None
            };
            if json {
                let value = serde_json::json!({
                    "target": match target { AgentTarget::Claude => "claude", AgentTarget::Codex => "codex" },
                    "available": available.available,
                    "version": available.version,
                    "settings": settings.map(|result| result.map(|item| serde_json::json!({
                        "installed": item.installed(),
                        "session_start": item.session_start,
                        "user_prompt_submit": item.user_prompt_submit,
                        "subagent_start": item.subagent_start,
                    }))).transpose().map_err(|error| app::AppError::Message(error.to_string()))?,
                });
                print_json(&value)?;
            } else {
                println!(
                    "{}：{}",
                    match target {
                        AgentTarget::Claude => "Claude Code",
                        AgentTarget::Codex => "Codex",
                    },
                    tool_text(&available)
                );
                if let Some(settings) = settings {
                    println!(
                        "Hook：{}",
                        if settings
                            .map_err(|error| app::AppError::Message(error.to_string()))?
                            .installed()
                        {
                            "已安装"
                        } else {
                            "未安装"
                        }
                    );
                }
            }
            Ok(0)
        }
    }
}

fn claude_settings_path(scope: AgentScope, project: Option<PathBuf>) -> app::Result<PathBuf> {
    match scope {
        AgentScope::Project => Ok(project
            .unwrap_or(std::env::current_dir()?)
            .join(".claude/settings.local.json")),
        AgentScope::User => {
            let home = std::env::var_os("HOME")
                .ok_or_else(|| app::AppError::Message("HOME 未设置".into()))?;
            Ok(PathBuf::from(home).join(".claude/settings.json"))
        }
    }
}
fn agent_binary() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
        .unwrap_or_else(|| "ghis".into())
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
    if args.shell {
        return shell_status(path);
    }
    let ctx = context(path, explicit, std::env::current_dir()?)?;
    if args.quiet {
        return Ok(0);
    }
    let report = app::display_status(&ctx);
    if args.json {
        print_json(&report)?;
    } else {
        println!("{}", ctx.status_summary());
        for warning in &ctx.warnings {
            eprintln!("警告：{warning}");
        }
    }
    Ok(0)
}

fn shell_status(path: Option<&Path>) -> app::Result<i32> {
    let (paths, config) = load_config(path)?;
    if !config.behavior.display_profile_on_chpwd {
        println!("GHIS_CHPWD_ENABLED=0");
        println!("GHIS_REPO_PROFILE=''\nGHIS_REPO_PROFILE_DISPLAY=''\nGHIS_REPO_ROOT=''");
        return Ok(0);
    }

    let ctx = AppContext::from_config(paths, config, std::env::current_dir()?, None)?;
    let profile = ctx
        .repository
        .as_ref()
        .and_then(|_| ctx.profile_id())
        .unwrap_or("");
    let root = ctx
        .repository
        .as_ref()
        .and_then(|repository| repository.root.as_deref())
        .map(|path| path.to_string_lossy())
        .unwrap_or_default();
    println!("GHIS_CHPWD_ENABLED=1");
    println!("GHIS_REPO_PROFILE={}", shell::shell_quote(profile));
    println!(
        "GHIS_REPO_PROFILE_DISPLAY={}",
        shell::shell_quote(&diagnostics::sanitize_display_text(profile))
    );
    println!("GHIS_REPO_ROOT={}", shell::shell_quote(&root));
    Ok(0)
}

fn agent_context(
    path: Option<&Path>,
    explicit: Option<&str>,
    args: ContextArgs,
) -> app::Result<i32> {
    let cwd = args.cwd.unwrap_or(std::env::current_dir()?);
    let context = context(path, explicit, cwd)?;
    let context = AgentContext::from_app(&context);
    if args.require_resolved {
        context
            .require_resolved()
            .map_err(|error| app::AppError::Message(error.to_string()))?;
    }
    let format = match args.format {
        ContextFormatArg::Json => AgentContextFormat::Json,
        ContextFormatArg::Claude => AgentContextFormat::Claude,
        ContextFormatArg::Codex => AgentContextFormat::Codex,
    };
    println!(
        "{}",
        context
            .render(format)
            .map_err(|error| app::AppError::Message(error.to_string()))?
    );
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

fn onboard(path: Option<&Path>, args: OnboardArgs) -> app::Result<i32> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err(app::AppError::Message(
            "`ghis onboard` 需要交互终端；脚本环境请使用 `ghis profile add ...`、`ghis use ...` 和 `ghis setup --yes`。".into(),
        ));
    }
    let (paths, config) = load_config(path)?;
    let target = args.repo.unwrap_or(std::env::current_dir()?);
    let repository = ghis::repo::discover(&target).ok();
    let discovery = match github::discover_accounts(None) {
        Ok(value) => value,
        Err(_) => github::load_discovery_cache(&discovery_cache_path(&paths))?.unwrap_or_default(),
    };
    let mut prompt = ghis::onboarding::Prompt::new(
        BufReader::new(io::stdin()),
        io::stderr(),
        std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM")
                .map(|value| value != "dumb")
                .unwrap_or(true),
    );
    let result = ghis::onboarding::collect(
        &mut prompt,
        &config,
        &discovery.accounts,
        |host, login| github::profile_email_candidates(host, login).unwrap_or_default(),
        repository.as_ref().map(|value| value.command_dir()),
    )?;
    let draft = match result {
        ghis::onboarding::FlowResult::Cancelled | ghis::onboarding::FlowResult::Back => {
            eprintln!("已取消，未写入任何文件。");
            return Ok(0);
        }
        ghis::onboarding::FlowResult::Complete(draft) => draft,
    };
    eprintln!("\n── ghis 初始设置 ───────────────────────────────────────────── 5 / 5 ──");
    eprintln!(
        "[完成] 目标  ──>  [完成] 账号  ──>  [完成] 身份  ──>  [完成] 选项  ──>  [当前] 确认"
    );
    eprintln!("\n将执行：");
    eprintln!(
        "  ✓ 写入 Profile `{}` 到 {}",
        draft.id,
        paths.config_file.display()
    );
    if draft.make_default {
        eprintln!("  ✓ 设置默认 Profile 为 `{}`", draft.id);
    }
    if draft.bind_repository {
        eprintln!("  ✓ 绑定当前 Git worktree：{}", target.display());
    }
    if draft.setup_zsh {
        eprintln!("  ✓ 安装 zsh wrapper");
    }
    eprintln!("\n继续吗？[y/N]：");
    if !matches!(
        prompt.confirm_final()?,
        ghis::onboarding::FlowResult::Complete(true)
    ) {
        eprintln!("已取消，未写入任何文件。");
        return Ok(0);
    }
    let profile = Profile {
        host: github::normalize_host(&draft.host),
        login: draft.login,
        git_name: draft.git_name,
        git_email: draft.git_email,
        description: draft.description,
        ssh: Some(SshProfile::default()),
        signing: SigningProfile::default(),
    };
    let (saved, ()) = update_config(&paths, |config| {
        if config.profiles.contains_key(&draft.id) {
            return Err(app::AppError::Message(format!(
                "profile `{}` 已存在",
                draft.id
            )));
        }
        config.profiles.insert(draft.id.clone(), profile);
        if draft.make_default {
            config.behavior.default_profile = Some(draft.id.clone());
        }
        Ok(())
    })?;
    app::sync_fragments(&paths, &saved)?;
    if draft.bind_repository {
        let ctx = context(path, Some(&draft.id), &target)?;
        app::bind_repository(&ctx, &draft.id)?;
    }
    if draft.setup_zsh {
        let home =
            std::env::var_os("HOME").ok_or_else(|| app::AppError::Message("HOME 未设置".into()))?;
        let zshrc = shell::zshrc_path(Path::new(&home), std::env::var_os("ZDOTDIR").as_deref());
        let init = paths.config_dir.join("init.zsh");
        shell::setup(zshrc, &init, "ghis")?;
    }
    println!("已创建 Profile `{}`。", draft.id);
    if draft.bind_repository {
        println!(
            "已绑定到当前 Git worktree。\nHint: 运行 ghis status 查看当前身份。\nHint: 也可稍后运行 ghis doctor 检查 SSH 和 signing。"
        );
    } else {
        println!(
            "Hint: 可稍后运行 ghis use {} --repo {} 完成绑定。",
            draft.id,
            target.display()
        );
    }
    if !draft.setup_zsh {
        println!("Hint: 可稍后运行 ghis setup；脚本中使用 ghis setup --yes。");
    }
    Ok(0)
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
                    match profile.description.as_deref() {
                        Some(description) => println!("{id}\n  {description}"),
                        None => println!("{id}"),
                    }
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
                println!("Profile：{id}");
                if let Some(description) = profile.description.as_deref() {
                    println!("描述：{description}");
                }
                println!(
                    "提交身份：{} <{}>\nGitHub：{}@{}\n签名：{}",
                    profile.git_name,
                    profile.git_email,
                    profile.login,
                    profile.host,
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
        description: args.description,
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
    if args.clear_description {
        profile.description = None;
    } else if let Some(description) = args.description {
        profile.description = Some(description);
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
    println!("已绑定 Profile `{id}`。\n{}", ctx.identity_banner());
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
        "display_profile_on_chpwd" => config.behavior.display_profile_on_chpwd.to_string(),
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
        "display_profile_on_chpwd" => config.behavior.display_profile_on_chpwd = parse_bool(value)?,
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

const KNOWN_BEHAVIOR_KEYS: [&str; 7] = [
    "default_profile",
    "auto_bind",
    "unresolved",
    "credential_failure",
    "ssh_unmanaged",
    "display_identity",
    "display_profile_on_chpwd",
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
    checks: Vec<diagnostics::DiagnosticCheck>,
    repairs: Vec<diagnostics::RepairAction>,
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
    let git_status = tool_status("git", &["--version"]);
    let gh_status = tool_status("gh", &["--version"]);
    let zsh_status = tool_status("zsh", &["--version"]);
    let checks = build_doctor_checks(
        &ctx,
        &shell_integration,
        &git_status,
        &gh_status,
        &zsh_status,
        credential_available,
        ssh_agent.as_ref(),
        &git_config,
    );
    let repairs = collect_repairs(&checks, &git_config);

    let report = DoctorReport {
        schema_version: ghis::SCHEMA_VERSION,
        git: git_status,
        gh: gh_status,
        zsh: zsh_status,
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
        checks,
        repairs,
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
        if !report.repairs.is_empty() {
            println!("修复建议:");
            for repair in &report.repairs {
                println!(
                    "  [{}] {}{}",
                    repair.kind.label(),
                    repair.command,
                    if repair.confirmation {
                        "（执行前确认）"
                    } else {
                        ""
                    }
                );
            }
        }
        for warning in &report.warnings {
            eprintln!("警告：{warning}");
        }
    }
    Ok(0)
}

#[allow(clippy::too_many_arguments)]
fn build_doctor_checks(
    ctx: &AppContext,
    shell: &DoctorShellIntegration,
    git: &ToolStatus,
    gh: &ToolStatus,
    zsh: &ToolStatus,
    credential_available: Option<bool>,
    ssh_agent: Option<&DoctorAgent>,
    git_config: &diagnostics::GitConfigReport,
) -> Vec<diagnostics::DiagnosticCheck> {
    use diagnostics::{DiagnosticCheck, DiagnosticCode, RepairAction, Severity};
    let mut checks = Vec::new();
    for (name, status) in [("git", git), ("gh", gh), ("zsh", zsh)] {
        checks.push(DiagnosticCheck::new(
            match name {
                "gh" => DiagnosticCode::GhAuth,
                "zsh" => DiagnosticCode::ShellIntegration,
                _ => DiagnosticCode::GlobalGitConfig,
            },
            if status.available {
                Severity::Info
            } else {
                Severity::Error
            },
            format!(
                "{name} {}",
                if status.available {
                    "可用"
                } else {
                    "不可用"
                }
            ),
            None,
        ));
    }
    checks.push(DiagnosticCheck::new(
        DiagnosticCode::ShellIntegration,
        if shell.wrapper_loaded {
            Severity::Info
        } else {
            Severity::Warning
        },
        shell.advice.clone(),
        (!shell.wrapper_loaded).then(|| RepairAction::confirmation("ghis setup")),
    ));
    if credential_available == Some(false) {
        let command = ctx.profile.as_ref().map_or_else(
            || "gh auth login".to_string(),
            |profile| format!("gh auth login --hostname {}", profile.host),
        );
        checks.push(DiagnosticCheck::new(
            DiagnosticCode::GhAuth,
            Severity::Error,
            "当前 Profile 的 GitHub 凭据不可用",
            Some(RepairAction::manual(command)),
        ));
    }
    if matches!(
        ctx.resolution.source,
        ResolutionSource::InvalidRepositoryBinding
    ) {
        checks.push(DiagnosticCheck::new(
            DiagnosticCode::InvalidBinding,
            Severity::Error,
            "仓库绑定的 Profile 已不存在；必须选择新的 Profile 或解绑",
            Some(RepairAction::confirmation(
                "ghis use <profile>  # 或 ghis unbind",
            )),
        ));
    }
    if ssh_agent.is_some_and(|agent| !agent.available) {
        checks.push(DiagnosticCheck::new(
            DiagnosticCode::SshKey,
            Severity::Error,
            "当前 Profile 所需的 SSH Agent/key 不可用",
            Some(RepairAction::manual("ssh-add -L")),
        ));
    }
    if !git_config.diagnostics.is_empty() {
        checks.push(DiagnosticCheck::new(
            DiagnosticCode::GlobalGitConfig,
            if git_config.errors > 0 {
                Severity::Error
            } else {
                Severity::Warning
            },
            diagnostics::summary_line(git_config),
            Some(RepairAction::manual(
                "git config --show-origin --show-scope --list",
            )),
        ));
    }
    checks.push(DiagnosticCheck::new(
        DiagnosticCode::GlobalGitConfig,
        Severity::Info,
        "可安全重建 ghis fragment 和已登记仓库配置",
        Some(RepairAction::automatic("ghis sync")),
    ));
    checks
}

fn collect_repairs(
    checks: &[diagnostics::DiagnosticCheck],
    report: &diagnostics::GitConfigReport,
) -> Vec<diagnostics::RepairAction> {
    let mut seen = BTreeSet::new();
    checks
        .iter()
        .filter_map(|check| check.repair.clone())
        .chain(
            report
                .diagnostics
                .iter()
                .filter_map(|item| item.repair.clone()),
        )
        .filter(|repair| seen.insert((repair.kind as u8, repair.command.clone())))
        .collect()
}

#[derive(Serialize)]
struct CheckReport {
    schema_version: u32,
    operation: CheckOperation,
    checks: Vec<diagnostics::DiagnosticCheck>,
    repairs: Vec<diagnostics::RepairAction>,
}

fn check(path: Option<&Path>, explicit: Option<&str>, args: CheckArgs) -> app::Result<i32> {
    let ctx = context(path, explicit, std::env::current_dir()?)?;
    let cwd = ctx
        .repository
        .as_ref()
        .map_or(std::env::current_dir()?, |repo| {
            repo.command_dir().to_path_buf()
        });
    let git_config = diagnostics::scan_git_config(
        &cwd,
        ctx.profile.as_ref(),
        ctx.remote.as_ref(),
        ctx.identities.as_ref(),
    )?;
    let shell = doctor_shell_integration(&ctx);
    let checks = build_doctor_checks(
        &ctx,
        &shell,
        &tool_status("git", &["--version"]),
        &tool_status("gh", &["--version"]),
        &tool_status("zsh", &["--version"]),
        None,
        None,
        &git_config,
    );
    let repairs = collect_repairs(&checks, &git_config);
    let report = CheckReport {
        schema_version: ghis::SCHEMA_VERSION,
        operation: args.operation,
        checks,
        repairs,
    };
    if args.json {
        print_json(&report)?;
    } else {
        for check in &report.checks {
            println!(
                "{}\t{:?}\t{}",
                check.severity.label(),
                check.code,
                check.summary
            );
        }
        for repair in &report.repairs {
            println!("[{}]\t{}", repair.kind.label(), repair.command);
        }
    }
    Ok(
        if report
            .checks
            .iter()
            .any(|check| check.severity == diagnostics::Severity::Error)
        {
            1
        } else {
            0
        },
    )
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
    match health {
        shell::IntegrationHealth::Healthy => DoctorShellIntegrationState::WrapperLoaded,
        shell::IntegrationHealth::Incomplete => DoctorShellIntegrationState::WrapperIncomplete,
        shell::IntegrationHealth::NotLoaded if setup_installed => {
            DoctorShellIntegrationState::InstalledNotLoaded
        }
        shell::IntegrationHealth::NotLoaded if repository_bound => {
            DoctorShellIntegrationState::RepositoryOnly
        }
        shell::IntegrationHealth::NotLoaded => DoctorShellIntegrationState::NotIntegrated,
    }
}

fn doctor_shell_integration_text(state: DoctorShellIntegrationState) -> &'static str {
    match state {
        DoctorShellIntegrationState::WrapperLoaded => "当前 shell 已完整加载",
        DoctorShellIntegrationState::WrapperIncomplete => "当前 shell wrapper 依赖不完整",
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
            "GHIS marker 存在，但函数依赖不完整；运行 `source ${XDG_CONFIG_HOME:-$HOME/.config}/ghis/init.zsh` 或重新启动 zsh"
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
            classify_shell_integration(shell::IntegrationHealth::Healthy, true, true),
            DoctorShellIntegrationState::WrapperLoaded
        );
        assert_eq!(
            classify_shell_integration(shell::IntegrationHealth::Incomplete, true, true),
            DoctorShellIntegrationState::WrapperIncomplete
        );
        assert_eq!(
            classify_shell_integration(shell::IntegrationHealth::NotLoaded, true, true),
            DoctorShellIntegrationState::InstalledNotLoaded
        );
        assert_eq!(
            classify_shell_integration(shell::IntegrationHealth::NotLoaded, false, true),
            DoctorShellIntegrationState::RepositoryOnly
        );
        assert_eq!(
            classify_shell_integration(shell::IntegrationHealth::NotLoaded, false, false),
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
}
