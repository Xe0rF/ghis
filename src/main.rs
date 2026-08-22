use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use fd_lock::RwLock;
use ghis::agent_context::{AgentContext, AgentContextFormat};
use ghis::app::{self, AppContext};
use ghis::config::{
    Config, ConfigPaths, CredentialFailurePolicy, DisplayIdentity, Profile, ResolutionSource, Rule,
    SigningProfile, SigningTransport, SshMode, SshProfile, SshUnmanagedPolicy, UnresolvedPolicy,
};
use ghis::{credential, diagnostics, github, platform, shell, signing};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
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
    /// 输出供 prompt 和 direnv 使用的最小、无凭据解析状态
    Prompt(PromptArgs),
    /// 输出供 coding agent 使用的最小、脱敏仓库身份上下文
    Context(ContextArgs),
    /// 从 gh CLI 发现已有账号
    Discover(JsonArgs),
    /// 通过中文交互向导创建 Profile 并可选初始化当前仓库
    Onboard(OnboardArgs),
    /// 管理 profile
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// 将 profile 绑定到仓库
    Use {
        profile: String,
        #[arg(short = 'r', long)]
        repo: Option<PathBuf>,
    },
    /// 解除当前仓库绑定
    Unbind {
        #[arg(short = 'r', long)]
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
    /// 管理 Shell 包装器、初始化脚本与补全
    Shell {
        #[command(subcommand)]
        command: ShellCommand,
    },
    /// 撤销 ghis 管理的用户集成
    Teardown(TeardownArgs),
    /// 由 shell wrapper 调用，透明执行真实 git
    #[command(trailing_var_arg = true)]
    Git(Passthrough),
    /// 由 shell wrapper 调用，为真实 gh 注入当前 profile token
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

#[derive(Debug, Subcommand)]
enum ShellCommand {
    /// 安装 Shell 包装器
    Setup(SetupArgs),
    /// 移除 ghis 管理的 Shell 集成
    Uninstall(ShellArgs),
    /// 输出 Shell 初始化脚本
    Init(ShellArgs),
    /// 输出 Shell 补全脚本
    Completion(ShellArgs),
}

#[derive(Debug, Args, Default)]
struct ShellArgs {
    /// Shell family; when omitted, ghis detects the current shell.
    #[arg(value_name = "SHELL", value_parser = parse_shell_kind)]
    shell: Option<shell::ShellKind>,
}

fn parse_shell_kind(value: &str) -> Result<shell::ShellKind, String> {
    value
        .parse()
        .map_err(|error: shell::ShellError| error.to_string())
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
    #[arg(short = 'C', long)]
    cwd: Option<PathBuf>,
    /// 未解析、歧义或失效选择时返回错误
    #[arg(long)]
    require_resolved: bool,
}

#[derive(Debug, Args, Default)]
struct JsonArgs {
    #[arg(short = 'j', long)]
    json: bool,
}

#[derive(Debug, Args, Default)]
struct StatusArgs {
    #[arg(short = 'j', long)]
    json: bool,
    /// JSON 中隐藏环境路径、配置来源和程序细节
    #[arg(long)]
    redacted: bool,
    /// 供 chpwd hook 使用：不访问网络，也不输出正文
    #[arg(long, hide = true)]
    shell: bool,
    #[arg(long, hide = true)]
    quiet: bool,
}

#[derive(Debug, Args, Default)]
struct PromptArgs {
    /// Output format. JSON is the stable default; profile emits only the ID for direnv.
    #[arg(long, value_enum, default_value_t = PromptFormatArg::Json)]
    format: PromptFormatArg,
}

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
enum PromptFormatArg {
    #[default]
    Json,
    Profile,
}

#[derive(Debug, Args, Default)]
struct DoctorArgs {
    #[arg(short = 'j', long)]
    json: bool,
    /// JSON 中隐藏环境路径、配置来源和程序细节
    #[arg(long)]
    redacted: bool,
    /// 显式访问 GitHub API，核对当前 Profile 的 SSH signing 公钥
    #[arg(long, visible_alias = "check-github-signing-keys")]
    check_github_signing_key: bool,
}

#[derive(Debug, Args)]
struct CheckArgs {
    #[arg(long, value_enum)]
    operation: CheckOperation,
    #[arg(short = 'j', long)]
    json: bool,
    /// JSON 中隐藏环境路径、配置来源和程序细节
    #[arg(long)]
    redacted: bool,
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
        #[arg(short = 'y', long)]
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
        #[arg(short = 'j', long)]
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
    #[command(flatten)]
    shell: ShellArgs,
    /// 只打印将要写入的 shell 初始化脚本
    #[arg(long)]
    print: bool,
    /// 跳过修改 shell 启动文件前的确认，供脚本安装使用
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct TeardownArgs {
    #[command(flatten)]
    shell: ShellArgs,
    /// 仅列出将执行的清理，不修改文件或仓库
    #[arg(long)]
    dry_run: bool,
    /// 同时删除当前配置 namespace 中的 ghis 用户数据
    #[arg(long)]
    purge: bool,
    /// 跳过 --purge 的交互确认
    #[arg(short = 'y', long, requires = "purge")]
    yes: bool,
    /// 清理指定仓库；默认检查当前目录
    #[arg(short = 'r', long)]
    repo: Option<PathBuf>,
}

#[derive(Debug, Args, Default)]
struct OnboardArgs {
    /// 评估并可选绑定的仓库路径，默认使用当前目录
    #[arg(short = 'r', long)]
    repo: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct Passthrough {
    #[arg(allow_hyphen_values = true)]
    args: Vec<std::ffi::OsString>,
}

#[derive(Debug, Args, Default)]
struct ProfileListArgs {
    #[arg(short = 'j', long, conflicts_with = "details")]
    json: bool,
    /// 显示所有 Profile 的人类可读详情
    #[arg(short = 'd', long)]
    details: bool,
}

#[derive(Debug, Subcommand)]
enum ProfileCommand {
    List(ProfileListArgs),
    Show {
        id: String,
        #[arg(short = 'j', long)]
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
    #[arg(short = 'j', long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ProfileArgs {
    id: String,
    #[arg(short = 'H', long, default_value = "github.com")]
    host: String,
    #[arg(short = 'l', long)]
    login: String,
    #[arg(short = 'n', long = "name")]
    git_name: String,
    /// 用于状态和列表展示的人类可读说明
    #[arg(short = 'd', long)]
    description: Option<String>,
    #[arg(short = 'e', long = "email", conflicts_with = "noreply")]
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
    /// Explicit managed SSH jump hosts, in traversal order.
    #[arg(long = "proxy-jump", value_delimiter = ',')]
    proxy_jump: Vec<String>,
    /// Forward the selected Agent through managed SSH hops.
    #[arg(long)]
    forward_agent: bool,
    #[arg(long)]
    sign: bool,
    #[arg(long)]
    signing_key: Option<String>,
    /// Fingerprint of the SSH key used only for commit signing.
    #[arg(long)]
    signing_fingerprint: Option<String>,
    #[arg(long)]
    signing_program: Option<PathBuf>,
    /// SSH signing agent source; forwarded-agent uses only SSH_AUTH_SOCK.
    #[arg(long, value_enum, default_value_t = SigningTransportArg::LocalAgent)]
    signing_transport: SigningTransportArg,
}

#[derive(Debug, Args)]
struct ProfileEditArgs {
    id: String,
    #[arg(short = 'H', long)]
    host: Option<String>,
    #[arg(short = 'l', long)]
    login: Option<String>,
    #[arg(short = 'n', long = "name")]
    git_name: Option<String>,
    /// 设置用于状态和列表展示的人类可读说明
    #[arg(short = 'd', long, conflicts_with = "clear_description")]
    description: Option<String>,
    /// 清除 Profile 的人类可读说明
    #[arg(long, conflicts_with = "description")]
    clear_description: bool,
    #[arg(short = 'e', long = "email", conflicts_with = "noreply")]
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
    /// Replace the explicit managed SSH jump-host chain.
    #[arg(
        long = "proxy-jump",
        value_delimiter = ',',
        conflicts_with = "clear_proxy_jump"
    )]
    proxy_jump: Option<Vec<String>>,
    /// Clear the explicit managed SSH jump-host chain.
    #[arg(long, conflicts_with = "proxy_jump")]
    clear_proxy_jump: bool,
    /// Enable or disable Agent forwarding for managed SSH hops.
    #[arg(
        long,
        action = clap::ArgAction::Set,
        num_args = 0..=1,
        default_missing_value = "true"
    )]
    forward_agent: Option<bool>,
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
    /// Fingerprint of the SSH key used only for commit signing.
    #[arg(long)]
    signing_fingerprint: Option<String>,
    #[arg(long)]
    signing_program: Option<PathBuf>,
    /// SSH signing agent source; forwarded-agent uses only SSH_AUTH_SOCK.
    #[arg(long, value_enum)]
    signing_transport: Option<SigningTransportArg>,
}

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
enum SigningTransportArg {
    #[default]
    LocalAgent,
    ForwardedAgent,
}

impl From<SigningTransportArg> for SigningTransport {
    fn from(value: SigningTransportArg) -> Self {
        match value {
            SigningTransportArg::LocalAgent => Self::LocalAgent,
            SigningTransportArg::ForwardedAgent => Self::ForwardedAgent,
        }
    }
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
    /// 匹配执行 ghis 时的工作目录，支持 glob 和 ~
    #[arg(long)]
    cwd: Option<String>,
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
    let cli = parse_cli();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("ghis: {error}");
            std::process::exit(2);
        }
    }
}

fn parse_cli() -> Cli {
    #[cfg(windows)]
    if let Some(command) = ghis::agent::windows_native_shim_command() {
        let mut arguments = Vec::with_capacity(std::env::args_os().len() + 2);
        arguments.push(std::ffi::OsString::from("ghis"));
        arguments.push(std::ffi::OsString::from(command));
        arguments.push(std::ffi::OsString::from("--"));
        arguments.extend(std::env::args_os().skip(1));
        return Cli::parse_from(arguments);
    }

    Cli::parse()
}

#[derive(Clone, Copy)]
enum OperationLockMode {
    None,
    Shared,
    Exclusive,
}

fn operation_lock_mode(command: Option<&Commands>) -> OperationLockMode {
    match command {
        Some(Commands::Teardown(_)) => OperationLockMode::Exclusive,
        Some(
            Commands::Discover(_)
            | Commands::Onboard(_)
            | Commands::Profile { .. }
            | Commands::Use { .. }
            | Commands::Unbind { .. }
            | Commands::Rule { .. }
            | Commands::Config { .. }
            | Commands::Sync
            | Commands::Doctor(_),
        ) => OperationLockMode::Shared,
        Some(Commands::Agent {
            command: AgentCommand::Setup { .. } | AgentCommand::Uninstall { .. },
        }) => OperationLockMode::Shared,
        Some(Commands::Shell {
            command: ShellCommand::Setup(_) | ShellCommand::Uninstall(_),
        }) => OperationLockMode::Shared,
        _ => OperationLockMode::None,
    }
}

fn operation_lock() -> app::Result<RwLock<std::fs::File>> {
    let paths = ConfigPaths::discover()?;
    let digest = Sha256::digest(paths.config_dir.as_os_str().as_encoded_bytes());
    let mut namespace = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut namespace, "{byte:02x}").expect("writing a digest to a string cannot fail");
    }
    let path = std::env::temp_dir().join(format!("ghis-{namespace}.operation.lock"));
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    Ok(RwLock::new(file))
}

fn run(cli: Cli) -> app::Result<i32> {
    match operation_lock_mode(cli.command.as_ref()) {
        OperationLockMode::None => dispatch(cli),
        OperationLockMode::Shared => {
            let lock = operation_lock()?;
            let _guard = lock.read()?;
            dispatch(cli)
        }
        OperationLockMode::Exclusive => {
            let mut lock = operation_lock()?;
            let _guard = lock.write()?;
            dispatch(cli)
        }
    }
}

fn dispatch(cli: Cli) -> app::Result<i32> {
    let command = cli
        .command
        .unwrap_or_else(|| Commands::Status(StatusArgs::default()));
    match command {
        Commands::Status(args) => status(cli.config.as_deref(), cli.profile.as_deref(), args),
        Commands::Prompt(args) => prompt(cli.config.as_deref(), cli.profile.as_deref(), args),
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
            args.redacted,
            args.check_github_signing_key,
        ),
        Commands::Check(args) => check(cli.config.as_deref(), cli.profile.as_deref(), args),
        Commands::Agent { command } => {
            agent_command(cli.config.as_deref(), cli.profile.as_deref(), command)
        }
        Commands::Shell { command } => shell_command(command),
        Commands::Teardown(args) => teardown(cli.config.as_deref(), args),
        Commands::Git(args) => {
            let argument_view = args
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let cwd = git_working_directory(&argument_view)?;
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
            let binary = PathBuf::from(agent_binary());
            let shims = ghis::agent::SessionShims::create(&binary)?;
            let session_path = ghis::agent::session_path(shims.directory())?;
            let spec = match target {
                AgentTarget::Claude => {
                    ghis::agent::claude::run_spec(&context, "claude", args, &cwd)
                }
                AgentTarget::Codex => ghis::agent::codex::run_spec_with_shell_path(
                    &context,
                    "codex",
                    args,
                    &cwd,
                    &session_path,
                ),
            }
            .map_err(|error| app::AppError::Message(error.to_string()))?;
            let spec = ghis::agent::prepend_path(spec, shims.directory())?;
            let launched = ghis::agent::launch_session(&spec);
            drop(shims);
            launched.map_err(|error| app::AppError::Message(error.to_string()))
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
            if !yes {
                eprint!("将更新 Claude Code 设置 {}，继续吗？[y/N] ", path.display());
                io::stderr().flush()?;
                let mut answer = String::new();
                io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                    println!("已取消，未修改 Claude Code 设置。");
                    return Ok(0);
                }
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
        AgentScope::User => Ok(platform::user_home()
            .map_err(|error| app::AppError::Message(format!("无法解析用户目录：{error}")))?
            .join(".claude/settings.json")),
    }
}
fn agent_binary() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
        .unwrap_or_else(|| "ghis".into())
}

/// How the active command treats a missing explicit configuration file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConfigLoadMode {
    /// Read/execute commands must not silently resolve identities from
    /// defaults when the operator selected a config file that is absent.
    Strict,
    /// Commands whose purpose is creating or extending the configuration may
    /// initialize a missing file.
    AllowCreate,
}

fn load_config_with_mode(
    path: Option<&Path>,
    mode: ConfigLoadMode,
) -> app::Result<(ConfigPaths, Config)> {
    let mut paths = ConfigPaths::discover()?;
    let explicit = path.is_some();
    if let Some(path) = path {
        paths.set_config_file(path)?;
    }
    // The default XDG path stays lenient on first use; only an explicitly
    // selected file can trigger strict loading.
    let config = match (explicit, mode) {
        (true, ConfigLoadMode::Strict) => Config::load_required(&paths.config_file)?,
        _ => Config::load(&paths.config_file)?,
    };
    Ok((paths, config))
}

fn load_config(path: Option<&Path>) -> app::Result<(ConfigPaths, Config)> {
    load_config_with_mode(path, ConfigLoadMode::Strict)
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
    if args.redacted {
        print_redacted_json(&report)?;
    } else if args.json {
        print_json(&report)?;
    } else {
        println!("{}", ctx.status_summary());
        for warning in &ctx.warnings {
            eprintln!("警告：{warning}");
        }
    }
    Ok(0)
}

fn prompt(path: Option<&Path>, explicit: Option<&str>, args: PromptArgs) -> app::Result<i32> {
    let (paths, config) = load_config(path)?;
    let ctx =
        AppContext::from_config_for_prompt(paths, config, std::env::current_dir()?, explicit)?;
    let report = app::prompt_status(&ctx);
    match args.format {
        PromptFormatArg::Json => print_json(&report)?,
        PromptFormatArg::Profile => {
            if let Some(profile) = report.profile.as_deref() {
                if profile.chars().any(char::is_control) {
                    return Err(app::AppError::Message(
                        "`prompt --format profile` cannot render a profile id containing control characters; use JSON output instead".into(),
                    ));
                }
                println!("{profile}");
            }
        }
    }
    Ok(i32::from(!report.is_resolved()))
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
            "`ghis onboard` 需要交互终端；脚本环境请使用 `ghis profile add ...`、`ghis use ...` 和 `ghis shell setup --yes`。".into(),
        ));
    }

    let (paths, config) = load_config_with_mode(path, ConfigLoadMode::AllowCreate)?;
    let target = args.repo.unwrap_or(std::env::current_dir()?);
    let repository = match ghis::repo::discover(&target) {
        Ok(repository) => Some(repository),
        Err(ghis::repo::RepoError::NotRepository { .. }) => None,
        Err(error) => return Err(error.into()),
    };
    let (accounts, discovery_notice) = match github::discover_accounts(None) {
        Ok(discovery) => {
            let mut notices = Vec::new();
            if discovery.offline {
                notices.push("部分账号当前无法联网验证。".to_owned());
            }
            if let Some(warning) = cache_discovery(&paths, &discovery) {
                notices.push(warning);
            }
            (
                discovery.accounts,
                (!notices.is_empty()).then(|| notices.join(" ")),
            )
        }
        Err(discovery_error) => match github::load_discovery_cache(&discovery_cache_path(&paths)) {
            Ok(Some(cached)) => (
                cached.accounts,
                Some(format!(
                    "无法从 gh 刷新账号，已使用本地缓存：{discovery_error}"
                )),
            ),
            Ok(None) => (
                Vec::new(),
                Some(format!(
                    "无法从 gh 发现账号，且没有可用缓存：{discovery_error}"
                )),
            ),
            Err(cache_error) => (
                Vec::new(),
                Some(format!(
                    "无法从 gh 发现账号（{discovery_error}），读取缓存也失败（{cache_error}）。"
                )),
            ),
        },
    };

    let color = std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM")
            .map(|value| value != "dumb")
            .unwrap_or(true);
    let line_mode = std::env::var("TERM")
        .map(|value| value == "dumb")
        .unwrap_or(false)
        || std::env::var_os("GHIS_ONBOARD_LINE_MODE").is_some();
    let repository_path = repository.as_ref().map(|value| value.command_dir());
    let mut load_candidates = |host: &str, login: &str| {
        github::profile_email_candidates(host, login).map_err(|error| error.to_string())
    };
    let result = if line_mode {
        ghis::onboarding::run_line_mode(
            BufReader::new(io::stdin()),
            io::stderr(),
            &config,
            &accounts,
            discovery_notice,
            repository_path,
            &mut load_candidates,
        )?
    } else {
        match ghis::onboarding::run_terminal(
            io::stderr(),
            &config,
            &accounts,
            discovery_notice.clone(),
            repository_path,
            color,
            &mut load_candidates,
        ) {
            Ok(result) => result,
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                eprintln!("警告：{error}，已切换到逐行兼容模式。");
                ghis::onboarding::run_line_mode(
                    BufReader::new(io::stdin()),
                    io::stderr(),
                    &config,
                    &accounts,
                    discovery_notice,
                    repository_path,
                    &mut load_candidates,
                )?
            }
            Err(error) => return Err(error.into()),
        }
    };
    let draft = match result {
        ghis::onboarding::FlowResult::Cancelled => return Ok(0),
        ghis::onboarding::FlowResult::Complete(draft) => draft,
    };

    apply_onboarding(path, &paths, &target, repository.as_ref(), draft)
}

fn apply_onboarding(
    path: Option<&Path>,
    paths: &ConfigPaths,
    target: &Path,
    repository: Option<&ghis::repo::Repository>,
    draft: ghis::onboarding::Draft,
) -> app::Result<i32> {
    let profile = Profile {
        host: github::normalize_host(&draft.host),
        login: draft.login.clone(),
        git_name: draft.git_name.clone(),
        git_email: draft.git_email.clone(),
        description: draft.description.clone(),
        ssh: Some(SshProfile::default()),
        signing: SigningProfile::default(),
    };
    let previous_default = Config::load(&paths.config_file)?.behavior.default_profile;
    let previous_binding = if draft.bind_repository {
        repository
            .map(|repository| ghis::repo::local_config(repository, app::PROFILE_CONFIG_KEY))
            .transpose()?
            .flatten()
    } else {
        None
    };

    let (saved, ()) = update_config(paths, |config| {
        if config.profiles.contains_key(&draft.id) {
            return Err(app::AppError::Message(format!(
                "Profile `{}` 已存在，请重新运行向导。",
                draft.id
            )));
        }
        config.profiles.insert(draft.id.clone(), profile.clone());
        if draft.make_default {
            config.behavior.default_profile = Some(draft.id.clone());
        }
        Ok(())
    })?;

    if let Err(error) = app::sync_fragments(paths, &saved) {
        let rollback =
            rollback_onboarding_profile(paths, &draft.id, &profile, previous_default.as_deref());
        return Err(with_rollback_context(error, rollback));
    }

    let mut binding_applied = false;
    if draft.bind_repository {
        let result = context(path, Some(&draft.id), target)
            .and_then(|ctx| app::bind_repository(&ctx, &draft.id));
        if let Err(error) = result {
            let binding_rollback =
                restore_onboarding_binding(path, target, &draft.id, previous_binding.as_deref());
            let profile_rollback = rollback_onboarding_profile(
                paths,
                &draft.id,
                &profile,
                previous_default.as_deref(),
            );
            return Err(with_two_rollback_context(
                error,
                binding_rollback,
                profile_rollback,
            ));
        }
        binding_applied = true;
    }

    if draft.setup_zsh {
        let setup_result = (|| -> app::Result<()> {
            let home = std::env::var_os("HOME")
                .ok_or_else(|| app::AppError::Message("HOME 未设置".into()))?;
            let renderer = shell::renderer(shell::ShellKind::Zsh)
                .map_err(|error| app::AppError::Message(error.to_string()))?;
            let init = paths.config_dir.join(renderer.spec().init_file_name());
            let startup_file = renderer.startup_file(Path::new(&home));
            renderer.install(&startup_file, &init, &renderer.render_init("ghis"))?;
            Ok(())
        })();
        if let Err(error) = setup_result {
            let binding_rollback = if binding_applied {
                restore_onboarding_binding(path, target, &draft.id, previous_binding.as_deref())
            } else {
                Ok(())
            };
            let profile_rollback = rollback_onboarding_profile(
                paths,
                &draft.id,
                &profile,
                previous_default.as_deref(),
            );
            return Err(with_two_rollback_context(
                error,
                binding_rollback,
                profile_rollback,
            ));
        }
    }

    println!("✓ 已完成 ghis 初始设置");
    println!("\n  Profile  {}", draft.id);
    println!("  GitHub   {}@{}", draft.login, draft.host);
    if draft.bind_repository {
        println!("  仓库     {}", target.display());
    }
    println!("\n后续可以运行：\n  ghis status\n  ghis doctor");
    Ok(0)
}

fn rollback_onboarding_profile(
    paths: &ConfigPaths,
    id: &str,
    profile: &Profile,
    previous_default: Option<&str>,
) -> app::Result<()> {
    let (config, ()) = update_config(paths, |config| {
        if config.profiles.get(id) == Some(profile) {
            config.profiles.remove(id);
        }
        if config.behavior.default_profile.as_deref() == Some(id) {
            config.behavior.default_profile = previous_default.map(str::to_owned);
        }
        Ok(())
    })?;
    app::sync_fragments(paths, &config)?;
    Ok(())
}

fn restore_onboarding_binding(
    path: Option<&Path>,
    target: &Path,
    new_id: &str,
    previous_binding: Option<&str>,
) -> app::Result<()> {
    if let Some(previous) = previous_binding {
        let ctx = context(path, Some(previous), target)?;
        app::bind_repository(&ctx, previous)
    } else {
        let ctx = context(path, Some(new_id), target)?;
        app::unbind_repository(&ctx)
    }
}

fn with_rollback_context(error: app::AppError, rollback: app::Result<()>) -> app::AppError {
    match rollback {
        Ok(()) => app::AppError::Message(format!("初始化失败，已恢复原状态：{error}")),
        Err(rollback_error) => app::AppError::Message(format!(
            "初始化失败：{error}；自动恢复也失败：{rollback_error}。请运行 `ghis doctor` 检查残留状态。"
        )),
    }
}

fn with_two_rollback_context(
    error: app::AppError,
    first: app::Result<()>,
    second: app::Result<()>,
) -> app::AppError {
    match (first, second) {
        (Ok(()), Ok(())) => app::AppError::Message(format!("初始化失败，已恢复原状态：{error}")),
        (first, second) => {
            let mut failures = Vec::new();
            if let Err(error) = first {
                failures.push(error.to_string());
            }
            if let Err(error) = second {
                failures.push(error.to_string());
            }
            app::AppError::Message(format!(
                "初始化失败：{error}；自动恢复不完整：{}。请运行 `ghis doctor` 检查残留状态。",
                failures.join("；")
            ))
        }
    }
}

fn profile_command(path: Option<&Path>, command: ProfileCommand) -> app::Result<i32> {
    // Only `profile add` is expected to bootstrap a missing config file;
    // every other subcommand inspects or edits an existing one.
    let mode = match command {
        ProfileCommand::Add(_) => ConfigLoadMode::AllowCreate,
        _ => ConfigLoadMode::Strict,
    };
    let (paths, config) = load_config_with_mode(path, mode)?;
    match command {
        ProfileCommand::List(args) => {
            if args.json {
                print_json(&config.profiles)?;
            } else if config.profiles.is_empty() {
                println!("尚未配置 profile。可先运行 `ghis discover`。")
            } else if args.details {
                for (index, (id, profile)) in config.profiles.iter().enumerate() {
                    if index > 0 {
                        println!();
                    }
                    println!("{}", profile_details(id, profile));
                }
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
                println!("{}", profile_details(&id, profile));
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

fn profile_details(id: &str, profile: &Profile) -> String {
    let mut lines = vec![format!("Profile：{id}")];
    if let Some(description) = profile.description.as_deref() {
        lines.push(format!("描述：{description}"));
    }
    lines.push(format!(
        "提交身份：{} <{}>",
        profile.git_name, profile.git_email
    ));
    lines.push(format!("GitHub：{}@{}", profile.login, profile.host));
    lines.push(format!(
        "签名：{}",
        if profile.signing.enabled {
            "开启"
        } else {
            "关闭"
        }
    ));
    if profile.signing.enabled {
        lines.push(format!(
            "签名传输：{}",
            match profile.signing.transport {
                SigningTransport::LocalAgent => "local-agent",
                SigningTransport::ForwardedAgent => "forwarded-agent",
            }
        ));
        lines.push(format!(
            "签名选择器：{}",
            match (
                profile.signing.signing_key.is_some(),
                profile.signing.fingerprint.is_some(),
            ) {
                (true, true) => "public key + fingerprint",
                (true, false) => "public key",
                (false, true) => "fingerprint",
                (false, false) => "未配置",
            }
        ));
    }
    lines.join("\n")
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
            proxy_jump: args.proxy_jump,
            forward_agent: args.forward_agent,
        }),
        signing: SigningProfile {
            enabled: args.sign,
            transport: args.signing_transport.into(),
            signing_key: args
                .signing_key
                .map(|value| signing::git_signing_key_value(&value)),
            fingerprint: args.signing_fingerprint,
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
        || args.proxy_jump.is_some()
        || args.clear_proxy_jump
        || args.forward_agent.is_some()
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
        if args.clear_proxy_jump {
            ssh.proxy_jump.clear();
        } else if let Some(proxy_jump) = args.proxy_jump {
            ssh.proxy_jump = proxy_jump;
        }
        if let Some(forward_agent) = args.forward_agent {
            ssh.forward_agent = forward_agent;
        }
    }

    if let Some(enabled) = args.sign {
        profile.signing.enabled = enabled;
    }
    if let Some(signing_key) = args.signing_key {
        profile.signing.signing_key = Some(signing::git_signing_key_value(&signing_key));
    }
    if let Some(fingerprint) = args.signing_fingerprint {
        profile.signing.fingerprint = Some(fingerprint);
    }
    if let Some(signing_program) = args.signing_program {
        profile.signing.program = Some(signing_program);
    }
    if let Some(transport) = args.signing_transport {
        profile.signing.transport = transport.into();
    }
}

fn use_profile(path: Option<&Path>, id: &str, repo: Option<&Path>) -> app::Result<i32> {
    let cwd = repo
        .map(Path::to_path_buf)
        .unwrap_or(std::env::current_dir()?);
    let ctx = context(path, Some(id), cwd)?;
    app::bind_repository(&ctx, id)?;
    println!("已绑定 Profile `{id}`。");
    if let Some(description) = ctx
        .profile
        .as_ref()
        .and_then(|profile| profile.description.as_deref())
    {
        println!("  {description}");
    }
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
    // `rule add` may bootstrap a missing explicit config; list/edit/remove
    // operate on an existing configuration.
    let mode = match command {
        RuleCommand::Add(_) => ConfigLoadMode::AllowCreate,
        _ => ConfigLoadMode::Strict,
    };
    let (paths, config) = load_config_with_mode(path, mode)?;
    match command {
        RuleCommand::List(args) => {
            if args.json {
                print_json(&config.rules)?;
            } else if config.rules.is_empty() {
                println!("尚未配置规则。")
            } else {
                for (index, rule) in config.rules.iter().enumerate() {
                    if index > 0 {
                        println!();
                    }
                    println!("{} → {}", rule.id, rule.profile);
                    if let Some(description) = config
                        .profiles
                        .get(&rule.profile)
                        .and_then(|profile| profile.description.as_deref())
                    {
                        println!("  Profile：{description}");
                    }
                    println!("  优先级：{}", rule.priority);
                    let matches = [
                        ("主机", rule.host.as_deref()),
                        ("所有者", rule.owner.as_deref()),
                        ("仓库", rule.repo.as_deref()),
                        ("远端", rule.remote.as_deref()),
                        ("Git 目录", rule.gitdir.as_deref()),
                        ("工作目录", rule.cwd.as_deref()),
                    ];
                    if matches.iter().any(|(_, value)| value.is_some()) {
                        println!("  匹配条件：");
                        for (label, value) in matches {
                            if let Some(value) = value {
                                println!("    {label}：{value}");
                            }
                        }
                    }
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
        cwd: args.cwd,
    }
}

fn config_command(path: Option<&Path>, command: ConfigCommand) -> app::Result<i32> {
    // `config set` may bootstrap a missing explicit config; list/get read one.
    let mode = match command {
        ConfigCommand::Set { .. } => ConfigLoadMode::AllowCreate,
        _ => ConfigLoadMode::Strict,
    };
    let (paths, config) = load_config_with_mode(path, mode)?;
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
    shell: ToolStatus,
    shell_kind: String,
    shell_integration: DoctorShellIntegration,
    accounts: Vec<DoctorAccount>,
    repository: Option<String>,
    profile: Option<String>,
    credential_available: Option<bool>,
    ssh_agent: Option<DoctorAgent>,
    signing_transport: Option<String>,
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
    repair_command: String,
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
    redacted: bool,
    check_github_signing_key: bool,
) -> app::Result<i32> {
    let ctx = context(path, explicit, std::env::current_dir()?)?;
    let mut warnings = ctx.warnings.clone();
    let unknown_keys = diagnostics::unknown_config_keys_report(&ctx.paths.config_file);
    if !unknown_keys.is_empty() {
        warnings.push(format!(
            "配置文件包含 ghis 未识别的键（已保留，不会影响执行）：{}",
            unknown_keys.join("、")
        ));
    }
    let shell_integration = doctor_shell_integration(&ctx)?;
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

    let forwarded_signing = ctx.profile.as_ref().is_some_and(|profile| {
        profile.signing.enabled
            && matches!(profile.signing.transport, SigningTransport::ForwardedAgent)
    });
    let socket = if forwarded_signing {
        signing::forwarded_agent_socket()
    } else {
        signing::discover_agent_socket(
            ctx.profile
                .as_ref()
                .and_then(|profile| profile.ssh.as_ref())
                .and_then(|ssh| ssh.agent_socket.as_deref()),
        )
    };
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
        let fingerprint_only = ctx.profile.as_ref().is_some_and(|profile| {
            profile.signing.signing_key.is_none() && profile.signing.fingerprint.is_some()
        });
        if fingerprint_only {
            (None, None)
        } else {
            match doctor_local_signing_key(ctx.profile.as_ref().expect("signing profile exists")) {
                Ok(key) => (Some(key), None),
                Err(error) => {
                    warnings.push(error.clone());
                    (None, Some(error))
                }
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
                let agent_error = agent
                    .error
                    .as_deref()
                    .map(diagnostics::sanitize_external_output);
                if agent_required && !agent.available {
                    warnings.push(format!(
                        "SSH Agent 不可用：{}",
                        agent_error.as_deref().unwrap_or("未知错误")
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
                    source: if forwarded_signing {
                        "forwarded"
                    } else {
                        match agent.source {
                            signing::AgentSource::OnePassword => "1password",
                            signing::AgentSource::System => "system",
                            signing::AgentSource::Unknown => "unknown",
                        }
                    },
                    available: agent.available,
                    key_count: agent.keys.len(),
                    selected_key_fingerprint: selected_key.and_then(|key| key.fingerprint),
                    error: agent_error,
                })
            }
            Err(error) => {
                let error = diagnostics::sanitize_external_output(&error.to_string());
                if agent_required {
                    warnings.push(format!("SSH Agent 检查失败：{error}"));
                }
                Some(DoctorAgent {
                    socket: socket.display().to_string(),
                    source: if forwarded_signing {
                        "forwarded"
                    } else if signing::is_onepassword_socket(&socket) {
                        "1password"
                    } else {
                        "unknown"
                    },
                    available: false,
                    key_count: 0,
                    selected_key_fingerprint: None,
                    error: Some(error),
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

    let program = ctx.profile.as_ref().and_then(|profile| {
        if matches!(profile.signing.transport, SigningTransport::ForwardedAgent) {
            profile
                .signing
                .program
                .as_deref()
                .map(|path| signing::SigningProgram {
                    path: signing::expand_user(path),
                    onepassword: false,
                })
        } else {
            signing::discover_signing_program(profile.signing.program.as_deref())
        }
    });
    let signing_program = program.map(|program| DoctorSigningProgram {
        available: signing::signing_program_available(&program),
        path: program.path.display().to_string(),
        onepassword: program.onepassword,
    });
    if signing_required
        && (ctx.profile.as_ref().is_some_and(|profile| {
            matches!(profile.signing.transport, SigningTransport::LocalAgent)
        }) && signing_program.is_none()
            || signing_program
                .as_ref()
                .is_some_and(|program| !program.available))
    {
        warnings.push("已启用 SSH commit signing，但签名程序不可执行".into());
    }
    let signing_transport = ctx.profile.as_ref().map(|profile| {
        match profile.signing.transport {
            SigningTransport::LocalAgent => "local-agent",
            SigningTransport::ForwardedAgent => "forwarded-agent",
        }
        .to_owned()
    });

    let github_signing_key = doctor_github_signing_key(
        check_github_signing_key,
        ctx.profile.as_ref(),
        local_signing_key.as_ref(),
        local_signing_key_error.as_deref(),
        &mut warnings,
    );
    let git_status = tool_status("git", &["--version"]);
    let gh_status = tool_status("gh", &["--version"]);
    let shell_kind = resolve_shell(None)?;
    let shell_kind_name = shell_kind.to_string();
    let shell_status = shell_tool_status(shell_kind);
    let checks = build_doctor_checks(
        &ctx,
        &shell_integration,
        &git_status,
        &gh_status,
        &shell_status,
        &shell_kind_name,
        credential_available,
        ssh_agent.as_ref(),
        &git_config,
    );
    let repairs = collect_repairs(&checks, &git_config);
    let report = DoctorReport {
        schema_version: ghis::SCHEMA_VERSION,
        git: git_status,
        gh: gh_status,
        shell: shell_status,
        shell_kind: shell_kind_name,
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
        signing_transport,
        signing_program,
        github_signing_key,
        git_config,
        checks,
        repairs,
        warnings,
    };
    if redacted {
        print_redacted_json(&report)?;
    } else if json {
        print_json(&report)?;
    } else {
        println!("Git: {}", tool_text(&report.git));
        println!("gh: {}", tool_text(&report.gh));
        println!("{}: {}", report.shell_kind, tool_text(&report.shell));
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
        if let Some(transport) = &report.signing_transport {
            println!("SSH 签名传输: {transport}");
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
    shell_tool: &ToolStatus,
    shell_kind: &str,
    credential_available: Option<bool>,
    ssh_agent: Option<&DoctorAgent>,
    git_config: &diagnostics::GitConfigReport,
) -> Vec<diagnostics::DiagnosticCheck> {
    use diagnostics::{DiagnosticCheck, DiagnosticCode, RepairAction, Severity};
    let mut checks = Vec::new();
    for (name, status) in [("git", git), ("gh", gh), (shell_kind, shell_tool)] {
        checks.push(DiagnosticCheck::new(
            match name {
                "gh" => DiagnosticCode::GhAuth,
                _ if name == shell_kind => DiagnosticCode::ShellIntegration,
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
        (!shell.wrapper_loaded).then(|| RepairAction::confirmation(shell.repair_command.clone())),
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

#[derive(Debug, Clone, Serialize)]
struct CheckOperationSummary {
    target: String,
    command: String,
    argument_count: usize,
    sensitive_arguments_redacted: bool,
}

fn summarize_check_operation(operation: CheckOperation, args: &[String]) -> CheckOperationSummary {
    let command = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(|arg| match arg.as_str() {
            "status" | "diff" | "log" | "show" | "fetch" | "pull" | "push" | "commit"
            | "branch" | "remote" | "config" | "auth" | "api" | "repo" => arg.clone(),
            _ => "unrecognized".to_owned(),
        })
        .unwrap_or_else(|| "default".to_owned());
    let sensitive_arguments_redacted = args.iter().any(|arg| {
        let lower = arg.to_ascii_lowercase();
        lower.contains("token")
            || lower.contains("authorization")
            || lower.contains("password")
            || lower.contains("://")
            || lower.contains('@')
            || lower.starts_with("-") && (lower.contains("body") || lower.contains("message"))
    });
    CheckOperationSummary {
        target: match operation {
            CheckOperation::Git => "git".to_owned(),
            CheckOperation::Gh => "gh".to_owned(),
        },
        command,
        argument_count: args.len(),
        sensitive_arguments_redacted,
    }
}

#[derive(Serialize)]
struct CheckReport {
    schema_version: u32,
    operation: CheckOperation,
    operation_summary: CheckOperationSummary,
    git_config: diagnostics::GitConfigReport,
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
    let shell = doctor_shell_integration(&ctx)?;
    let shell_kind = resolve_shell(None)?;
    let shell_kind_name = shell_kind.to_string();
    let checks = build_doctor_checks(
        &ctx,
        &shell,
        &tool_status("git", &["--version"]),
        &tool_status("gh", &["--version"]),
        &shell_tool_status(shell_kind),
        &shell_kind_name,
        None,
        None,
        &git_config,
    );
    let repairs = collect_repairs(&checks, &git_config);
    let report = CheckReport {
        schema_version: ghis::SCHEMA_VERSION,
        operation: args.operation,
        operation_summary: summarize_check_operation(args.operation, &args.args),
        git_config,
        checks,
        repairs,
    };
    if args.redacted {
        print_redacted_json(&report)?;
    } else if args.json {
        print_json(&report)?;
    } else {
        println!(
            "操作\t{} {}\t参数 {} 个{}",
            report.operation_summary.target,
            report.operation_summary.command,
            report.operation_summary.argument_count,
            if report.operation_summary.sensitive_arguments_redacted {
                "（敏感参数已隐藏）"
            } else {
                ""
            }
        );
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

fn doctor_shell_integration(ctx: &AppContext) -> app::Result<DoctorShellIntegration> {
    let (renderer, health, wrapper_loaded) = match shell::active_renderer() {
        shell::ActiveRenderer::Matched(renderer) | shell::ActiveRenderer::Fallback(renderer) => {
            let health = renderer.integration_health();
            (renderer, health, renderer.integration_is_loaded())
        }
        // Two renderers claiming one marker cannot safely describe the active
        // shell. Keep zsh's established installation fallback, but never trust
        // the ambiguous marker as evidence that a wrapper is healthy.
        shell::ActiveRenderer::Ambiguous => {
            let renderer = shell::renderer(shell::ShellKind::Zsh)
                .map_err(|error| app::AppError::Message(error.to_string()))?;
            (renderer, shell::IntegrationHealth::NotLoaded, false)
        }
    };
    let kind = renderer.spec().kind();
    let wrapper_healthy = health == shell::IntegrationHealth::Healthy;
    let health_marker = renderer
        .inherited_health_marker()
        .map(|value| diagnostics::sanitize_display_text(&value.to_string_lossy()));
    let init_file = ctx.paths.config_dir.join(renderer.spec().init_file_name());
    let setup_installed = platform::user_home()
        .ok()
        .map(|home| {
            let startup_file = renderer.startup_file(&home);
            renderer.integration_is_installed(&startup_file, &init_file, "ghis")
        })
        .unwrap_or(false);
    let repository_bound = ctx
        .repository
        .as_ref()
        .is_some_and(repository_has_ghis_persistence);
    let state = classify_shell_integration(health, setup_installed, repository_bound);
    Ok(DoctorShellIntegration {
        state,
        wrapper_loaded,
        wrapper_healthy,
        health_marker,
        setup_installed,
        repository_bound,
        advice: doctor_shell_integration_advice(state, kind),
        repair_command: renderer.setup_command().into(),
    })
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
        _ => DoctorShellIntegrationState::NotIntegrated,
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

fn doctor_shell_integration_advice(
    state: DoctorShellIntegrationState,
    kind: shell::ShellKind,
) -> String {
    match state {
        DoctorShellIntegrationState::WrapperLoaded => {
            "普通 git/gh 会经过 ghis；command git、绝对路径或 GHIS_BYPASS=1 仍可明确绕过 wrapper"
                .into()
        }
        DoctorShellIntegrationState::WrapperIncomplete => format!(
            "GHIS marker 存在，但函数依赖不完整；运行 `ghis shell setup {kind}` 或重新启动 {kind}"
        ),
        DoctorShellIntegrationState::InstalledNotLoaded => {
            format!("运行 `exec {kind}` 或新开终端后再试；ghis 不会替换当前 shell")
        }
        DoctorShellIntegrationState::RepositoryOnly => {
            "普通 Git 仍会读取该仓库已有的 include/helper/hook，但没有 wrapper 的本次解析和注入"
                .into()
        }
        DoctorShellIntegrationState::NotIntegrated => {
            format!(
                "普通 git/gh 不会经过 ghis；需要时先运行 `ghis shell setup {kind}` 并重新加载 {kind}"
            )
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
        signing_public_key.map(str::to_owned)
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
    let fingerprint = if profile.signing.enabled {
        profile.signing.fingerprint.clone()
    } else {
        ssh.and_then(|ssh| ssh.fingerprint.clone())
    };
    Some(signing::SigningProfile {
        enabled: key_required,
        transport: profile.signing.transport,
        public_key,
        fingerprint,
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

fn shell_tool_status(kind: shell::ShellKind) -> ToolStatus {
    match kind {
        shell::ShellKind::PowerShell => {
            let status = tool_status("pwsh", &["--version"]);
            if status.available {
                status
            } else {
                tool_status("powershell", &["--version"])
            }
        }
        shell::ShellKind::Zsh => tool_status("zsh", &["--version"]),
        shell::ShellKind::Bash => tool_status("bash", &["--version"]),
        shell::ShellKind::Fish => tool_status("fish", &["--version"]),
        _ => ToolStatus {
            available: false,
            version: None,
        },
    }
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

/// Preserve the established Unix zsh default while using native PowerShell on Windows.
fn resolve_shell(explicit: Option<shell::ShellKind>) -> app::Result<shell::ShellKind> {
    #[cfg(windows)]
    let default = shell::ShellKind::PowerShell;
    #[cfg(not(windows))]
    let default = shell::ShellKind::Zsh;
    Ok(explicit.unwrap_or(default))
}

/// Resolve a shell startup-file root without falling back to the current
/// directory. This keeps PowerShell's Windows profile target independent from
/// Unix-only `HOME` while retaining the established Unix behavior.
fn shell_home() -> app::Result<PathBuf> {
    platform::user_home()
        .map_err(|error| app::AppError::Message(format!("无法解析用户目录：{error}")))
}

fn completion(kind: shell::ShellKind) -> app::Result<i32> {
    let renderer =
        shell::renderer(kind).map_err(|error| app::AppError::Message(error.to_string()))?;
    let mut command = Cli::command();
    let output = renderer.render_completion(&mut command, "ghis");
    write_stdout(&output)?;
    Ok(0)
}

fn shell_command(command: ShellCommand) -> app::Result<i32> {
    match command {
        ShellCommand::Setup(args) => setup(args),
        ShellCommand::Uninstall(args) => uninstall(args),
        ShellCommand::Init(args) => {
            print!(
                "{}",
                shell::render_init(resolve_shell(args.shell)?, "ghis")
                    .map_err(|error| app::AppError::Message(error.to_string()))?
            );
            Ok(0)
        }
        ShellCommand::Completion(args) => completion(resolve_shell(args.shell)?),
    }
}

fn teardown(path: Option<&Path>, args: TeardownArgs) -> app::Result<i32> {
    if args.purge && !args.dry_run && !args.yes {
        if !io::stdin().is_terminal() {
            return Err(app::AppError::Message(
                "非交互环境执行完整清理时必须使用 `ghis teardown --purge --yes`".into(),
            ));
        }
        eprint!("将删除当前 ghis 配置、缓存和状态，继续吗？[y/N] ");
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("已取消，未修改任何文件。");
            return Ok(0);
        }
    }

    let mut paths = ConfigPaths::discover()?;
    if let Some(path) = path {
        paths.set_config_file(path)?;
    }
    let kind = resolve_shell(args.shell.shell)?;
    let renderer =
        shell::renderer(kind).map_err(|error| app::AppError::Message(error.to_string()))?;
    let home = shell_home()?;
    let startup_file = renderer.startup_file(&home);
    let init_file = paths.config_dir.join(renderer.spec().init_file_name());
    let cwd = args.repo.unwrap_or(std::env::current_dir()?);
    let ctx = context(path, None, &cwd)?;
    let project_root = ctx
        .repository
        .as_ref()
        .map(|repository| repository.command_dir().to_path_buf())
        .unwrap_or_else(|| cwd.clone());
    let user_settings = claude_settings_path(AgentScope::User, None)?;
    let project_settings = claude_settings_path(AgentScope::Project, Some(project_root))?;

    if args.dry_run {
        println!("将检查并撤销以下 ghis 管理的集成：");
        println!("  Shell ({kind})：{}", startup_file.display());
        println!("  Shell 初始化文件：{}", init_file.display());
        println!("  Claude Code 用户 Hook：{}", user_settings.display());
        println!("  Claude Code 项目 Hook：{}", project_settings.display());
        if let Some(repository) = &ctx.repository {
            println!("  仓库绑定：{}", repository.command_dir().display());
        } else {
            println!("  仓库绑定：跳过（{} 不是 Git 仓库）", cwd.display());
        }
        if args.purge {
            println!("将删除当前配置 namespace 中的用户数据：");
            for target in purge_targets(&paths) {
                println!("  {}", target.display());
            }
            println!(
                "  {} 中由 ghis 生成的配置片段",
                paths.fragments_dir.display()
            );
        } else {
            println!("将保留 Profile、规则、配置、缓存和状态；使用 --purge 才会删除。")
        }
        return Ok(0);
    }

    let shell_changed = renderer.uninstall(&startup_file)?;
    let init_removed = remove_file_if_exists(&init_file)?;
    println!(
        "Shell ({kind})：{}",
        if shell_changed || init_removed {
            "已撤销"
        } else {
            "未发现"
        }
    );

    for (label, settings) in [("用户", user_settings), ("项目", project_settings)] {
        let changed = ghis::agent::claude::uninstall_settings(&settings)
            .map_err(|error| app::AppError::Message(error.to_string()))?;
        println!(
            "Claude Code {label} Hook：{}（{}）",
            if changed { "已移除" } else { "未发现" },
            settings.display()
        );
    }

    if ctx.repository.is_some() {
        app::unbind_repository(&ctx)?;
        println!("仓库集成：已撤销（{}）", cwd.display());
    } else {
        println!("仓库集成：跳过（{} 不是 Git 仓库）", cwd.display());
    }

    if args.purge {
        purge_user_data(&paths)?;
        println!("用户数据：已清理当前配置 namespace。");
    } else {
        println!("用户数据：已保留。");
    }
    println!("ghis 程序本体仍由原安装器或包管理器管理。");
    Ok(0)
}

fn purge_targets(paths: &ConfigPaths) -> Vec<PathBuf> {
    vec![
        paths.config_file.clone(),
        config_lock_path(&paths.config_file),
        paths.cache_dir.join(github::DISCOVERY_CACHE_FILENAME),
        paths.repositories_file.clone(),
        config_lock_path(&paths.repositories_file),
        paths.log_file.clone(),
        config_lock_path(&paths.log_file),
    ]
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

fn remove_file_if_exists(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn purge_user_data(paths: &ConfigPaths) -> app::Result<()> {
    for target in purge_targets(paths) {
        remove_file_if_exists(&target)?;
    }
    match fs::read_dir(&paths.fragments_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name();
                let entry_path = entry.path();
                let extension = entry_path.extension().and_then(|value| value.to_str());
                if name == ".lock" || matches!(extension, Some("gitconfig" | "sshconfig")) {
                    let file_type = entry.file_type()?;
                    if file_type.is_file() || file_type.is_symlink() {
                        remove_file_if_exists(&entry_path)?;
                    }
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    for directory in [&paths.fragments_dir, &paths.cache_dir, &paths.state_dir] {
        match fs::remove_dir(directory) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn setup(args: SetupArgs) -> app::Result<i32> {
    let kind = resolve_shell(args.shell.shell)?;
    let renderer =
        shell::renderer(kind).map_err(|error| app::AppError::Message(error.to_string()))?;
    let init_script = renderer.render_init("ghis");
    let paths = ConfigPaths::discover()?;
    let init = paths.config_dir.join(renderer.spec().init_file_name());
    if args.print {
        print!("{init_script}");
        return Ok(0);
    }
    let home = shell_home()?;
    let startup_file = renderer.startup_file(&home);
    if !args.yes {
        if !io::stdin().is_terminal() {
            return Err(app::AppError::Message(format!(
                "非交互环境不会自动修改 {}；确认目标后重新运行 `ghis shell setup --yes`",
                startup_file.display()
            )));
        }
        eprint!("将备份并更新 {}，继续吗？[y/N] ", startup_file.display());
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("已取消，未修改任何文件。");
            return Ok(0);
        }
    }
    let report = renderer.install(&startup_file, &init, &init_script)?;
    println!(
        "{kind} 集成{}：{}",
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
    if renderer.integration_is_loaded() {
        println!("当前 {kind} 已加载 ghis wrapper。");
    } else {
        println!(
            "当前 {kind} 尚未加载；运行 `exec {kind}` 或新开终端后生效，ghis 不会自动替换 shell。"
        );
    }
    Ok(0)
}

fn uninstall(args: ShellArgs) -> app::Result<i32> {
    let kind = resolve_shell(args.shell)?;
    let renderer =
        shell::renderer(kind).map_err(|error| app::AppError::Message(error.to_string()))?;
    let home = shell_home()?;
    let startup_file = renderer.startup_file(&home);
    let changed = renderer.uninstall(&startup_file)?;
    if changed {
        println!(
            "已从 {} 移除 ghis 管理的 {kind} 集成。",
            startup_file.display()
        );
    } else {
        println!(
            "未在 {} 发现 ghis 管理的 {kind} 集成。",
            startup_file.display()
        );
    }
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
        if std::env::var_os("GHIS_BANNER_SHOWN").as_deref() != Some(std::ffi::OsStr::new("1"))
            && io::stderr().is_terminal()
        {
            eprintln!("{}", ctx.operation_banner());
        }
        if let Some(warning) = ctx.hook_identity_warning() {
            eprintln!("{warning}");
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

fn print_redacted_json<T: Serialize>(value: &T) -> app::Result<()> {
    let mut value =
        serde_json::to_value(value).map_err(|error| app::AppError::Message(error.to_string()))?;
    diagnostics::redact_json_value(&mut value);
    print_json(&value)
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
    fn cli_short_options_are_conflict_free() {
        Cli::command().debug_assert();
    }

    #[test]
    fn shell_commands_share_optional_shell_arguments() {
        for command in ["setup", "uninstall", "init", "completion"] {
            let parsed = Cli::try_parse_from(["ghis", "shell", command, "zsh"])
                .unwrap_or_else(|error| panic!("shell {command} should accept zsh: {error}"));
            assert!(matches!(
                parsed.command,
                Some(Commands::Shell {
                    command: ShellCommand::Setup(SetupArgs {
                        shell: ShellArgs {
                            shell: Some(shell::ShellKind::Zsh)
                        },
                        ..
                    })
                }) | Some(Commands::Shell {
                    command: ShellCommand::Uninstall(ShellArgs {
                        shell: Some(shell::ShellKind::Zsh)
                    })
                }) | Some(Commands::Shell {
                    command: ShellCommand::Init(ShellArgs {
                        shell: Some(shell::ShellKind::Zsh)
                    })
                }) | Some(Commands::Shell {
                    command: ShellCommand::Completion(ShellArgs {
                        shell: Some(shell::ShellKind::Zsh)
                    })
                })
            ));
        }
    }

    #[test]
    fn legacy_top_level_shell_commands_are_rejected() {
        for command in ["setup", "uninstall", "init", "completion"] {
            Cli::try_parse_from(["ghis", command]).unwrap_err();
        }
    }

    #[cfg(windows)]
    #[test]
    fn shell_commands_use_powershell_as_the_implicit_default() {
        assert_eq!(resolve_shell(None).unwrap(), shell::ShellKind::PowerShell);
    }

    #[test]
    fn unknown_shell_is_rejected_during_cli_parsing() {
        let error = Cli::try_parse_from(["ghis", "shell", "init", "nu"])
            .expect_err("unknown shell must not parse");
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
        assert!(error.to_string().contains("未知 shell `nu`"));
    }

    #[test]
    fn profile_list_details_conflicts_with_json() {
        let error = Cli::try_parse_from(["ghis", "profile", "list", "-d", "-j"])
            .expect_err("details and json must conflict");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn profile_short_options_parse_in_their_subcommands() {
        Cli::try_parse_from([
            "ghis",
            "profile",
            "add",
            "work",
            "-H",
            "github.example.test",
            "-l",
            "alice",
            "-n",
            "Alice",
            "-e",
            "alice@example.test",
            "-d",
            "Work",
        ])
        .expect("profile add short options");
        Cli::try_parse_from(["ghis", "profile", "list", "-d"])
            .expect("profile list details short option");
    }

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

    #[cfg(windows)]
    #[test]
    fn native_shim_parser_preserves_arbitrary_windows_os_arguments() {
        use std::os::windows::ffi::OsStringExt;

        let non_unicode = std::ffi::OsString::from_wide(&[b'a' as u16, 0xd800, b'b' as u16]);
        let expected = vec![
            std::ffi::OsString::new(),
            std::ffi::OsString::from("space quote \" & | ^ % trailing\\"),
            non_unicode,
        ];
        let cli = Cli::try_parse_from(
            [
                std::ffi::OsString::from("ghis"),
                std::ffi::OsString::from("git"),
                std::ffi::OsString::from("--"),
            ]
            .into_iter()
            .chain(expected.iter().cloned()),
        )
        .unwrap();
        let Commands::Git(parsed) = cli.command.unwrap() else {
            panic!("expected native git shim dispatch");
        };
        assert_eq!(parsed.args, expected);
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
