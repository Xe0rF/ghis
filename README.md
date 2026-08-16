# GitHub Identity Switcher (`ghis`)

`ghis` 按 Git 仓库或 worktree 选择 GitHub Profile，让普通的 `git`、`gh`、IDE、hook 和 CI 调用使用一致的提交身份与凭据策略。

默认使用 HTTPS。ghis 不调用 `gh auth switch`，不保存 GitHub token，不改写 remote，也不修改全局 `user.name` 或 `user.email`。身份无法明确解析或签名/凭据策略无法确认时，操作会停止而不会静默切换账号。

## 安装

需要 Rust 1.97+、Git 和 GitHub CLI（`gh`）：

```sh
cargo install --locked --path .
```

安装 shell 集成：

```sh
ghis setup                 # 自动检测当前 shell
ghis setup zsh             # 也可显式指定 zsh、bash、fish 或 powershell
ghis setup --print         # 只输出初始化内容，不写启动文件
```

Windows 原生环境使用 PowerShell 7（`pwsh`）。其他平台和 shell 的详细边界见 [Wiki：平台与 Shell](https://github.com/Xe0rF/ghis/wiki/Architecture#platform-and-shell-boundaries)。

## 快速开始

首次使用推荐运行向导：

```sh
ghis onboard
```

也可以手动创建并绑定 Profile：

```sh
ghis discover
ghis profile add personal \
  --login alice \
  --name "Alice" \
  --noreply \
  --description "个人项目"

cd ~/src/project
ghis use personal
ghis status
```

之后继续使用原命令：

```sh
git commit -m "更新说明"
git push
gh pr list
```

不带参数运行 `ghis` 等同于 `ghis status`。当前身份提示统一为：

```text
GHIS Profile: personal
```

## 常用命令

```sh
ghis status                          # 当前 Profile
ghis profile list                   # 所有 Profile
ghis profile show personal          # Profile 详情
ghis use personal -r ~/src/project   # 绑定仓库
ghis --profile personal git -- log   # 单次指定 Profile
ghis doctor                         # 检查依赖、账号和集成
ghis check --operation git -- status # 执行前检查
ghis sync                           # 重建配置片段
ghis prompt                         # prompt/direnv 使用的最小状态
ghis context                        # coding agent 使用的脱敏上下文
```

完整参数和子命令以本机帮助为准：

```sh
ghis --help
ghis profile --help
ghis agent --help
ghis setup --help
ghis doctor --help
```

`--json` 输出供脚本使用；调用方必须同时检查退出码。输出不会包含 token 或私钥，但路径、账号和环境信息仍可能敏感，分享前请审阅。

规则可按 host、owner、仓库或工作目录选择 Profile：

```sh
ghis rule add discussions \
  --profile personal \
  --priority 50 \
  --cwd '~/discussions/**'
```

## Coding agent

通过 ghis 启动 Claude Code 或 Codex：

```sh
ghis agent run claude --
ghis agent run codex --
ghis agent status codex
```

Claude Code 还支持由 ghis 管理的本地 Hook。集成只提供最小、脱敏的上下文，不替代每次 Git/`gh` 操作前的状态检查。

## 安全与高级场景

- Git fragment 和 credential helper 是 IDE、CI、hook、submodule 等直接启动 Git 时的主要集成面；shell wrapper 只是交互体验。
- SSH signing、1Password Agent、forwarded-agent、managed SSH 和 ProxyJump 有不同的信任边界；不能确认指定 key 时不会降级为无签名或其他 Profile。
- 容器建议只读挂载配置和 Git fragments，独立保存 cache/state，不要把 token、私钥或 SSH socket 写入镜像。
- subtree 继承父仓库身份；submodule 是独立仓库；monorepo 和 sparse checkout 默认按仓库解析，不按文件路径自动切换身份。

详细实现、安全边界、远程开发、容器、hooks、IDE 和排障说明请查看 Wiki：

- [Architecture](https://github.com/Xe0rF/ghis/wiki/Architecture)
- [Advanced Scenarios](https://github.com/Xe0rF/ghis/wiki/Advanced-Scenarios)
- [Remote Development and Agent Forwarding](https://github.com/Xe0rF/ghis/wiki/Remote-Development-and-Agent-Forwarding)
- [Security Boundaries](https://github.com/Xe0rF/ghis/wiki/Security-Boundaries)
- [Troubleshooting](https://github.com/Xe0rF/ghis/wiki/Troubleshooting)

## 本地沙盒

需要演示或测试时，可以使用不读取宿主配置和凭据的隔离环境：

```sh
scripts/onboard-sandbox.sh
scripts/onboard-sandbox.sh --shell
scripts/onboard-sandbox.sh -- cargo test --all -- --test-threads=1
```

## 许可证

[GNU Affero General Public License v3.0 or later](LICENSE)（`AGPL-3.0-or-later`）
