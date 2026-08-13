//! 行式 onboarding 交互。
//!
//! 该模块只负责收集内存中的答案，不读取终端状态，也不执行任何写入。
//! 业务层在最终确认后调用现有 Config/App/Shell API。

use crate::config::Config;
use crate::github::{EmailCandidate, GhAccount};
use std::io::{self, BufRead, Write};
use std::path::Path;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowResult<T> {
    Complete(T),
    Back,
    Cancelled,
}

#[derive(Debug)]
pub struct Prompt<R, W> {
    reader: R,
    writer: W,
    color: bool,
}

impl<R: BufRead, W: Write> Prompt<R, W> {
    pub fn new(reader: R, writer: W, color: bool) -> Self {
        Self {
            reader,
            writer,
            color,
        }
    }

    pub fn into_writer(self) -> W {
        self.writer
    }

    fn write(&mut self, text: &str) -> io::Result<()> {
        self.writer.write_all(text.as_bytes())?;
        self.writer.flush()
    }

    fn title(
        &mut self,
        step: usize,
        total: usize,
        name: &str,
        stages: &[(&str, Stage)],
    ) -> io::Result<()> {
        let line = if self.color {
            format!(
                "\x1b[36m── ghis 初始设置 ───────────────────────────────────────────── {step} / {total} ──\x1b[0m\n"
            )
        } else {
            format!(
                "── ghis 初始设置 ───────────────────────────────────────────── {step} / {total} ──\n"
            )
        };
        self.write(&line)?;
        let mut track = String::new();
        for (index, (stage, state)) in stages.iter().enumerate() {
            if index > 0 {
                track.push_str(" ──> ");
            }
            let marker = match state {
                Stage::Done => "[完成]",
                Stage::Current => "[当前]",
                Stage::Todo => "[待办]",
            };
            track.push_str(marker);
            track.push(' ');
            track.push_str(stage);
        }
        self.write(&format!("{track}\n\n{name}\n\n"))
    }

    fn hint(&mut self, text: &str) -> io::Result<()> {
        let value = format!("Hint: {text}\n");
        if self.color {
            self.write(&format!("\x1b[2m{value}\x1b[0m"))
        } else {
            self.write(&value)
        }
    }

    fn read_line(&mut self, prompt: &str) -> io::Result<Option<String>> {
        self.write(prompt)?;
        let mut input = String::new();
        if self.reader.read_line(&mut input)? == 0 {
            return Ok(None);
        }
        Ok(Some(input.trim_end_matches(['\r', '\n']).to_owned()))
    }

    fn command(input: &str) -> Command {
        match input.trim().to_ascii_lowercase().as_str() {
            "back" => Command::Back,
            "cancel" | "q" => Command::Cancel,
            "?" => Command::Help,
            _ => Command::Value,
        }
    }

    fn text(
        &mut self,
        label: &str,
        default: Option<&str>,
        required: bool,
    ) -> io::Result<FlowResult<String>> {
        loop {
            let prompt = match default {
                Some(value) if !value.is_empty() => format!("{label} [{value}]："),
                _ => format!("{label}："),
            };
            let Some(input) = self.read_line(&prompt)? else {
                return Ok(FlowResult::Cancelled);
            };
            match Self::command(&input) {
                Command::Cancel => return Ok(FlowResult::Cancelled),
                Command::Back => return Ok(FlowResult::Back),
                Command::Help => {
                    self.write("输入值后按 Enter，输入 back 返回，输入 cancel 取消。\n")?;
                    continue;
                }
                Command::Value => {
                    let value = if input.trim().is_empty() {
                        default.unwrap_or_default().to_owned()
                    } else {
                        input.trim().to_owned()
                    };
                    if required && value.trim().is_empty() {
                        self.write("错误：此项不能为空。\n")?;
                        continue;
                    }
                    return Ok(FlowResult::Complete(value));
                }
            }
        }
    }

    fn choose(
        &mut self,
        label: &str,
        values: &[String],
        default: usize,
    ) -> io::Result<FlowResult<usize>> {
        if values.is_empty() {
            return Ok(FlowResult::Cancelled);
        }
        let mut selected = default.min(values.len() - 1);
        loop {
            self.write(&format!("{label}\n"))?;
            for (index, value) in values.iter().enumerate() {
                let marker = if index == selected { ">" } else { " " };
                let suffix = if index == selected { "  [默认]" } else { "" };
                self.write(&format!("  {marker} {}. {value}{suffix}\n", index + 1))?;
            }
            let Some(input) = self.read_line("选择：")? else {
                return Ok(FlowResult::Cancelled);
            };
            match Self::command(&input) {
                Command::Cancel => return Ok(FlowResult::Cancelled),
                Command::Back => return Ok(FlowResult::Back),
                Command::Help => {
                    self.write("输入编号后按 Enter，输入 j/k 后按 Enter 移动默认项，直接 Enter 确认默认项。\n")?;
                }
                Command::Value => {
                    let value = input.trim();
                    if value.is_empty() {
                        return Ok(FlowResult::Complete(selected));
                    }
                    match value.to_ascii_lowercase().as_str() {
                        "j" => selected = (selected + 1) % values.len(),
                        "k" => selected = (selected + values.len() - 1) % values.len(),
                        _ => match value.parse::<usize>() {
                            Ok(index) if (1..=values.len()).contains(&index) => {
                                return Ok(FlowResult::Complete(index - 1));
                            }
                            _ => self.write("错误：请输入列表中的编号、j、k 或 ?。\n")?,
                        },
                    }
                }
            }
        }
    }

    pub fn confirm_final(&mut self) -> io::Result<FlowResult<bool>> {
        self.confirm("继续吗？", false)
    }

    fn confirm(&mut self, label: &str, default: bool) -> io::Result<FlowResult<bool>> {
        let suffix = if default { "[Y/n]" } else { "[y/N]" };
        let Some(input) = self.read_line(&format!("{label} {suffix}："))? else {
            return Ok(FlowResult::Cancelled);
        };
        match Self::command(&input) {
            Command::Cancel => Ok(FlowResult::Cancelled),
            Command::Back => Ok(FlowResult::Cancelled),
            Command::Help => {
                self.write("输入 y/yes 或 n/no 后按 Enter，直接 Enter 使用安全默认值。\n")?;
                self.confirm(label, default)
            }
            Command::Value => {
                let value = input.trim().to_ascii_lowercase();
                if value.is_empty() {
                    Ok(FlowResult::Complete(default))
                } else if matches!(value.as_str(), "y" | "yes") {
                    Ok(FlowResult::Complete(true))
                } else if matches!(value.as_str(), "n" | "no") {
                    Ok(FlowResult::Complete(false))
                } else {
                    self.write("错误：请输入 y 或 n。\n")?;
                    self.confirm(label, default)
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Done,
    Current,
    Todo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Value,
    Back,
    Cancel,
    Help,
}

pub fn collect<R: BufRead, W: Write, F: FnMut(&str, &str) -> Vec<EmailCandidate>>(
    prompt: &mut Prompt<R, W>,
    config: &Config,
    accounts: &[GhAccount],
    mut candidate_loader: F,
    repository: Option<&Path>,
) -> io::Result<FlowResult<Draft>> {
    let stages = [
        ("目标", Stage::Done),
        ("账号", Stage::Current),
        ("身份", Stage::Todo),
        ("选项", Stage::Todo),
        ("确认", Stage::Todo),
    ];
    prompt.title(2, 5, "2. 选择 GitHub 账号", &stages)?;
    let mut account_values = accounts
        .iter()
        .map(|account| {
            let state = if account.verified {
                "已验证"
            } else {
                "未验证"
            };
            let active = if account.active {
                "，当前账号"
            } else {
                ""
            };
            format!("{} / {}{}，{}", account.host, account.login, active, state)
        })
        .collect::<Vec<_>>();
    account_values.push("手动输入 host 和 login".into());
    prompt.hint("此步骤只读取 gh 已登录账号，不会执行 gh auth login，也不会读取或保存 token。")?;
    let account = match prompt.choose("发现的账号：", &account_values, 0)? {
        FlowResult::Cancelled | FlowResult::Back => return Ok(FlowResult::Cancelled),
        FlowResult::Complete(index) if index < accounts.len() => {
            let account = &accounts[index];
            (account.host.clone(), account.login.clone())
        }
        FlowResult::Complete(_) => {
            let host = match prompt.text("GitHub host", Some("github.com"), true)? {
                FlowResult::Complete(value) => value,
                FlowResult::Cancelled | FlowResult::Back => return Ok(FlowResult::Cancelled),
            };
            let login = match prompt.text("GitHub login", None, true)? {
                FlowResult::Complete(value) => value,
                FlowResult::Cancelled | FlowResult::Back => return Ok(FlowResult::Cancelled),
            };
            (host, login)
        }
    };

    let (host, login) = account;
    let candidates = candidate_loader(&host, &login);
    prompt.title(
        3,
        5,
        "3. 设置提交身份",
        &[
            ("目标", Stage::Done),
            ("账号", Stage::Done),
            ("身份", Stage::Current),
            ("选项", Stage::Todo),
            ("确认", Stage::Todo),
        ],
    )?;
    let suggested_id = login.to_ascii_lowercase();
    let id = match prompt.text("Profile ID", Some(&suggested_id), true)? {
        FlowResult::Complete(value) => value,
        FlowResult::Cancelled | FlowResult::Back => return Ok(FlowResult::Cancelled),
    };
    if config.profiles.contains_key(&id) {
        prompt.write("错误：该 Profile ID 已存在，请输入其他 ID。\n")?;
        return Ok(FlowResult::Cancelled);
    }
    let git_name = match prompt.text("提交姓名", None, true)? {
        FlowResult::Complete(value) => value,
        FlowResult::Cancelled | FlowResult::Back => return Ok(FlowResult::Cancelled),
    };
    let email_values = candidates
        .iter()
        .map(|candidate| candidate.email.clone())
        .chain(["手动输入邮箱".into()])
        .collect::<Vec<_>>();
    let email_index = match prompt.choose("提交邮箱：", &email_values, 0)? {
        FlowResult::Complete(value) => value,
        FlowResult::Cancelled | FlowResult::Back => return Ok(FlowResult::Cancelled),
    };
    let git_email = if email_index < candidates.len() {
        candidates[email_index].email.clone()
    } else {
        match prompt.text("提交邮箱", None, true)? {
            FlowResult::Complete(value) => value,
            FlowResult::Cancelled | FlowResult::Back => return Ok(FlowResult::Cancelled),
        }
    };
    prompt.hint("GitHub Enterprise 不提供 GitHub.com noreply 候选，可使用手工邮箱。")?;

    prompt.title(
        4,
        5,
        "4. 可选设置",
        &[
            ("目标", Stage::Done),
            ("账号", Stage::Done),
            ("身份", Stage::Done),
            ("选项", Stage::Current),
            ("确认", Stage::Todo),
        ],
    )?;
    let make_default = match prompt.confirm(
        "设为默认 Profile？",
        config.behavior.default_profile.is_none(),
    )? {
        FlowResult::Complete(value) => value,
        FlowResult::Cancelled | FlowResult::Back => return Ok(FlowResult::Cancelled),
    };
    let bind_repository = if let Some(repository) = repository {
        prompt.hint(&format!(
            "也可稍后运行 ghis use {id} --repo {} 绑定。",
            repository.display()
        ))?;
        match prompt.confirm("绑定当前 Git worktree？", false)? {
            FlowResult::Complete(value) => value,
            FlowResult::Cancelled | FlowResult::Back => return Ok(FlowResult::Cancelled),
        }
    } else {
        prompt.hint(&format!("进入目标仓库后运行 ghis use {id} 完成绑定。"))?;
        false
    };
    let setup_zsh = match prompt.confirm("安装 zsh wrapper？", false)? {
        FlowResult::Complete(value) => value,
        FlowResult::Cancelled | FlowResult::Back => return Ok(FlowResult::Cancelled),
    };
    if !setup_zsh {
        prompt.hint("可稍后运行 ghis setup；脚本中使用 ghis setup --yes。")?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn config() -> Config {
        Config::default()
    }

    #[test]
    fn menu_supports_number_and_jk_without_cursor_control() {
        let input = Cursor::new("j\n\n");
        let output = Vec::new();
        let mut prompt = Prompt::new(input, output, false);
        let result = prompt
            .choose("选择", &["one".into(), "two".into()], 0)
            .unwrap();
        assert_eq!(result, FlowResult::Complete(1));
        let output = prompt.into_writer();
        assert!(!String::from_utf8(output).unwrap().contains('\u{00b7}'));
    }

    #[test]
    fn collect_cancel_does_not_create_draft() {
        let input = Cursor::new("cancel\n");
        let mut prompt = Prompt::new(input, Vec::new(), false);
        let result = collect(&mut prompt, &config(), &[], |_, _| Vec::new(), None).unwrap();
        assert_eq!(result, FlowResult::Cancelled);
    }
}
