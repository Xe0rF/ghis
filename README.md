# GitHub Identity Switcher (`ghis`)

`ghis` 是面向 Linux 和 zsh 的本地 GitHub 提交身份切换器。它按仓库或 worktree 选择 Profile，让普通的 `git commit`、`git push` 和 `gh` 命令使用对应的提交姓名、邮箱与 GitHub 账号，并在敏感操作前显示实际身份。

默认使用 HTTPS，不要求配置 SSH Authentication Key。ghis 不调用 `gh auth switch`，不保存 GitHub token，不改写 remote，也不会修改全局 `user.name` 或 `user.email`。

## 核心能力

- 按仓库绑定身份，也可用规则、remote owner 或默认值自动选择。
- 提供完整 CLI 和透明的 zsh wrapper。
- 自动发现 `gh` 已保存的账号，并为 HTTPS 操作精确选择对应凭据。
- 可选集成 1Password SSH Agent 和 `op-ssh-sign`，为不同 Profile 使用不同的 SSH commit signing key。
- 提供身份预览、结构化执行检查和带安全快速修复的 `doctor`；已选身份不可用时不会静默换成另一个账号。
- 可在 Claude Code 会话 Hook 或 Codex developer instructions 中主动注入最小 ghis 上下文，无需 MCP、skill 或联网下载文档。

## 安装

需要 Linux、Rust 1.97+、Git、GitHub CLI（`gh`）和 zsh。当前安装来源是源码构建或本地 Arch 归档。

```sh
cargo install --locked --path .
```

`ghis --version` 会同时显示 Git commit、clean/dirty 状态、精确 tag、UTC 构建时间、
目标平台和 debug/release 模式。设置 `SOURCE_DATE_EPOCH` 后，构建时间使用该 Unix
时间戳，以便生成可复现的发布产物。

Arch Linux 可以从当前源码生成本地归档并交给 pacman 安装：

```sh
scripts/package-release.sh
(cd packaging/arch && makepkg -si)
```

## 快速开始

先确认需要使用的账号都已经由 `gh` 登录，然后创建 Profile、绑定当前仓库并安装 zsh wrapper。下面的账号、姓名、邮箱、路径均为示例，需要替换成自己的值：

```sh
ghis discover
ghis profile add personal \
  --login alice \
  --name "Alice" \
  --noreply \
  --description "个人开源项目"

cd ~/src/project
ghis use personal
ghis status

ghis setup
exec zsh
```

之后照常使用原命令：

```sh
git commit -m "更新说明"
git push
gh pr list
```

不带参数运行 `ghis` 等同于 `ghis status`，直接显示当前 Profile 及其可选描述。详细的提交身份、GitHub 账号和签名设置可通过 `ghis profile show <id>` 或 `ghis status --json` 查看。`profile list` 默认已经列出全部 Profile，因此不提供冗余的 `--all`；使用 `-d/--details` 展开人类可读详情，或使用 `-j/--json` 获取机器输出，两者互斥。

高频参数提供了短形式，例如 Profile 的 `-H/--host`、`-l/--login`、`-n/--name`、`-e/--email`、`-d/--description`，仓库目标的 `-r/--repo`，非交互确认的 `-y/--yes`，以及工作目录的 `-C/--cwd`。低频的 SSH/signing 和清除类参数保留完整长名称。

## 常用命令

```sh
ghis status                         # 查看当前仓库将使用的身份
ghis profile list                  # 简洁列出全部 Profile 及描述
ghis profile list -d               # 批量显示人类可读详情
ghis profile list -j               # 批量输出机器可读 JSON
ghis profile show work             # 显示单个 Profile 详情
ghis use work -r ~/src/project     # 将指定仓库绑定到 work
ghis --profile work git -- push     # 单次临时使用 work，不改变绑定
ghis doctor                         # 检查依赖、账号、集成并显示可用修复
ghis check --operation gh --json -- --repo OWNER/REPO issue list
ghis sync                           # 重建 Profile 配置片段并检查已登记仓库
```

在非 Git 目录中也可以把仓库或 PR/Issue URL 作为显式目标；ghis 会先从目标 host/owner 解析 Profile，再在取得 token 前验证主机：

```sh
ghis gh -- --repo OWNER/REPO issue list
ghis gh -- pr view https://github.com/OWNER/REPO/pull/123
```

## Agentic coding CLI

推荐通过 launcher 启动，使模型在第一条消息前就获得当前、脱敏的身份上下文：

```sh
ghis agent run claude --
ghis agent run codex --
```

Claude Code 还可以安装本地 `SessionStart`、`UserPromptSubmit` 和 `SubagentStart` Hook：

```sh
ghis agent setup claude --yes
ghis agent status claude
```

Codex launcher 使用 CLI 的 `developer_instructions` 注入；运行 `ghis setup` 后，zsh 中直接输入 `codex` 也会透明转发到该 launcher。两者都不依赖 MCP、skill 或联网文档。上下文不包含 token、私钥、SSH socket、raw remote URL 或 Doctor 诊断。agent 在具体 `git`/`gh` 操作遇到阻力时，再从 shell 运行 `ghis check` 或 `ghis doctor` 获取结构化修复信息。

完整的配置、规则、1Password、SSH 签名、安全边界、Shell 说明和排障方法见 [Wiki](../../wiki)。

## 许可证

[GNU Affero General Public License v3.0 or later](LICENSE)（`AGPL-3.0-or-later`）
