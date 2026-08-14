//! 中文局部动态 onboarding。
//!
//! 交互只重绘自身占用的终端行，不进入 alternate screen，也不接管
//! 终端中其他内容。业务写入由二进制入口在最终确认后执行。

use crate::config::Config;
use crate::github::{EmailCandidate, GhAccount};
use crossterm::cursor::{Hide, MoveDown, MoveToColumn, MoveUp, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const TOTAL_STEPS: usize = 4;
const MIN_WIDTH: usize = 24;
const MAX_WIDTH: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub id: String,
    pub host: String,
    pub login: String,
    pub git_name: String,
    pub git_email: String,
    pub description: Option<String>,
    pub make_default: bool,
    pub bind_repository: bool,
    pub setup_zsh: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowResult<T> {
    Complete(T),
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Account,
    Identity,
    Options,
    Review,
}

impl Step {
    fn number(self) -> usize {
        match self {
            Self::Account => 1,
            Self::Identity => 2,
            Self::Options => 3,
            Self::Review => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccountMode {
    Menu,
    Host,
    Login,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentityField {
    ProfileId,
    GitName,
    EmailMenu,
    ManualEmail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKey {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Escape,
    Backspace,
    Space,
    Character(char),
    Cancel,
}

struct TerminalSession;

impl TerminalSession {
    fn start(writer: &mut impl Write) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        if let Err(error) = execute!(writer, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stderr(), Show, MoveToColumn(0));
    }
}

#[derive(Debug)]
struct Renderer<W> {
    writer: W,
    previous_lines: usize,
    width: usize,
    color: bool,
}

impl<W: Write> Renderer<W> {
    fn new(writer: W, width: usize, color: bool) -> Self {
        Self {
            writer,
            previous_lines: 0,
            width: width.clamp(MIN_WIDTH, MAX_WIDTH),
            color,
        }
    }

    fn render(&mut self, lines: &[String]) -> io::Result<()> {
        self.clear()?;
        let rendered = layout_lines(lines, self.width);
        for line in &rendered {
            queue!(self.writer, Clear(ClearType::CurrentLine))?;
            self.writer.write_all(paint(line, self.color).as_bytes())?;
            self.writer.write_all(b"\r\n")?;
        }
        self.writer.flush()?;
        self.previous_lines = rendered.len();
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        if self.previous_lines > 0 {
            let line_count = self.previous_lines.min(u16::MAX as usize) as u16;
            queue!(self.writer, MoveUp(line_count), MoveToColumn(0))?;
            for index in 0..line_count {
                queue!(self.writer, Clear(ClearType::CurrentLine))?;
                if index + 1 < line_count {
                    queue!(self.writer, MoveDown(1), MoveToColumn(0))?;
                }
            }
            if line_count > 1 {
                queue!(self.writer, MoveUp(line_count - 1), MoveToColumn(0))?;
            }
            self.writer.flush()?;
            self.previous_lines = 0;
        }
        Ok(())
    }

    fn finish(&mut self, lines: &[String]) -> io::Result<()> {
        self.clear()?;
        for line in layout_lines(lines, self.width) {
            self.writer.write_all(paint(&line, self.color).as_bytes())?;
            self.writer.write_all(b"\r\n")?;
        }
        self.writer.flush()
    }
}

#[derive(Debug)]
struct Wizard<'a> {
    config: &'a Config,
    accounts: Vec<GhAccount>,
    repository: Option<PathBuf>,
    discovery_notice: Option<String>,
    step: Step,
    account_mode: AccountMode,
    account_selected: usize,
    host_input: String,
    login_input: String,
    host: String,
    login: String,
    identity_field: IdentityField,
    profile_id: String,
    git_name: String,
    email: String,
    email_candidates: Vec<EmailCandidate>,
    email_selected: usize,
    email_error: Option<String>,
    option_selected: usize,
    make_default: bool,
    bind_repository: bool,
    setup_zsh: bool,
    review_selected: usize,
    error: Option<String>,
}

impl<'a> Wizard<'a> {
    fn new(
        config: &'a Config,
        accounts: &[GhAccount],
        discovery_notice: Option<String>,
        repository: Option<&Path>,
    ) -> Self {
        let account_selected = accounts
            .iter()
            .position(|account| account.active)
            .unwrap_or(0)
            .min(accounts.len());
        Self {
            config,
            accounts: accounts.to_vec(),
            repository: repository.map(Path::to_path_buf),
            discovery_notice,
            step: Step::Account,
            account_mode: AccountMode::Menu,
            account_selected,
            host_input: "github.com".into(),
            login_input: String::new(),
            host: String::new(),
            login: String::new(),
            identity_field: IdentityField::ProfileId,
            profile_id: String::new(),
            git_name: String::new(),
            email: String::new(),
            email_candidates: Vec::new(),
            email_selected: 0,
            email_error: None,
            option_selected: 0,
            make_default: config.behavior.default_profile.is_none(),
            bind_repository: false,
            setup_zsh: false,
            review_selected: 0,
            error: None,
        }
    }

    fn frame(&self, width: usize) -> Vec<String> {
        let mut lines = vec![
            header(self.step, width.saturating_sub(1).max(1)),
            String::new(),
        ];
        match self.step {
            Step::Account => self.render_account(&mut lines),
            Step::Identity => self.render_identity(&mut lines),
            Step::Options => self.render_options(&mut lines),
            Step::Review => self.render_review(&mut lines),
        }
        if let Some(error) = self.error.as_deref() {
            lines.push(String::new());
            lines.push(format!("! {error}"));
        }
        lines
    }

    fn render_account(&self, lines: &mut Vec<String>) {
        lines.push("配置范围".into());
        lines.push(format!(
            "  仓库    {}",
            self.repository
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "不绑定仓库".into())
        ));
        lines.push(String::new());
        match self.account_mode {
            AccountMode::Menu => {
                lines.push("选择 GitHub 账号".into());
                lines.push(String::new());
                for (index, account) in self.accounts.iter().enumerate() {
                    let marker = if index == self.account_selected {
                        ">"
                    } else {
                        " "
                    };
                    let active = if account.active {
                        "    当前账号"
                    } else {
                        ""
                    };
                    let verified = if account.verified {
                        ""
                    } else {
                        "    未验证"
                    };
                    lines.push(format!(
                        "  {marker} {}@{}{}{}",
                        account.login, account.host, active, verified
                    ));
                }
                let manual = self.accounts.len();
                let marker = if self.account_selected == manual {
                    ">"
                } else {
                    " "
                };
                lines.push(format!("  {marker} 手动输入账号"));
                if let Some(notice) = self.discovery_notice.as_deref() {
                    lines.push(String::new());
                    lines.push(format!("! {notice}"));
                }
                lines.push(String::new());
                lines.push("↑/k 上移  ↓/j 下移  h 返回  l/Enter 确认  q 取消".into());
            }
            AccountMode::Host => {
                lines.push("手动输入 GitHub host".into());
                lines.push(String::new());
                lines.push(format!("> {}", self.host_input));
                lines.push(String::new());
                lines.push("Enter 继续  Esc 返回  Ctrl+C 取消".into());
            }
            AccountMode::Login => {
                lines.push("手动输入 GitHub login".into());
                lines.push(String::new());
                lines.push(format!("> {}", self.login_input));
                lines.push(String::new());
                lines.push("Enter 继续  Esc 返回  Ctrl+C 取消".into());
            }
        }
    }

    fn render_identity(&self, lines: &mut Vec<String>) {
        lines.push("当前选择".into());
        lines.push(format!("  账号    {}@{}", self.login, self.host));
        if matches!(
            self.identity_field,
            IdentityField::EmailMenu | IdentityField::ManualEmail
        ) && !self.git_name.is_empty()
        {
            lines.push(format!("  姓名    {}", self.git_name));
        }
        lines.push(String::new());
        match self.identity_field {
            IdentityField::ProfileId => {
                lines.push("设置 Profile ID".into());
                lines.push(String::new());
                lines.push(format!("> {}", self.profile_id));
                lines.push(String::new());
                lines.push("Enter 继续  Esc 返回  Ctrl+C 取消".into());
            }
            IdentityField::GitName => {
                lines.push("设置提交姓名".into());
                lines.push(String::new());
                lines.push(format!("> {}", self.git_name));
                lines.push(String::new());
                lines.push("Enter 继续  Esc 返回  Ctrl+C 取消".into());
            }
            IdentityField::EmailMenu => {
                lines.push("选择提交邮箱".into());
                lines.push(String::new());
                let email_width = self
                    .email_candidates
                    .iter()
                    .map(|candidate| UnicodeWidthStr::width(candidate.email.as_str()))
                    .max()
                    .unwrap_or(0);
                for (index, candidate) in self.email_candidates.iter().enumerate() {
                    let marker = if index == self.email_selected {
                        ">"
                    } else {
                        " "
                    };
                    let label = if candidate.noreply {
                        "[GitHub noreply]"
                    } else if candidate.primary {
                        "[GitHub 主邮箱]"
                    } else if candidate.verified {
                        "[GitHub 已验证邮箱]"
                    } else {
                        "[GitHub 账号邮箱]"
                    };
                    lines.push(format!(
                        "  {marker} {}  {label}",
                        pad_columns(&candidate.email, email_width)
                    ));
                }
                let manual = self.email_candidates.len();
                let marker = if self.email_selected == manual {
                    ">"
                } else {
                    " "
                };
                lines.push(format!("  {marker} 手动输入其他邮箱"));
                if self.email_error.is_some() {
                    let retry = manual + 1;
                    let marker = if self.email_selected == retry {
                        ">"
                    } else {
                        " "
                    };
                    lines.push(format!("  {marker} 重新获取邮箱候选"));
                }
                if let Some(error) = self.email_error.as_deref() {
                    lines.push(String::new());
                    lines.push(format!("! 获取邮箱候选失败：{error}"));
                }
                lines.push(String::new());
                lines.push("↑/k 上移  ↓/j 下移  h 返回  l/Enter 确认  q 取消".into());
            }
            IdentityField::ManualEmail => {
                lines.push("手动输入提交邮箱".into());
                lines.push(String::new());
                lines.push(format!("> {}", self.email));
                lines.push(String::new());
                lines.push("Enter 继续  Esc 返回  Ctrl+C 取消".into());
            }
        }
    }

    fn render_options(&self, lines: &mut Vec<String>) {
        lines.push("当前配置".into());
        lines.push(format!("  Profile    {}", self.profile_id));
        lines.push(format!("  GitHub     {}@{}", self.login, self.host));
        lines.push(format!("  身份        {} <{}>", self.git_name, self.email));
        lines.push(String::new());
        lines.push("附加设置".into());
        lines.push(String::new());
        let options = [
            ("设为默认 Profile", self.make_default),
            ("绑定当前 Git worktree", self.bind_repository),
            ("安装 zsh wrapper", self.setup_zsh),
        ];
        for (index, (label, enabled)) in options.into_iter().enumerate() {
            let marker = if index == self.option_selected {
                ">"
            } else {
                " "
            };
            let checkbox = if enabled { "[x]" } else { "[ ]" };
            let suffix = if index == 1 && self.repository.is_none() {
                "（当前不可用）"
            } else {
                ""
            };
            lines.push(format!("  {marker} {checkbox} {label}{suffix}"));
        }
        lines.push(String::new());
        lines.push("j/k 移动  Space 切换  h 返回  l/Enter 继续  q 取消".into());
    }

    fn render_review(&self, lines: &mut Vec<String>) {
        lines.push("确认配置".into());
        lines.push(String::new());
        lines.push(format!("  Profile      {}", self.profile_id));
        lines.push(format!("  GitHub       {}@{}", self.login, self.host));
        lines.push(format!("  提交姓名      {}", self.git_name));
        lines.push(format!("  提交邮箱      {}", self.email));
        lines.push(format!("  默认 Profile  {}", yes_no(self.make_default)));
        lines.push(format!("  绑定仓库      {}", yes_no(self.bind_repository)));
        lines.push(format!("  zsh wrapper  {}", yes_no(self.setup_zsh)));
        lines.push(String::new());
        for (index, label) in ["应用配置", "返回修改", "取消"].into_iter().enumerate() {
            let marker = if index == self.review_selected {
                ">"
            } else {
                " "
            };
            lines.push(format!("  {marker} {label}"));
        }
        lines.push(String::new());
        lines.push("↑/k 上移  ↓/j 下移  h 返回  l/Enter 确认".into());
    }

    fn is_text_editing(&self) -> bool {
        matches!(
            (self.step, self.account_mode, self.identity_field),
            (Step::Account, AccountMode::Host | AccountMode::Login, _)
                | (
                    Step::Identity,
                    _,
                    IdentityField::ProfileId | IdentityField::GitName | IdentityField::ManualEmail
                )
        )
    }

    fn handle_key<F>(
        &mut self,
        key: InputKey,
        candidate_loader: &mut F,
    ) -> Option<FlowResult<Draft>>
    where
        F: FnMut(&str, &str) -> Result<Vec<EmailCandidate>, String>,
    {
        if key == InputKey::Cancel {
            return Some(FlowResult::Cancelled);
        }
        self.error = None;
        if !self.is_text_editing() && key == InputKey::Character('q') {
            return Some(FlowResult::Cancelled);
        }
        match self.step {
            Step::Account => self.handle_account(key, candidate_loader),
            Step::Identity => self.handle_identity(key, candidate_loader),
            Step::Options => self.handle_options(key),
            Step::Review => self.handle_review(key),
        }
    }

    fn handle_account<F>(
        &mut self,
        key: InputKey,
        candidate_loader: &mut F,
    ) -> Option<FlowResult<Draft>>
    where
        F: FnMut(&str, &str) -> Result<Vec<EmailCandidate>, String>,
    {
        match self.account_mode {
            AccountMode::Menu => {
                let count = self.accounts.len() + 1;
                if is_up(key) {
                    self.account_selected = previous(self.account_selected, count);
                } else if is_down(key) {
                    self.account_selected = next(self.account_selected, count);
                } else if is_back(key) {
                    return Some(FlowResult::Cancelled);
                } else if is_forward(key) {
                    if self.account_selected < self.accounts.len() {
                        let account = &self.accounts[self.account_selected];
                        let host = account.host.clone();
                        let login = account.login.clone();
                        self.accept_account(host, login, candidate_loader);
                    } else {
                        self.account_mode = AccountMode::Host;
                    }
                } else if let InputKey::Character(value) = key
                    && let Some(index) = value.to_digit(10).map(|value| value as usize)
                    && (1..=count).contains(&index)
                {
                    self.account_selected = index - 1;
                }
            }
            AccountMode::Host => match key {
                InputKey::Enter => {
                    if self.host_input.trim().is_empty() {
                        self.error = Some("GitHub host 不能为空。".into());
                    } else {
                        self.account_mode = AccountMode::Login;
                    }
                }
                InputKey::Escape => self.account_mode = AccountMode::Menu,
                _ => edit_text(&mut self.host_input, key),
            },
            AccountMode::Login => match key {
                InputKey::Enter => {
                    if self.login_input.trim().is_empty() {
                        self.error = Some("GitHub login 不能为空。".into());
                    } else {
                        let host = self.host_input.trim().to_owned();
                        let login = self.login_input.trim().to_owned();
                        self.accept_account(host, login, candidate_loader);
                    }
                }
                InputKey::Escape => self.account_mode = AccountMode::Host,
                _ => edit_text(&mut self.login_input, key),
            },
        }
        None
    }

    fn accept_account<F>(&mut self, host: String, login: String, candidate_loader: &mut F)
    where
        F: FnMut(&str, &str) -> Result<Vec<EmailCandidate>, String>,
    {
        self.host = crate::github::normalize_host(&host);
        self.login = login.trim().to_owned();
        if self.profile_id.is_empty() {
            self.profile_id = self.login.to_ascii_lowercase();
        }
        self.reload_candidates(candidate_loader);
        self.step = Step::Identity;
        self.identity_field = IdentityField::ProfileId;
    }

    fn reload_candidates<F>(&mut self, candidate_loader: &mut F)
    where
        F: FnMut(&str, &str) -> Result<Vec<EmailCandidate>, String>,
    {
        match candidate_loader(&self.host, &self.login) {
            Ok(candidates) => {
                self.email_candidates = candidates;
                self.email_error = None;
            }
            Err(error) => {
                self.email_candidates.clear();
                self.email_error = Some(error);
            }
        }
        self.email_selected = 0;
    }

    fn handle_identity<F>(
        &mut self,
        key: InputKey,
        candidate_loader: &mut F,
    ) -> Option<FlowResult<Draft>>
    where
        F: FnMut(&str, &str) -> Result<Vec<EmailCandidate>, String>,
    {
        match self.identity_field {
            IdentityField::ProfileId => match key {
                InputKey::Enter => {
                    let id = self.profile_id.trim();
                    if id.is_empty() {
                        self.error = Some("Profile ID 不能为空。".into());
                    } else if self.config.profiles.contains_key(id) {
                        self.error = Some(format!("Profile ID“{id}”已经存在，请输入其他名称。"));
                    } else {
                        self.profile_id = id.to_owned();
                        self.identity_field = IdentityField::GitName;
                    }
                }
                InputKey::Escape => self.step = Step::Account,
                _ => edit_text(&mut self.profile_id, key),
            },
            IdentityField::GitName => match key {
                InputKey::Enter => {
                    if self.git_name.trim().is_empty() {
                        self.error = Some("提交姓名不能为空。".into());
                    } else {
                        self.git_name = self.git_name.trim().to_owned();
                        self.identity_field = IdentityField::EmailMenu;
                    }
                }
                InputKey::Escape => self.identity_field = IdentityField::ProfileId,
                _ => edit_text(&mut self.git_name, key),
            },
            IdentityField::EmailMenu => {
                let count =
                    self.email_candidates.len() + 1 + usize::from(self.email_error.is_some());
                if is_up(key) {
                    self.email_selected = previous(self.email_selected, count);
                } else if is_down(key) {
                    self.email_selected = next(self.email_selected, count);
                } else if is_back(key) {
                    self.identity_field = IdentityField::GitName;
                } else if is_forward(key) {
                    let manual = self.email_candidates.len();
                    if self.email_selected < manual {
                        self.email = self.email_candidates[self.email_selected].email.clone();
                        self.step = Step::Options;
                    } else if self.email_selected == manual {
                        self.identity_field = IdentityField::ManualEmail;
                    } else {
                        self.reload_candidates(candidate_loader);
                    }
                }
            }
            IdentityField::ManualEmail => match key {
                InputKey::Enter => {
                    if self.email.trim().is_empty() || !self.email.contains('@') {
                        self.error = Some("请输入有效的提交邮箱。".into());
                    } else {
                        self.email = self.email.trim().to_owned();
                        self.step = Step::Options;
                    }
                }
                InputKey::Escape => self.identity_field = IdentityField::EmailMenu,
                _ => edit_text(&mut self.email, key),
            },
        }
        None
    }

    fn handle_options(&mut self, key: InputKey) -> Option<FlowResult<Draft>> {
        if is_up(key) {
            self.option_selected = previous(self.option_selected, 3);
        } else if is_down(key) {
            self.option_selected = next(self.option_selected, 3);
        } else if is_back(key) {
            self.step = Step::Identity;
            self.identity_field = IdentityField::EmailMenu;
        } else if key == InputKey::Space {
            match self.option_selected {
                0 => self.make_default = !self.make_default,
                1 if self.repository.is_some() => {
                    self.bind_repository = !self.bind_repository;
                }
                1 => self.error = Some("当前目录不是 Git 仓库，无法绑定 worktree。".into()),
                2 => self.setup_zsh = !self.setup_zsh,
                _ => {}
            }
        } else if is_forward(key) {
            self.step = Step::Review;
            self.review_selected = 0;
        }
        None
    }

    fn handle_review(&mut self, key: InputKey) -> Option<FlowResult<Draft>> {
        if is_up(key) {
            self.review_selected = previous(self.review_selected, 3);
        } else if is_down(key) {
            self.review_selected = next(self.review_selected, 3);
        } else if is_back(key) {
            self.step = Step::Options;
        } else if is_forward(key) {
            match self.review_selected {
                0 => {
                    return Some(FlowResult::Complete(Draft {
                        id: self.profile_id.clone(),
                        host: self.host.clone(),
                        login: self.login.clone(),
                        git_name: self.git_name.clone(),
                        git_email: self.email.clone(),
                        description: None,
                        make_default: self.make_default,
                        bind_repository: self.bind_repository,
                        setup_zsh: self.setup_zsh,
                    }));
                }
                1 => self.step = Step::Options,
                _ => return Some(FlowResult::Cancelled),
            }
        }
        None
    }
}

/// Run the dynamic prompt in the current terminal.
///
/// The caller must verify stdin/stderr are TTYs. If terminal raw mode or cursor
/// control is unavailable, call [`run_line_mode`] instead.
pub fn run_terminal<F>(
    writer: impl Write,
    config: &Config,
    accounts: &[GhAccount],
    discovery_notice: Option<String>,
    repository: Option<&Path>,
    color: bool,
    candidate_loader: F,
) -> io::Result<FlowResult<Draft>>
where
    F: FnMut(&str, &str) -> Result<Vec<EmailCandidate>, String>,
{
    let width = terminal_width();
    let mut writer = writer;
    let _session = TerminalSession::start(&mut writer).map_err(|error| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("无法启用动态终端模式：{error}"),
        )
    })?;
    let mut renderer = Renderer::new(writer, width, color);
    let mut wizard = Wizard::new(config, accounts, discovery_notice, repository);
    let mut candidate_loader = candidate_loader;
    loop {
        renderer.render(&wizard.frame(width))?;
        let key = read_key()?;
        if let Some(result) = wizard.handle_key(key, &mut candidate_loader) {
            match result {
                FlowResult::Cancelled => renderer.finish(&[
                    "已取消 ghis 初始设置。".into(),
                    "未写入任何配置、仓库或 shell 文件。".into(),
                ])?,
                FlowResult::Complete(_) => renderer.clear()?,
            }
            return Ok(result);
        }
    }
}

/// Conservative fallback for terminals without raw mode or cursor control.
pub fn run_line_mode<R, W, F>(
    mut reader: R,
    mut writer: W,
    config: &Config,
    accounts: &[GhAccount],
    discovery_notice: Option<String>,
    repository: Option<&Path>,
    mut candidate_loader: F,
) -> io::Result<FlowResult<Draft>>
where
    R: io::BufRead,
    W: Write,
    F: FnMut(&str, &str) -> Result<Vec<EmailCandidate>, String>,
{
    writeln!(writer, "ghis 初始设置（兼容模式）")?;
    if let Some(notice) = discovery_notice {
        writeln!(writer, "警告：{notice}")?;
    }
    macro_rules! answer {
        ($expression:expr) => {
            match $expression {
                Ok(value) => value,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::UnexpectedEof
                    ) =>
                {
                    writeln!(writer, "已取消，未写入任何文件。")?;
                    return Ok(FlowResult::Cancelled);
                }
                Err(error) => return Err(error),
            }
        };
    }
    let mut choices = accounts
        .iter()
        .map(|account| format!("{}@{}", account.login, account.host))
        .collect::<Vec<_>>();
    choices.push("手动输入账号".into());
    let account_index = answer!(line_choose(
        &mut reader,
        &mut writer,
        "选择 GitHub 账号",
        &choices,
        0,
    ));
    let (host, login) = if account_index < accounts.len() {
        (
            accounts[account_index].host.clone(),
            accounts[account_index].login.clone(),
        )
    } else {
        (
            answer!(line_text(
                &mut reader,
                &mut writer,
                "GitHub host",
                Some("github.com"),
            )),
            answer!(line_text(&mut reader, &mut writer, "GitHub login", None,)),
        )
    };
    let id = loop {
        let value = answer!(line_text(
            &mut reader,
            &mut writer,
            "Profile ID",
            Some(&login.to_ascii_lowercase()),
        ));
        if config.profiles.contains_key(&value) {
            writeln!(writer, "错误：Profile ID“{value}”已经存在。")?;
        } else {
            break value;
        }
    };
    let git_name = answer!(line_text(&mut reader, &mut writer, "提交姓名", None));
    let candidates = candidate_loader(&host, &login).unwrap_or_else(|error| {
        let _ = writeln!(writer, "警告：获取邮箱候选失败：{error}");
        Vec::new()
    });
    let mut emails = candidates
        .iter()
        .map(|candidate| candidate.email.clone())
        .collect::<Vec<_>>();
    emails.push("手动输入其他邮箱".into());
    let selected = answer!(line_choose(
        &mut reader,
        &mut writer,
        "选择提交邮箱",
        &emails,
        0,
    ));
    let git_email = if selected < candidates.len() {
        candidates[selected].email.clone()
    } else {
        answer!(line_text(&mut reader, &mut writer, "提交邮箱", None,))
    };
    let make_default = answer!(line_confirm(
        &mut reader,
        &mut writer,
        "设为默认 Profile",
        config.behavior.default_profile.is_none(),
    ));
    let bind_repository = repository.is_some()
        && answer!(line_confirm(
            &mut reader,
            &mut writer,
            "绑定当前 Git worktree",
            false,
        ));
    let setup_zsh = answer!(line_confirm(
        &mut reader,
        &mut writer,
        "安装 zsh wrapper",
        false,
    ));
    writeln!(
        writer,
        "\n确认配置：{id}，{login}@{}，{git_name} <{git_email}>",
        crate::github::normalize_host(&host)
    )?;
    if !answer!(line_confirm(
        &mut reader,
        &mut writer,
        "应用以上配置",
        false,
    )) {
        writeln!(writer, "已取消，未写入任何文件。")?;
        return Ok(FlowResult::Cancelled);
    }
    Ok(FlowResult::Complete(Draft {
        id,
        host: crate::github::normalize_host(&host),
        login,
        git_name,
        git_email,
        description: None,
        make_default,
        bind_repository,
        setup_zsh,
    }))
}

fn line_choose<R: io::BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
    values: &[String],
    default: usize,
) -> io::Result<usize> {
    loop {
        writeln!(writer, "\n{label}：")?;
        for (index, value) in values.iter().enumerate() {
            writeln!(writer, "  {}. {value}", index + 1)?;
        }
        write!(writer, "选择 [{}]：", default + 1)?;
        writer.flush()?;
        let value = read_line(reader)?;
        if value.eq_ignore_ascii_case("q") || value.eq_ignore_ascii_case("cancel") {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "用户取消"));
        }
        if value.trim().is_empty() {
            return Ok(default.min(values.len().saturating_sub(1)));
        }
        if let Ok(index) = value.trim().parse::<usize>()
            && (1..=values.len()).contains(&index)
        {
            return Ok(index - 1);
        }
        writeln!(writer, "错误：请输入列表编号。")?;
    }
}

fn line_text<R: io::BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
    default: Option<&str>,
) -> io::Result<String> {
    loop {
        match default {
            Some(default) => write!(writer, "{label} [{default}]：")?,
            None => write!(writer, "{label}：")?,
        }
        writer.flush()?;
        let input = read_line(reader)?;
        let value = if input.trim().is_empty() {
            default.unwrap_or_default().to_owned()
        } else {
            input.trim().to_owned()
        };
        if !value.is_empty() {
            return Ok(value);
        }
        writeln!(writer, "错误：此项不能为空。")?;
    }
}

fn line_confirm<R: io::BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
    default: bool,
) -> io::Result<bool> {
    loop {
        write!(
            writer,
            "{label} {}：",
            if default { "[Y/n]" } else { "[y/N]" }
        )?;
        writer.flush()?;
        let input = read_line(reader)?.trim().to_ascii_lowercase();
        if input.is_empty() {
            return Ok(default);
        }
        if matches!(input.as_str(), "y" | "yes" | "是") {
            return Ok(true);
        }
        if matches!(input.as_str(), "n" | "no" | "否") {
            return Ok(false);
        }
        writeln!(writer, "错误：请输入 y 或 n。")?;
    }
}

fn read_line(reader: &mut impl io::BufRead) -> io::Result<String> {
    let mut input = String::new();
    if reader.read_line(&mut input)? == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "输入已结束"));
    }
    Ok(input.trim_end_matches(['\r', '\n']).to_owned())
}

fn read_key() -> io::Result<InputKey> {
    loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return Ok(InputKey::Cancel);
        }
        let input = match key.code {
            KeyCode::Up => InputKey::Up,
            KeyCode::Down => InputKey::Down,
            KeyCode::Left => InputKey::Left,
            KeyCode::Right => InputKey::Right,
            KeyCode::Enter => InputKey::Enter,
            KeyCode::Esc => InputKey::Escape,
            KeyCode::Backspace | KeyCode::Delete => InputKey::Backspace,
            KeyCode::Char(' ') => InputKey::Space,
            KeyCode::Char(value) => InputKey::Character(value),
            _ => continue,
        };
        return Ok(input);
    }
}

fn edit_text(value: &mut String, key: InputKey) {
    match key {
        InputKey::Backspace => {
            value.pop();
        }
        InputKey::Space => value.push(' '),
        InputKey::Character(character) if !character.is_control() => value.push(character),
        _ => {}
    }
}

fn is_up(key: InputKey) -> bool {
    matches!(key, InputKey::Up | InputKey::Character('k'))
}

fn is_down(key: InputKey) -> bool {
    matches!(key, InputKey::Down | InputKey::Character('j'))
}

fn is_back(key: InputKey) -> bool {
    matches!(
        key,
        InputKey::Left | InputKey::Escape | InputKey::Character('h')
    )
}

fn is_forward(key: InputKey) -> bool {
    matches!(
        key,
        InputKey::Right | InputKey::Enter | InputKey::Character('l')
    )
}

fn previous(current: usize, count: usize) -> usize {
    if count == 0 {
        0
    } else {
        (current + count - 1) % count
    }
}

fn next(current: usize, count: usize) -> usize {
    if count == 0 { 0 } else { (current + 1) % count }
}

fn pad_columns(value: &str, width: usize) -> String {
    let current = UnicodeWidthStr::width(value);
    if current >= width {
        value.to_owned()
    } else {
        format!("{value}{}", " ".repeat(width - current))
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "是" } else { "否" }
}

fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .or_else(|| terminal::size().ok().map(|(width, _)| usize::from(width)))
        .unwrap_or(80)
        .clamp(MIN_WIDTH, MAX_WIDTH)
}

fn header(step: Step, width: usize) -> String {
    let left = "ghis 初始设置";
    let right = format!("{} / {TOTAL_STEPS}", step.number());
    let used = UnicodeWidthStr::width(left) + UnicodeWidthStr::width(right.as_str());
    if width > used + 3 {
        format!("{left}{}{right}", " ".repeat(width - used))
    } else {
        format!("{left}  {right}")
    }
}

fn layout_lines(lines: &[String], width: usize) -> Vec<String> {
    let width = width.saturating_sub(1).max(1);
    lines
        .iter()
        .flat_map(|line| wrap_line(line, width))
        .collect()
}

fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    let mut result = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for character in line.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if current_width > 0 && current_width + character_width > width {
            result.push(current);
            current = String::new();
            current_width = 0;
        }
        current.push(character);
        current_width += character_width;
    }
    result.push(current);
    result
}

fn paint(line: &str, color: bool) -> String {
    if !color {
        return line.to_owned();
    }
    if line.starts_with("ghis 初始设置") {
        format!("\x1b[36m{line}\x1b[0m")
    } else if line.starts_with('!') || line.starts_with("错误：") || line.starts_with("警告：")
    {
        format!("\x1b[33m{line}\x1b[0m")
    } else if line.starts_with('✓') {
        format!("\x1b[32m{line}\x1b[0m")
    } else if line.starts_with('>') || line.starts_with("  >") {
        format!("\x1b[36m{line}\x1b[0m")
    } else if line.contains("↑/") || line.starts_with("Enter ") || line.starts_with("j/k ") {
        format!("\x1b[2m{line}\x1b[0m")
    } else {
        line.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> GhAccount {
        GhAccount {
            host: "github.com".into(),
            login: "alice".into(),
            active: true,
            verified: true,
            ..GhAccount::default()
        }
    }

    #[test]
    fn menu_supports_hjkl_and_arrow_keys() {
        assert!(is_down(InputKey::Character('j')));
        assert!(is_up(InputKey::Character('k')));
        assert!(is_back(InputKey::Character('h')));
        assert!(is_forward(InputKey::Character('l')));
        assert!(is_down(InputKey::Down));
        assert!(is_up(InputKey::Up));
    }

    #[test]
    fn text_editor_keeps_hjkl_as_literal_text() {
        let mut text = String::new();
        for character in "hjkl".chars() {
            edit_text(&mut text, InputKey::Character(character));
        }
        assert_eq!(text, "hjkl");
    }

    #[test]
    fn back_returns_to_previous_step_and_preserves_answers() {
        let config = Config::default();
        let mut wizard = Wizard::new(&config, &[account()], None, None);
        let mut candidates = |_: &str, _: &str| Ok(Vec::new());
        wizard.handle_key(InputKey::Enter, &mut candidates);
        assert_eq!(wizard.step, Step::Identity);
        wizard.profile_id = "personal".into();
        wizard.handle_key(InputKey::Enter, &mut candidates);
        wizard.git_name = "Alice".into();
        wizard.handle_key(InputKey::Enter, &mut candidates);
        wizard.handle_key(InputKey::Character('h'), &mut candidates);
        assert_eq!(wizard.identity_field, IdentityField::GitName);
        wizard.handle_key(InputKey::Escape, &mut candidates);
        assert_eq!(wizard.identity_field, IdentityField::ProfileId);
        assert_eq!(wizard.profile_id, "personal");
    }

    #[test]
    fn duplicate_profile_id_stays_on_the_same_field() {
        let mut config = Config::default();
        config.profiles.insert("work".into(), Default::default());
        let mut wizard = Wizard::new(&config, &[account()], None, None);
        let mut candidates = |_: &str, _: &str| Ok(Vec::new());
        wizard.handle_key(InputKey::Enter, &mut candidates);
        wizard.profile_id = "work".into();
        wizard.handle_key(InputKey::Enter, &mut candidates);
        assert_eq!(wizard.identity_field, IdentityField::ProfileId);
        assert!(wizard.error.as_deref().unwrap().contains("已经存在"));
    }

    #[test]
    fn header_and_chinese_layout_fit_the_available_width() {
        for width in [24_usize, 40, 60, 80, 100] {
            let header = header(Step::Identity, width.saturating_sub(1));
            assert!(UnicodeWidthStr::width(header.as_str()) <= width.saturating_sub(1));
        }
        let lines = layout_lines(&["提交姓名：陈晓明@example.com".into()], 12);
        assert!(lines.len() > 1);
        assert!(
            lines
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) <= 11)
        );
    }

    #[test]
    fn email_view_keeps_summary_compact_and_explains_sources() {
        let config = Config::default();
        let accounts = [account()];
        let mut wizard = Wizard::new(&config, &accounts, None, None);
        wizard.step = Step::Identity;
        wizard.identity_field = IdentityField::EmailMenu;
        wizard.host = "github.com".into();
        wizard.login = "alice".into();
        wizard.git_name = "Alice".into();
        wizard.email_candidates = vec![
            EmailCandidate {
                email: "12345+alice@users.noreply.github.com".into(),
                noreply: true,
                verified: true,
                ..EmailCandidate::default()
            },
            EmailCandidate {
                email: "alice@example.test".into(),
                primary: true,
                verified: true,
                ..EmailCandidate::default()
            },
        ];
        let lines = wizard.frame(80);
        let account_line = lines
            .iter()
            .position(|line| line.starts_with("  账号"))
            .unwrap();
        assert_eq!(lines[account_line + 1], "  姓名    Alice");
        let noreply = lines
            .iter()
            .find(|line| line.contains("[GitHub noreply]"))
            .unwrap();
        let primary = lines
            .iter()
            .find(|line| line.contains("[GitHub 主邮箱]"))
            .unwrap();
        assert_eq!(
            noreply.find('[').unwrap(),
            primary.find('[').unwrap(),
            "邮箱来源标签应当对齐"
        );
    }

    #[test]
    fn options_use_plain_checkboxes_without_help_prompt() {
        let config = Config::default();
        let accounts = [account()];
        let mut wizard = Wizard::new(&config, &accounts, None, None);
        wizard.step = Step::Options;
        let lines = wizard.frame(80).join("\n");
        assert!(lines.contains("[x] 设为默认 Profile"));
        assert!(lines.contains("[ ] 安装 zsh wrapper"));
        assert!(!lines.contains("已选择"));
        assert!(!lines.contains("? 帮助"));
    }

    #[test]
    fn line_mode_collects_hjkl_text_without_cursor_control() {
        let input = io::Cursor::new("\n\nhjkl User\n\n\nn\ny\n");
        let mut output = Vec::new();
        let result = run_line_mode(
            input,
            &mut output,
            &Config::default(),
            &[account()],
            None,
            None,
            |_, _| {
                Ok(vec![EmailCandidate {
                    email: "alice@example.test".into(),
                    primary: true,
                    verified: true,
                    ..EmailCandidate::default()
                }])
            },
        )
        .unwrap();
        let FlowResult::Complete(draft) = result else {
            panic!("expected completed draft");
        };
        assert_eq!(draft.id, "alice");
        assert_eq!(draft.git_name, "hjkl User");
        assert_eq!(draft.git_email, "alice@example.test");
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("\x1b["));
    }

    #[test]
    fn line_mode_eof_cancels_without_a_draft() {
        let mut output = Vec::new();
        let result = run_line_mode(
            io::Cursor::new(Vec::<u8>::new()),
            &mut output,
            &Config::default(),
            &[account()],
            None,
            None,
            |_, _| Ok(Vec::new()),
        )
        .unwrap();
        assert_eq!(result, FlowResult::Cancelled);
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("未写入任何文件")
        );
    }

    #[test]
    fn renderer_clears_only_its_previous_region() {
        let mut renderer = Renderer::new(Vec::new(), 80, false);
        renderer
            .render(&["第一帧".into(), "第二行".into()])
            .unwrap();
        renderer.render(&["第二帧".into()]).unwrap();
        let output = String::from_utf8(renderer.writer).unwrap();
        assert!(output.contains("\x1b[2A"));
        assert!(!output.contains("\x1b[J"));
        assert!(!output.contains("\x1b[2J"));
        assert!(!output.contains("\x1b[?1049h"));
    }
}
