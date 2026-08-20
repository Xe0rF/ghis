# GitHub Identity Switcher (`ghis`)

`ghis` 按 Git 仓库或 worktree 选择 GitHub Profile，让 `git`、`gh`、IDE、钩子和 CI 使用一致的提交身份与凭据策略。

默认使用 HTTPS。ghis 不调用 `gh auth switch`，不保存 GitHub token，不改写 remote，也不修改全局 `user.name` 或 `user.email`。

身份、凭据或签名策略无法确认时，操作会停止，不会静默切换账号。完整边界见 [Wiki：安全边界](https://github.com/Xe0rF/ghis/wiki/Security-Boundaries)。

## 安装

需要：

- Rust 1.97+；
- Git；
- GitHub CLI（`gh`）。

从当前源码安装：

```sh
cargo install --locked --path .
```

安装 Shell 集成：

```sh
ghis shell setup
```

`ghis shell setup` 会自动检测当前 Shell。也可以显式指定 `zsh`、`bash`、`fish` 或 `powershell`：

```sh
ghis shell setup zsh
ghis shell setup --print  # 只输出初始化内容，不修改启动文件
```

Windows 原生环境使用 PowerShell 7（`pwsh`）。平台与 Shell 的适用范围见 [Wiki：平台与 Shell 边界](https://github.com/Xe0rF/ghis/wiki/Architecture#平台与-shell-边界)。

## 快速开始

首次使用推荐运行初始化向导：

```sh
ghis onboard
```

向导会发现 `gh` 已登录的账号，创建 Profile，并可将当前仓库绑定到所选 Profile。

也可以手动完成：

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

看到以下提示，说明当前仓库已经解析到 Profile：

```text
GHIS Profile: personal
```

`GHIS Profile` 提示只在对应输出流连接终端时显示；脚本、管道和重定向中会自动静默，便于机器解析。

之后继续使用原来的命令：

```sh
git commit -m "更新说明"
git push
gh pr list
```

不带参数运行 `ghis` 等同于 `ghis status`。

Profile 如何解析、绑定如何写入 worktree，以及配置片段如何让 IDE 和 CI 使用同一身份，见 [Wiki：架构](https://github.com/Xe0rF/ghis/wiki/Architecture)。规则、多 remote、1Password、SSH 和 Agentic 集成见 [Wiki：高级场景](https://github.com/Xe0rF/ghis/wiki/Advanced-Scenarios)。

遇到账号、凭据或签名问题时，先运行：

```sh
ghis doctor
```

按步骤排查见 [Wiki：故障排查](https://github.com/Xe0rF/ghis/wiki/Troubleshooting)。远程开发和 `forwarded-agent` 见 [Wiki：远程开发与 SSH Agent 转发](https://github.com/Xe0rF/ghis/wiki/Remote-Development-and-Agent-Forwarding)。

## 撤销集成与卸载

只移除当前 Shell 集成：

```sh
ghis shell uninstall
```

撤销 ghis 管理的 Shell、Claude Code 和当前仓库集成，同时保留 Profile、规则、配置、缓存和状态：

```sh
ghis teardown
ghis teardown --dry-run  # 只预览，不修改文件或仓库
```

完整删除当前配置 namespace 中的 ghis 用户数据需要显式确认：

```sh
ghis teardown --purge
ghis teardown --purge --yes  # 非交互环境
```

`teardown` 不删除 `ghis` 程序本体。二进制、man page 和补全等软件包文件仍由原安装器或包管理器卸载，例如 `cargo uninstall ghis` 或 `brew uninstall ghis`。

## 命令帮助

完整参数以本机帮助为准：

```sh
ghis --help
ghis profile --help
ghis shell --help
ghis shell setup --help
ghis teardown --help
ghis agent --help
ghis doctor --help
```

`--json` 输出供脚本使用。调用方必须同时检查退出码。输出不会包含 token 或私钥，但路径、账号和环境信息仍可能敏感，分享前请审阅。脚本应解析 stdout、单独处理 stderr，不要用 `2>&1` 合并后再解析；身份提示只写入终端，错误和安全警告始终写入 stderr。

## 许可证

[GNU Affero General Public License v3.0 or later](LICENSE)（`AGPL-3.0-or-later`）
