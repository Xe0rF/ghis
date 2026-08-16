# GitHub Identity Switcher (`ghis`)

`ghis` 是跨平台的本地 GitHub 提交身份切换器，支持 Linux、macOS 和 Windows。它按仓库或 worktree 选择 Profile，让普通的 `git commit`、`git push` 和 `gh` 命令使用对应的提交姓名、邮箱与 GitHub 账号，并在敏感操作前显示实际身份。Unix shell 集成支持 zsh、bash 和 fish；Windows 原生 shell 集成仅支持 PowerShell 7（`pwsh`），不支持 Windows PowerShell 5.1。

默认使用 HTTPS，不要求配置 SSH Authentication Key。ghis 不调用 `gh auth switch`，不保存 GitHub token，不改写 remote，也不会修改全局 `user.name` 或 `user.email`。

## 核心能力

- 按仓库绑定身份，也可用规则、工作目录、remote owner 或默认值自动选择。
- 提供完整 CLI 和 zsh、bash、fish、PowerShell 集成。
- 自动发现 `gh` 已保存的账号，并为 HTTPS 操作精确选择对应凭据。
- 可选集成 1Password SSH Agent 和 `op-ssh-sign`，为不同 Profile 使用不同的 SSH commit signing key。
- 提供身份预览、结构化执行检查和带安全快速修复的 `doctor`；已选身份不可用时不会静默换成另一个账号。
- 可在 Claude Code 会话 Hook 或 Codex developer instructions 中主动注入最小 ghis 上下文，无需 MCP、skill 或联网下载文档。

## 安装

Linux、macOS 和 Windows 原生运行需要 Rust 1.97+、Git 与 GitHub CLI（`gh`）；Unix shell 集成使用 zsh、bash 或 fish，Windows 原生 shell 集成需要 PowerShell 7（`pwsh`）。当前 GitHub Release 已验证的预编译归档 target 为 `x86_64-unknown-linux-gnu`、`aarch64-unknown-linux-gnu` 和 `aarch64-apple-darwin`；其他 target（包括 Windows）需要从源码构建。Windows 上的 SSH signing、1Password socket 发现和 Unix shell 属于单独的兼容边界，默认使用 HTTPS 与 PowerShell 7。

```sh
cargo install --locked --path .
```

`ghis --version` 会同时显示 Git commit、clean/dirty 状态、精确 tag、UTC 构建时间、
目标平台和 debug/release 模式。设置 `SOURCE_DATE_EPOCH` 后，构建时间使用该 Unix
时间戳，以便生成可复现的发布产物。

本地生成发布归档需要 POSIX `sh`、zsh、bash 和 Python 3：

```sh
scripts/package-release.sh
```

## 快速开始

首次使用推荐运行中文初始化向导：

```sh
ghis onboard
# 也可以指定要评估并可选绑定的仓库
ghis onboard --repo ~/src/project
```

向导使用一个局部动态区域展示四个步骤，并在切换步骤时原地更新；它不会清空终端、进入 alternate screen 或启动全屏 TUI，完成后只留下结果摘要。选择界面同时支持方向键和 Vim 键位：`j/k` 或 `↓/↑` 移动，`h/←/Esc` 返回，`l/→/Enter` 确认，`Space` 切换附加选项，`q` 或 `Ctrl+C` 取消。编辑 Profile ID、姓名等文本时，`h/j/k/l` 是普通输入字符。

设置 `NO_COLOR=1` 可禁用颜色；`TERM=dumb` 或 `GHIS_ONBOARD_LINE_MODE=1` 会使用不含光标控制的中文兼容模式。最终确认前不会修改主配置、仓库或 shell 文件；失败时会尝试恢复原状态并给出检查命令。

也可以继续使用独立命令手工完成相同配置。先确认需要使用的账号都已经由 `gh` 登录，然后创建 Profile、绑定当前仓库并安装 shell 集成。下面的账号、姓名、邮箱、路径均为示例，需要替换成自己的值：

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
```

PowerShell 7 使用 `ghis setup`（也可以显式运行 `ghis setup powershell`）；Unix 环境可显式选择 `zsh`、`bash` 或 `fish`。

之后照常使用原命令：

```sh
git commit -m "更新说明"
git push
gh pr list
```

不带参数运行 `ghis` 等同于 `ghis status`，直接显示当前 Profile 及其可选描述。详细的提交身份、GitHub 账号和签名设置可通过 `ghis profile show <id>` 或 `ghis status --json` 查看。`profile list` 默认已经列出全部 Profile，因此不提供冗余的 `--all`；使用 `-d/--details` 展开人类可读详情，或使用 `-j/--json` 获取机器输出，两者互斥。

高频参数提供了短形式，例如 Profile 的 `-H/--host`、`-l/--login`、`-n/--name`、`-e/--email`、`-d/--description`，仓库目标的 `-r/--repo`，非交互确认的 `-y/--yes`，以及工作目录的 `-C/--cwd`。低频的 SSH/signing 和清除类参数保留完整长名称。

## 本地隔离沙盒

不想手动准备 fake `gh`、临时 `HOME` 和测试仓库时，可以直接运行：

```sh
scripts/onboard-sandbox.sh
```

脚本会创建临时的 HOME、XDG 和 Git 配置，准备一个 Git 仓库，并让 fake `gh` 模拟 `github.com / alice`。退出后临时目录自动删除。可使用逐行兼容模式、进入隔离 shell，或在隔离环境中运行指定命令：

```sh
scripts/onboard-sandbox.sh --line
scripts/onboard-sandbox.sh --shell
scripts/onboard-sandbox.sh -- cargo test --all -- --test-threads=1
```

隔离 shell 中可运行：

```sh
cargo run --quiet -- onboard --repo "$GHIS_SANDBOX_REPO"
git -C "$GHIS_SANDBOX_REPO" config --local --list
gh              # 只会调用脚本内的 fake gh
```

若需要保留现场排查：

```sh
scripts/onboard-sandbox.sh --keep --shell
```

脚本退出时会打印临时目录路径。这个沙盒不会访问真实 GitHub、使用真实 token，或读写宿主的 gh、Git 和 XDG 配置。

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
ghis gh -- issue list --repo OWNER/REPO
ghis gh -- pr view https://github.com/OWNER/REPO/pull/123
```

也可以用规则让某个普通工作目录自动选择 Profile。`--cwd` 匹配执行 ghis 时的绝对工作目录，支持 glob 和 `~` 展开；它与只匹配 Git 元数据目录的 `--gitdir` 不同，因此在非 Git 目录中也有效：

```sh
ghis rule add discussions-xe0rf \
  --profile xe0rf \
  --priority 50 \
  --cwd '~/discussions/**'

cd ~/discussions/project-a
ghis gh -- issue list --repo OWNER/REPO
```

`--cwd` 可以与 `--host`、`--owner`、`--repo` 组合，使规则同时限制工作目录和 GitHub 目标。规则仍遵循显式 `--profile`、仓库绑定、规则优先级和歧义检测；规则只在运行时选择身份，不会把非 Git 目录绑定成仓库。

### 机器可读诊断输出

`ghis status --json`、`ghis doctor --json` 和 `ghis check --operation <git|gh> --json -- ...` 分别输出当前身份状态、环境诊断和单次操作预检；它们是三种不同的 JSON 对象，不能按同一字段集合解析。每个对象都包含整数 `schema_version`。同一 schema 版本内可能增加字段，调用方应忽略未知字段；仓库、配置、工具路径以及其他由运行环境产生的字段不保证跨机器或版本稳定。

退出码应与 JSON 一起判断：`status` 和完成诊断的 `doctor` 返回 `0`，`check` 在存在错误级检查项时返回 `1`，参数、配置或运行时失败返回 `2`，此时不保证 stdout 中有完整 JSON。机器输出不会包含 token 或私钥内容，但不能据此把整份输出视为可公开的脱敏报告：例如 `doctor` 可能包含 SSH Agent socket 路径，路径、账号和环境信息也可能敏感；分享前仍应审阅并按需移除。

### 远程开发与 SSH Agent forwarding

在远程开发机上使用本地 1Password SSH Agent 时，先由用户自行配置 SSH forwarding，例如 `ssh -A user@host` 或本地 `~/.ssh/config` 的 `ForwardAgent yes`；远端 SSH 服务端也必须允许 `AllowAgentForwarding yes`。登录后 OpenSSH 会为当前会话设置临时 `SSH_AUTH_SOCK`。

Profile 可以明确使用转发 Agent 进行 SSH commit signing：

```toml
[profiles.work.signing]
enabled = true
transport = "forwarded-agent"
signing_key = "/home/user/.ssh/work-signing.pub"
fingerprint = "SHA256:example"
```

`forwarded-agent` 只使用当前会话的 `SSH_AUTH_SOCK`，要求显式 public key 或 fingerprint 精确匹配；它不会查找本地 1Password socket、写入 `IdentityAgent`、覆盖 socket、自动开启 `ForwardAgent` 或把本机 `op-ssh-sign` 路径写到远端。ghis 不传输、导出或同步私钥，也不绕过 1Password 的本地批准。未显式配置 `program` 时使用远端 Git/OpenSSH 默认 SSH signer；只有显式配置的签名程序不可执行时，签名操作才会停止。

`local-agent` 是旧配置的默认 transport，继续使用本机 Agent/1Password 的现有发现行为。远程 forwarding、VS Code Remote、多跳 SSH 和容器的安全边界与排障步骤见 [Wiki](../../wiki) 的 [Remote Development and Agent Forwarding](../../wiki/Remote-Development-and-Agent-Forwarding)。

### 多 remote 与 push URL

Git 的 fetch 与 push 可能使用不同 URL。`git push backup` 按目标 remote 的 push URL、`remote.pushDefault` 或 branch push remote 选择；多个 `pushurl` 和 `insteadOf`/`pushInsteadOf` rewrite 还可能使一次操作访问多个或不同 transport。ghis 按具体操作检查实际目标，无法确认时保持保守策略；不会只依据 `origin` 猜测 SSH 安全状态。

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

Codex launcher 使用 CLI 的 `developer_instructions` 注入；在 Unix zsh、bash 或 fish 中运行 `ghis setup` 后，直接输入 `codex` 也会透明转发到该 launcher；PowerShell 等其他 shell 可显式运行 `ghis agent run codex --`。两者都不依赖 MCP、skill 或联网文档。上下文不包含 token、私钥、SSH socket、raw remote URL 或 Doctor 诊断。agent 在具体 `git`/`gh` 操作遇到阻力时，再从 shell 运行 `ghis check` 或 `ghis doctor` 获取结构化修复信息。

完整的配置、规则、1Password、SSH 签名、安全边界、Shell 说明和排障方法见 [Wiki](../../wiki)。

## 许可证

[GNU Affero General Public License v3.0 or later](LICENSE)（`AGPL-3.0-or-later`）
