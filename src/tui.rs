//! Ratatui front end for ghis.
//!
//! The state/reducer pair is intentionally independent from the application
//! layer.  Slow discovery, git, and gh work can run elsewhere and update the
//! public fields on [`AppState`]; this module only handles terminal events and
//! drawing.  That makes keyboard behavior straightforward to unit test.

use std::collections::BTreeMap;
use std::io::{self, stdout};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Current interaction mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Normal mode handles navigation and commands.
    #[default]
    Normal,
    /// Insert mode edits the free-form input field.
    Insert,
    /// Search mode edits the list filter.
    Search,
    /// Confirm mode waits for `y`, `n`, Enter, or Escape.
    Confirm,
}

/// Top-level views available in the terminal interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum View {
    #[default]
    Status,
    Profiles,
    Rules,
    Settings,
    Diagnostics,
}

impl View {
    const ALL: [Self; 5] = [
        Self::Status,
        Self::Profiles,
        Self::Rules,
        Self::Settings,
        Self::Diagnostics,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Status => "状态",
            Self::Profiles => "身份",
            Self::Rules => "规则",
            Self::Settings => "设置",
            Self::Diagnostics => "诊断",
        }
    }

    fn shifted(self, delta: isize) -> Self {
        let current = Self::ALL
            .iter()
            .position(|view| *view == self)
            .expect("当前视图必须存在于视图列表中");
        let len = Self::ALL.len() as isize;
        let next = (current as isize + delta).rem_euclid(len) as usize;
        Self::ALL[next]
    }
}

/// Commands emitted by the reducer for the application layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    /// Periodic event-loop pulse used to receive background worker results.
    Tick,
    Quit,
    Refresh,
    SubmitInput,
    Confirm,
    Cancel,
    Bind,
    Unbind,
    Add,
    Edit,
    Activate,
    Delete,
    Help,
}

/// Descriptive aliases for integration code.
pub type TuiAction = Action;
pub type TuiMode = Mode;
pub type TuiView = View;

/// A compact view model rendered by the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    pub mode: Mode,
    pub view: View,
    pub repository: String,
    pub profile: String,
    pub git_identity: String,
    pub github_identity: String,
    pub transport: String,
    pub signing: String,
    pub items: Vec<String>,
    view_items: BTreeMap<View, Vec<String>>,
    pub selected: usize,
    pub input: String,
    /// Cursor measured in Unicode grapheme clusters, not bytes.
    pub cursor: usize,
    pub query: String,
    pub status: String,
    pub warnings: Vec<String>,
    /// Context for the current background task or confirmation workflow.
    pub details: Vec<String>,
    pub pending_g: bool,
    pub should_quit: bool,
    pub loading: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            mode: Mode::Normal,
            view: View::Status,
            repository: "未检测到 Git 仓库".to_string(),
            profile: "未绑定".to_string(),
            git_identity: "未知".to_string(),
            github_identity: "未知".to_string(),
            transport: "未知".to_string(),
            signing: "未启用".to_string(),
            items: Vec::new(),
            view_items: BTreeMap::new(),
            selected: 0,
            input: String::new(),
            cursor: 0,
            query: String::new(),
            status: "按 ? 查看帮助，按 q 退出".to_string(),
            warnings: Vec::new(),
            details: Vec::new(),
            pending_g: false,
            should_quit: false,
            loading: false,
        }
    }
}

impl AppState {
    /// Construct a view model with repository and profile text.
    pub fn new(repository: impl Into<String>, profile: impl Into<String>) -> Self {
        Self {
            repository: repository.into(),
            profile: profile.into(),
            ..Self::default()
        }
    }

    /// Replace the list and keep selection within bounds.
    pub fn set_items<I, S>(&mut self, items: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.items = items.into_iter().map(Into::into).collect();
        self.view_items.insert(self.view, self.items.clone());
        self.selected = self.selected.min(self.visible_len().saturating_sub(1));
    }

    /// Replace the list belonging to one top-level view.
    pub fn set_view_items<I, S>(&mut self, view: View, items: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let items = items.into_iter().map(Into::into).collect::<Vec<_>>();
        self.view_items.insert(view, items.clone());
        if self.view == view {
            self.items = items;
            self.selected = self.selected.min(self.visible_len().saturating_sub(1));
        }
    }

    /// Read the unfiltered items stored for a view.
    pub fn view_items(&self, view: View) -> &[String] {
        self.view_items.get(&view).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The currently selected item, if any.
    pub fn selected_item(&self) -> Option<&str> {
        let query = self.query.to_lowercase();
        self.items
            .iter()
            .filter(|item| query.is_empty() || item.to_lowercase().contains(&query))
            .nth(self.selected)
            .map(String::as_str)
    }

    fn visible_len(&self) -> usize {
        let query = self.query.to_lowercase();
        self.items
            .iter()
            .filter(|item| query.is_empty() || item.to_lowercase().contains(&query))
            .count()
    }

    /// Change input and place the grapheme cursor at the end.
    pub fn set_input(&mut self, input: impl Into<String>) {
        self.input = input.into();
        self.cursor = grapheme_count(&self.input);
    }

    /// Move the list selection by a signed number of rows.
    pub fn move_selection(&mut self, delta: isize) {
        let length = self.visible_len();
        if length == 0 {
            self.selected = 0;
            return;
        }
        let last = length - 1;
        self.selected = if delta.is_negative() {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected.saturating_add(delta as usize).min(last)
        };
    }

    fn move_cursor(&mut self, delta: isize) {
        let count = grapheme_count(&self.input);
        self.cursor = if delta.is_negative() {
            self.cursor.saturating_sub(delta.unsigned_abs())
        } else {
            self.cursor.saturating_add(delta as usize).min(count)
        };
    }

    fn insert_grapheme(&mut self, grapheme: &str) {
        let byte = byte_index_at_grapheme(&self.input, self.cursor);
        self.input.insert_str(byte, grapheme);
        // A scalar may merge with its neighbour (combining marks and emoji
        // ZWJ sequences), so recalculate instead of assuming one new cluster.
        self.cursor = grapheme_count(&self.input[..byte + grapheme.len()]);
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = byte_index_at_grapheme(&self.input, self.cursor - 1);
        let end = byte_index_at_grapheme(&self.input, self.cursor);
        self.input.replace_range(start..end, "");
        self.cursor -= 1;
    }

    fn switch_view(&mut self, delta: isize) {
        self.view = self.view.shifted(delta);
        self.items = self.view_items.get(&self.view).cloned().unwrap_or_default();
        self.selected = 0;
        self.query.clear();
        self.pending_g = false;
    }
}

/// Reduce a crossterm key event into an application action.
///
/// This is a deterministic reducer: it performs no I/O and does not call git
/// or gh.  The caller can execute the returned action on a worker thread and
/// then update the view model.
pub fn reduce(state: &mut AppState, key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        state.should_quit = true;
        return Action::Quit;
    }
    match state.mode {
        Mode::Normal => reduce_normal(state, key),
        Mode::Insert | Mode::Search => reduce_editing(state, key),
        Mode::Confirm => reduce_confirm(state, key),
    }
}

/// Verbose alias that reads naturally at event-loop call sites.
pub fn handle_key(state: &mut AppState, key: KeyEvent) -> Action {
    reduce(state, key)
}

fn reduce_normal(state: &mut AppState, key: KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if state.pending_g && key.code != KeyCode::Char('g') {
        state.pending_g = false;
    }
    match key.code {
        KeyCode::Char('q') if !ctrl => {
            state.should_quit = true;
            Action::Quit
        }
        KeyCode::Char('r') => Action::Refresh,
        KeyCode::Char('?') => Action::Help,
        KeyCode::Char('j') | KeyCode::Down => {
            state.move_selection(1);
            Action::None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.move_selection(-1);
            Action::None
        }
        KeyCode::Char('h') | KeyCode::Left | KeyCode::BackTab => {
            state.switch_view(-1);
            Action::None
        }
        KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => {
            state.switch_view(1);
            Action::None
        }
        KeyCode::Char('g') if !state.pending_g => {
            state.pending_g = true;
            Action::None
        }
        KeyCode::Char('g') => {
            state.pending_g = false;
            state.selected = 0;
            Action::None
        }
        KeyCode::Char('G') => {
            state.pending_g = false;
            state.selected = state.visible_len().saturating_sub(1);
            Action::None
        }
        KeyCode::Char('u') if ctrl => {
            state.move_selection(-page_size(state));
            Action::None
        }
        KeyCode::Char('d') if ctrl => {
            state.move_selection(page_size(state));
            Action::None
        }
        KeyCode::Char('/') => {
            state.mode = Mode::Search;
            state.input = state.query.clone();
            state.cursor = grapheme_count(&state.input);
            Action::None
        }
        KeyCode::Char('i') => {
            state.mode = Mode::Insert;
            state.cursor = grapheme_count(&state.input);
            Action::None
        }
        KeyCode::Char('b') => Action::Bind,
        KeyCode::Char('U') => Action::Unbind,
        KeyCode::Char('a') => Action::Add,
        KeyCode::Char('e') => Action::Edit,
        KeyCode::Char('D') => Action::Delete,
        KeyCode::Enter | KeyCode::Char(' ') => Action::Activate,
        KeyCode::Char('n') => {
            find_next(state, false);
            Action::None
        }
        KeyCode::Char('N') => {
            find_next(state, true);
            Action::None
        }
        KeyCode::Esc => {
            state.pending_g = false;
            Action::Cancel
        }
        _ => {
            state.pending_g = false;
            Action::None
        }
    }
}

fn reduce_editing(state: &mut AppState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            state.mode = Mode::Normal;
            state.input.clear();
            state.cursor = 0;
            Action::Cancel
        }
        KeyCode::Enter => {
            if state.mode == Mode::Search {
                state.query = state.input.clone();
                state.selected = 0;
                state.mode = Mode::Normal;
                state.input.clear();
                state.cursor = 0;
                return Action::None;
            }
            state.mode = Mode::Normal;
            Action::SubmitInput
        }
        KeyCode::Backspace => {
            state.backspace();
            Action::None
        }
        KeyCode::Left => {
            state.move_cursor(-1);
            Action::None
        }
        KeyCode::Right => {
            state.move_cursor(1);
            Action::None
        }
        KeyCode::Home => {
            state.cursor = 0;
            Action::None
        }
        KeyCode::End => {
            state.cursor = grapheme_count(&state.input);
            Action::None
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            let mut encoded = [0u8; 4];
            state.insert_grapheme(character.encode_utf8(&mut encoded));
            Action::None
        }
        _ => Action::None,
    }
}

fn reduce_confirm(state: &mut AppState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            state.mode = Mode::Normal;
            Action::Confirm
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            state.mode = Mode::Normal;
            Action::Cancel
        }
        _ => Action::None,
    }
}

fn find_next(state: &mut AppState, backwards: bool) {
    if state.query.is_empty() {
        return;
    }
    let length = state.visible_len();
    if length == 0 {
        state.selected = 0;
    } else if backwards {
        state.selected = (state.selected + length - 1) % length;
    } else {
        state.selected = (state.selected + 1) % length;
    }
}

fn page_size(state: &AppState) -> isize {
    // A fixed page keeps the reducer independent from terminal dimensions.
    // Rendering still adapts to a narrow terminal.
    let _ = state;
    8
}

/// Grapheme count used by the editor cursor.
pub fn grapheme_count(value: &str) -> usize {
    UnicodeSegmentation::graphemes(value, true).count()
}

fn byte_index_at_grapheme(value: &str, grapheme: usize) -> usize {
    UnicodeSegmentation::grapheme_indices(value, true)
        .nth(grapheme)
        .map_or(value.len(), |(byte, _)| byte)
}

/// Render the complete TUI view.
pub fn render(frame: &mut Frame<'_>, state: &AppState) {
    let area = frame.area();
    if area.width < 40 || area.height < 12 {
        render_too_small(frame, area);
        return;
    }
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Min(4),
            Constraint::Length(3),
        ])
        .split(area);

    render_header(frame, layout[0], state);
    render_body(frame, layout[1], state);
    render_footer(frame, layout[2], state);
}

fn render_too_small(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("终端窗口过小"),
            Line::from("请调整到至少 40x12"),
            Line::from("按 q 退出"),
        ])
        .block(Block::default().borders(Borders::ALL).title(" ghis "))
        .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_header(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let warning = if state.warnings.is_empty() {
        "状态：正常".to_string()
    } else {
        format!("警告：{}", state.warnings.join("；"))
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("仓库 ", Style::default().fg(Color::Cyan)),
            Span::raw(truncate_for_area(
                &state.repository,
                area.width as usize,
                20,
            )),
        ]),
        Line::from(vec![
            Span::styled("身份 ", Style::default().fg(Color::Cyan)),
            Span::raw(&state.profile),
            Span::raw("  "),
            Span::styled("提交 ", Style::default().fg(Color::Cyan)),
            Span::raw(&state.git_identity),
        ]),
        Line::from(vec![
            Span::styled("GitHub ", Style::default().fg(Color::Cyan)),
            Span::raw(&state.github_identity),
            Span::raw("  "),
            Span::styled("传输 ", Style::default().fg(Color::Cyan)),
            Span::raw(&state.transport),
            Span::raw("  "),
            Span::styled("签名 ", Style::default().fg(Color::Cyan)),
            Span::raw(&state.signing),
        ]),
        Line::from(Span::styled(
            warning,
            if state.warnings.is_empty() {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Yellow)
            },
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" ghis 身份 · 当前视图：{} ", state.view.label())),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_body(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(area);

    let query = state.query.to_lowercase();
    let filtered = state
        .items
        .iter()
        .filter(|item| query.is_empty() || item.to_lowercase().contains(&query))
        .map(|item| ListItem::new(render_list_row(item)))
        .collect::<Vec<_>>();
    let empty = filtered.is_empty();
    let item_count = filtered.len();
    let list = if empty {
        List::new(vec![ListItem::new(match state.view {
            View::Status => "（没有状态明细）",
            View::Profiles => "（尚未配置身份）",
            View::Rules => "（尚未配置规则）",
            View::Settings => "（没有可修改的设置）",
            View::Diagnostics => "（没有诊断结果）",
        })])
    } else {
        List::new(filtered)
    }
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", state.view.label())),
    )
    .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .highlight_symbol("› ");
    let mut list_state = ListState::default();
    if !empty {
        list_state.select(Some(state.selected.min(item_count.saturating_sub(1))));
    }
    frame.render_stateful_widget(list, columns[0], &mut list_state);

    let mut details = vec![
        Line::from(Span::styled("当前操作", Style::default().fg(Color::Cyan))),
        Line::from(format!("身份配置：{}", state.profile)),
        Line::from(format!("提交身份：{}", state.git_identity)),
        Line::from(format!("GitHub 账号：{}", state.github_identity)),
        Line::from(format!("传输方式：{}", state.transport)),
        Line::from(format!("提交签名：{}", state.signing)),
    ];
    if state.loading {
        details.push(Line::from(Span::styled(
            "正在后台执行命令…",
            Style::default().fg(Color::Yellow),
        )));
    }
    details.extend(
        state
            .details
            .iter()
            .map(|detail| Line::from(detail.as_str())),
    );
    frame.render_widget(
        Paragraph::new(details)
            .block(Block::default().borders(Borders::ALL).title(" 预览 "))
            .wrap(Wrap { trim: true }),
        columns[1],
    );
}

fn render_list_row(item: &str) -> String {
    item.replace('\t', "  ")
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let title = match state.mode {
        Mode::Normal => " 普通模式 ",
        Mode::Insert => " 输入模式 ",
        Mode::Search => " 搜索 ",
        Mode::Confirm => " 确认 ",
    };
    let content = match state.mode {
        Mode::Normal => format!(
            "{}  |  h/l 视图  j/k 选择  / 搜索  b 绑定  U 解绑  r 刷新  ? 帮助  q 退出{}",
            state.status,
            if state.pending_g {
                "  (再按 g 到顶部)"
            } else {
                ""
            }
        ),
        Mode::Confirm => format!("确认此操作？{}", state.input),
        Mode::Insert | Mode::Search => format!("{}: {}", title.trim(), state.input),
    };
    frame.render_widget(
        Paragraph::new(content)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: true }),
        area,
    );
    if matches!(state.mode, Mode::Insert | Mode::Search) && area.width > 2 && area.height > 2 {
        let prefix_width = UnicodeWidthStr::width(format!("{}: ", title.trim()).as_str());
        let input_width = UnicodeWidthStr::width(
            &state.input[..byte_index_at_grapheme(&state.input, state.cursor)],
        );
        let x = area
            .x
            .saturating_add(1)
            .saturating_add((prefix_width + input_width) as u16)
            .min(area.right().saturating_sub(2));
        frame.set_cursor_position((x, area.y.saturating_add(1)));
    }
}

fn truncate_for_area(value: &str, width: usize, reserved: usize) -> String {
    let target = width.saturating_sub(reserved).max(8);
    if UnicodeWidthStr::width(value) <= target {
        return value.to_string();
    }
    let mut result = String::new();
    let mut used = 0usize;
    for grapheme in UnicodeSegmentation::graphemes(value, true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if used + grapheme_width + 1 > target {
            break;
        }
        used += grapheme_width;
        result.push_str(grapheme);
    }
    result.push('…');
    result
}

/// Run the event loop with raw mode and an alternate screen.
///
/// The application layer can execute actions returned by [`reduce`] in a
/// worker.  This convenience loop only handles local actions and is suitable
/// for a first-screen TUI or tests with a fake terminal.
pub fn run(state: AppState) -> io::Result<AppState> {
    run_with_handler(state, |_, _| {})
}

/// Run the event loop and send application actions to a callback.
///
/// The callback is invoked for every action except [`Action::None`] and may
/// update the state before the next frame is drawn. Terminal I/O and local
/// navigation remain owned by the TUI.
pub fn run_with_handler<F>(mut state: AppState, mut handler: F) -> io::Result<AppState>
where
    F: FnMut(&mut AppState, Action),
{
    let mut terminal = TerminalGuard::enter()?;
    let mut dirty = true;
    loop {
        if dirty {
            terminal.terminal.draw(|frame| render(frame, &state))?;
            dirty = false;
        }
        if state.should_quit {
            break;
        }
        if event::poll(std::time::Duration::from_millis(100))? {
            let before = state.clone();
            match event::read()? {
                Event::Key(key) => {
                    let action = reduce(&mut state, key);
                    dispatch_action(&mut state, action, &mut handler);
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
            dirty |= state != before;
        }
        let before = state.clone();
        dispatch_action(&mut state, Action::Tick, &mut handler);
        dirty |= state != before;
    }
    Ok(state)
}

fn dispatch_action<F>(state: &mut AppState, action: Action, handler: &mut F)
where
    F: FnMut(&mut AppState, Action),
{
    if action != Action::None {
        handler(state, action);
    }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut output = stdout();
        if let Err(error) = execute!(output, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        let backend = CrosstermBackend::new(output);
        let terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = disable_raw_mode();
                return Err(error);
            }
        };
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn snapshot_text(state: &AppState) -> String {
        let backend = TestBackend::new(100, 26);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, state)).expect("draw");
        buffer_text(terminal.backend().buffer())
    }

    fn buffer_text(buffer: &Buffer) -> String {
        let width = buffer.area.width as usize;
        buffer
            .content()
            .chunks(width)
            .map(|row| {
                let mut output = String::new();
                let mut column = 0usize;
                while let Some(cell) = row.get(column) {
                    let symbol = cell.symbol();
                    output.push_str(symbol);
                    column += UnicodeWidthStr::width(symbol).max(1);
                }
                output.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn bound_state() -> AppState {
        let mut state = AppState::new("/home/alice/src/ghis", "personal");
        state.git_identity = "Alice <alice@users.noreply.github.com>".into();
        state.github_identity = "github.com / alice".into();
        state.transport = "HTTPS".into();
        state.signing = "未启用".into();
        state.status = "身份已就绪".into();
        state.set_items([
            "仓库已绑定：personal",
            "提交身份与 profile 一致",
            "HTTPS 凭据由 ghis 按账号提供",
        ]);
        state.details = vec!["解析来源：仓库绑定".into(), "凭据状态：可用".into()];
        state
    }

    #[test]
    fn list_rows_render_tabs_as_visible_spacing() {
        assert_eq!(
            render_list_row("Git\tgit version test"),
            "Git  git version test"
        );
        assert_eq!(
            render_list_row("警告\tuser.email 范围=global"),
            "警告  user.email 范围=global"
        );
    }

    #[test]
    fn multiline_setting_description_renders_below_its_value() {
        let mut state = bound_state();
        state.view = View::Settings;
        state.set_items([
            "auto_bind\t开启\n  含义：唯一规则匹配后自动绑定当前仓库",
            "default_profile\tpersonal\n  含义：无匹配时使用的默认 Profile",
        ]);

        let rendered = snapshot_text(&state);
        assert!(rendered.contains("› auto_bind  开启"));
        assert!(rendered.contains("  含义：唯一规则匹配后自动绑定当前仓库"));
        assert!(rendered.contains("  default_profile  personal"));
        assert!(rendered.contains("  含义：无匹配时使用的默认 Profile"));
    }

    #[test]
    fn vim_navigation_and_pending_gg_work() {
        let mut state = AppState::default();
        state.set_items(["a", "b", "c"]);
        reduce(&mut state, key(KeyCode::Char('j')));
        assert_eq!(state.selected, 1);
        reduce(&mut state, key(KeyCode::Char('G')));
        assert_eq!(state.selected, 2);
        reduce(&mut state, key(KeyCode::Char('g')));
        assert!(state.pending_g);
        reduce(&mut state, key(KeyCode::Char('g')));
        assert_eq!(state.selected, 0);

        reduce(&mut state, key(KeyCode::Char('g')));
        reduce(&mut state, key(KeyCode::Char('j')));
        assert!(!state.pending_g);
    }

    #[test]
    fn views_cycle_with_vim_and_tab_keys() {
        let mut state = AppState::default();
        state.set_view_items(View::Status, ["status"]);
        state.set_view_items(View::Profiles, ["personal", "work"]);
        assert_eq!(state.view, View::Status);

        reduce(&mut state, key(KeyCode::Char('l')));
        assert_eq!(state.view, View::Profiles);
        assert_eq!(state.items, vec!["personal", "work"]);
        reduce(&mut state, key(KeyCode::Tab));
        assert_eq!(state.view, View::Rules);
        reduce(&mut state, key(KeyCode::Char('h')));
        assert_eq!(state.view, View::Profiles);
        reduce(&mut state, key(KeyCode::BackTab));
        assert_eq!(state.view, View::Status);
        assert_eq!(state.items, vec!["status"]);
        reduce(&mut state, key(KeyCode::Char('h')));
        assert_eq!(state.view, View::Diagnostics);
        reduce(&mut state, key(KeyCode::Char('l')));
        assert_eq!(state.view, View::Status);
    }

    #[test]
    fn handler_receives_non_none_actions_and_can_update_state() {
        let mut state = AppState::default();
        let mut actions = Vec::new();
        let mut handler = |state: &mut AppState, action| {
            actions.push(action);
            state.status = "动作已处理".into();
        };

        let navigation = reduce(&mut state, key(KeyCode::Char('j')));
        dispatch_action(&mut state, navigation, &mut handler);
        let bind = reduce(&mut state, key(KeyCode::Char('b')));
        dispatch_action(&mut state, bind, &mut handler);

        assert_eq!(actions, vec![Action::Bind]);
        assert_eq!(state.status, "动作已处理");
    }

    #[test]
    fn tick_is_dispatched_for_background_result_polling() {
        let mut state = AppState::default();
        let mut actions = Vec::new();
        dispatch_action(&mut state, Action::Tick, &mut |_, action| {
            actions.push(action);
        });
        assert_eq!(actions, vec![Action::Tick]);
    }

    #[test]
    fn filtered_selection_matches_the_visible_row() {
        let mut state = AppState::default();
        state.set_items(["personal", "work", "work-alt"]);
        state.query = "work".into();
        state.selected = 0;
        assert_eq!(state.selected_item(), Some("work"));
        state.move_selection(1);
        assert_eq!(state.selected_item(), Some("work-alt"));
        state.move_selection(1);
        assert_eq!(state.selected_item(), Some("work-alt"));
    }

    #[test]
    fn submitting_a_search_filters_without_emitting_a_save_action() {
        let mut state = AppState::default();
        state.set_items(["personal", "work"]);
        assert_eq!(reduce(&mut state, key(KeyCode::Char('/'))), Action::None);
        assert_eq!(state.mode, Mode::Search);
        assert_eq!(reduce(&mut state, key(KeyCode::Char('w'))), Action::None);
        assert_eq!(reduce(&mut state, key(KeyCode::Enter)), Action::None);
        assert_eq!(state.mode, Mode::Normal);
        assert_eq!(state.query, "w");
        assert_eq!(state.selected_item(), Some("work"));
    }

    #[test]
    fn unicode_input_moves_by_grapheme() {
        let mut state = AppState::default();
        reduce(&mut state, key(KeyCode::Char('i')));
        reduce(&mut state, key(KeyCode::Char('你')));
        reduce(&mut state, key(KeyCode::Char('🙂')));
        assert_eq!(state.input, "你🙂");
        assert_eq!(state.cursor, 2);
        reduce(&mut state, key(KeyCode::Left));
        reduce(&mut state, key(KeyCode::Backspace));
        assert_eq!(state.input, "🙂");
    }

    #[test]
    fn combining_mark_keeps_cursor_on_grapheme_boundary() {
        let mut state = AppState::default();
        reduce(&mut state, key(KeyCode::Char('i')));
        reduce(&mut state, key(KeyCode::Char('e')));
        reduce(&mut state, key(KeyCode::Char('\u{301}')));
        assert_eq!(grapheme_count(&state.input), 1);
        assert_eq!(state.cursor, 1);
        reduce(&mut state, key(KeyCode::Backspace));
        assert!(state.input.is_empty());
    }

    #[test]
    fn ctrl_c_quits_from_insert_mode() {
        let mut state = AppState {
            mode: Mode::Insert,
            ..AppState::default()
        };
        let action = reduce(
            &mut state,
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            },
        );
        assert_eq!(action, Action::Quit);
        assert!(state.should_quit);
    }

    #[test]
    fn snapshot_normal_state() {
        insta::assert_snapshot!(snapshot_text(&bound_state()));
    }

    #[test]
    fn snapshot_search_and_confirm_modes() {
        let mut search = bound_state();
        search.view = View::Profiles;
        search.set_items([
            "personal · github.com/alice",
            "work · github.example.com/alice",
        ]);
        search.mode = Mode::Search;
        search.query = "work".into();
        search.set_input("work");

        let mut confirm = bound_state();
        confirm.mode = Mode::Confirm;
        confirm.input = "解绑仓库 /home/alice/src/ghis 与 personal".into();
        confirm.details = vec!["即将移除仓库本地绑定".into(), "profile 配置不会删除".into()];

        insta::assert_snapshot!(format!(
            "=== 搜索模式 ===\n{}\n\n=== 确认模式 ===\n{}",
            snapshot_text(&search),
            snapshot_text(&confirm)
        ));
    }

    #[test]
    fn snapshot_unbound_state() {
        let mut state = AppState::new("/home/alice/src/unresolved", "未绑定");
        state.git_identity = "System User <system@example.test>".into();
        state.github_identity = "未解析".into();
        state.transport = "HTTPS".into();
        state.signing = "由现有 Git 配置决定".into();
        state.status = "未绑定，将保持现有 Git 行为".into();
        state.warnings = vec!["没有规则能唯一确定身份".into()];
        state.set_items(["仓库尚未绑定 ghis profile", "未匹配到唯一身份规则"]);
        state.details = vec!["敏感操作前会再次显示警告".into()];

        insta::assert_snapshot!(snapshot_text(&state));
    }

    #[test]
    fn snapshot_authentication_failure() {
        let mut state = bound_state();
        state.status = "认证失败，已停止操作".into();
        state.warnings = vec!["personal 的 GitHub 凭据不可用；不会回退到其他账号".into()];
        state.set_items(["GitHub 账号：github.com / alice", "认证状态：失败"]);
        state.details = vec![
            "写操作未执行".into(),
            "请运行 gh auth login 修复账号".into(),
        ];

        insta::assert_snapshot!(snapshot_text(&state));
    }

    #[test]
    fn snapshot_missing_one_password() {
        let mut state = bound_state();
        state.transport = "SSH（纳管）".into();
        state.signing = "SSH（1Password）".into();
        state.status = "1Password SSH Agent 不可用".into();
        state.warnings = vec!["未找到已配置的 1Password Agent socket".into()];
        state.set_items(["SSH 密钥：personal", "Agent：不可用"]);
        state.details = vec![
            "SSH push 与签名操作已停止".into(),
            "不会尝试其他 SSH 身份".into(),
        ];

        insta::assert_snapshot!(snapshot_text(&state));
    }

    #[test]
    fn snapshot_signing_error() {
        let mut state = bound_state();
        state.signing = "SSH（错误）".into();
        state.status = "提交签名检查失败".into();
        state.warnings = vec!["op-ssh-sign 不可执行；commit 已停止".into()];
        state.set_items(["签名：已启用", "签名程序：不可用"]);
        state.details = vec![
            "未创建 commit".into(),
            "请检查 profile 的 signing.program".into(),
        ];

        insta::assert_snapshot!(snapshot_text(&state));
    }

    #[test]
    fn test_backend_renders_non_blank() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut state = AppState::new("~/src/demo", "personal");
        state.github_identity = "github.com / alice".to_string();
        state.set_items(["personal", "work"]);
        terminal.draw(|frame| render(frame, &state)).expect("draw");
        let buffer = terminal.backend().buffer();
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| !cell.symbol().trim().is_empty())
        );
    }

    #[test]
    fn render_handles_tiny_terminals_and_long_unicode_state() {
        for (width, height) in [(8, 4), (20, 8), (40, 10), (60, 16), (120, 30)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let mut state = AppState::new(
                "/非常长的工作目录/含 空格/项目/linked-worktree",
                "工作身份-profile-with-a-long-name",
            );
            state.mode = Mode::Insert;
            state.git_identity = "组合字符 e\u{301} 与中文姓名 <very-long@example.test>".into();
            state.github_identity = "git.example.com/一个很长的登录名".into();
            state.transport = "https".into();
            state.signing = "SSH（1Password）".into();
            state.warnings = vec!["凭据暂时不可用；不会回退到其他账号".into()];
            state.details = vec!["后台诊断仍在运行，界面保持响应".into()];
            state.set_items([
                "第一条包含中文和很长的路径 /src/example/project",
                "第二条 e\u{301} emoji 🙂",
            ]);
            state.set_input("规则|工作身份|50|github.com|所有者|仓库||||");

            terminal.draw(|frame| render(frame, &state)).expect("draw");
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer.area.width, width);
            assert_eq!(buffer.area.height, height);
            assert!(
                buffer
                    .content()
                    .iter()
                    .any(|cell| !cell.symbol().trim().is_empty()),
                "{width}x{height} should not render a blank screen"
            );
            if width == 20 && height == 8 {
                let rendered = buffer
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                // Wide glyph continuation cells may appear between symbols
                // in TestBackend's raw cell stream.
                for glyph in ["终", "端", "窗", "口", "过", "小"] {
                    assert!(rendered.contains(glyph));
                }
            }
        }
    }
}
