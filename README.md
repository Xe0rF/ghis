# ghis

`ghis` 是面向 Linux 和 zsh 的本地 GitHub 提交身份切换器。它按仓库或 worktree 选择 Profile，让普通的 `git commit`、`git push` 和 `gh` 命令使用对应的提交姓名、邮箱与 GitHub 账号，并在敏感操作前显示实际身份。

默认使用 HTTPS，不要求配置 SSH Authentication Key。ghis 不调用 `gh auth switch`，不保存 GitHub token，不改写 remote，也不会修改全局 `user.name` 或 `user.email`。

## 核心能力

- 按仓库绑定身份，也可用规则、remote owner 或默认值自动选择。
- 提供中文 Vim 风格 TUI、完整 CLI 和透明的 zsh wrapper。
- 自动发现 `gh` 已保存的账号，并为 HTTPS 操作精确选择对应凭据。
- 可选集成 1Password SSH Agent 和 `op-ssh-sign`，为不同 Profile 使用不同的 SSH commit signing key。
- 提供身份预览、配置同步和 `doctor` 诊断；已选身份不可用时不会静默换成另一个账号。

## 安装

需要 Linux、Rust 1.97+、Git、GitHub CLI（`gh`）和 zsh。当前安装来源是源码构建或本地 Arch 归档。

```sh
cargo install --locked --path .
```

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
  --noreply

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

不带参数运行 `ghis` 会打开 TUI。常用按键包括 `h/j/k/l`、`gg/G`、`Ctrl-u/Ctrl-d`、`/`、`n/N`、`Enter` 和 `Esc`。

## 常用命令

```sh
ghis status                         # 查看当前仓库将使用的身份
ghis use work                       # 将当前仓库绑定到 work
ghis --profile work git -- push     # 单次临时使用 work，不改变绑定
ghis doctor                         # 检查依赖、账号和签名环境
ghis sync                           # 重建 Profile 配置片段并检查已登记仓库
```

完整的配置、规则、1Password、SSH 签名、安全边界、Shell 说明和排障方法见 [Wiki](../../wiki)。

## 许可证

[GNU Affero General Public License v3.0 or later](LICENSE)（`AGPL-3.0-or-later`）
