# GitHub Identity Switcher (`ghis`)

`ghis` 按 Git 仓库或 worktree 选择 GitHub Profile，让 `git`、`gh`、IDE、钩子和 CI 使用一致的提交身份与凭据策略。

默认使用 HTTPS。ghis 不调用 `gh auth switch`，不保存 GitHub token，不改写 remote，也不修改全局 `user.name` 或 `user.email`。

身份、凭据或签名策略无法确认时，操作会停止，不会静默切换账号。Profile 可以把凭据交给 ghis 管理，也可以声明由用户自己负责。完整边界见 [Wiki：安全边界](https://github.com/Xe0rF/ghis/wiki/Security-Boundaries)。

## 凭据模式

`credential_mode` 决定 HTTPS 传输的凭据由谁提供。身份与签名不受它影响，三种模式下都由 ghis 管理。

| 模式 | ghis 写入凭据配置 | token 来源 | 适用 |
| --- | --- | --- | --- |
| `manage`（默认） | 写入，并先清空继承链 | `gh auth token` | GitHub 与 GitHub Enterprise |
| `passthrough` | 不写入，保留继承链 | 不参与 | 传输层交给 SSH 或你自己的 helper |
| `command` | 写入，并先清空继承链 | 你指定的命令 | 其他 forge，或同一 forge 的多账号 |

省略该字段等同于 `manage`，配置文件不会因此多出一行。

`passthrough` 是一次显式的责任转移：该仓库的凭据由你负责，`ghis` 不参与，也不会因为取不到凭据而中止。它会在联网操作前提示一次。

`command` 模式的命令以 argv 形式给出，不经过 shell：

```sh
ghis profile add gitlab \
  --host git.example.test --login frez79 --name frez79 --email you@example.test \
  --credential-mode command \
  --credential-command 'op,read,op://Private/GitLab/frez79/credential' \
  --credential-username oauth2
```

命令输出即 token，去除首尾空白后由 ghis 配上 username 返回给 Git。命令失败或无输出时，ghis 与 `manage` 一样停止凭据查询，不会回退到终端提示。命令中不要写入 token 本身，它属于外部凭据存储。

`behavior.credential_routing` 控制 `ghis git` 是否为其他主机提供凭据，默认 `per-host`：只要某个主机在配置里只有一个 Profile，且该 Profile 由 ghis 应答凭据（`manage` 或 `command`），wrapper 就在运行时为这个主机注入同样的 helper。当前 Profile 的主机、有两个及以上 Profile 的主机，以及 `passthrough` 的主机都不会被接管。该行为只存在于 wrapper 运行的子进程里，不写入仓库配置，因此直接运行 `/usr/bin/git`、IDE 或 `GHIS_BYPASS=1` 时不会生效。设为 `current-profile` 可恢复“只应答当前 Profile 主机”的旧行为：

```sh
ghis config set behavior.credential_routing current-profile
```

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

## 按仓库 URL 选择 Profile（clone / ls-remote）

`[[rules]]` 通常按仓库的 remote 解析。没有仓库时，`git clone <url>` 和 `git ls-remote <url>` 会改用命令里的 URL 作为解析上下文，因此下面的规则可以直接命中首次 clone：

```toml
[[rules]]
id = "gitlab-flowecho"
profile = "flowecho"
priority = 100
host = "git.flowecho.org"
```

```sh
git clone https://git.flowecho.org/team/project
```

注意边界：

- 只有字面量的 `clone` 和仓库外的 `ls-remote` 参与。仓库内的 `fetch` / `pull` / `push` / `ls-remote` 始终按绑定和规则解析，URL 不会改变提交身份或签名。
- URL 会先按 `url.<base>.insteadOf` 展开，再参与匹配。
- `--profile` 和 `GHIS_PROFILE` 仍然优先级最高。
- 选项解析是保守的：遇到未知选项、缩写或捆绑的短选项时不会推断 URL，行为与以前完全一致。
- 用 URL 选出的 Profile 不会 auto-bind、修复绑定或登记外层仓库；在已有仓库里 clone 不会改动那个仓库的 `ghis.profile`。
- `default_profile` 只在主机一致时套用。URL 不匹配任何规则时会继续使用现有 Git 配置，`unresolved = "fail"` 时则直接停止。

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
