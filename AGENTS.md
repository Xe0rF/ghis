# Repository Guidelines

## 项目结构

- `build.rs` 将 Git commit、工作区状态、tag、构建时间、目标平台和构建模式写入版本信息；`src/` 是 Rust 核心代码：`main.rs` 提供 CLI 入口，`app.rs` 负责身份解析与仓库绑定，`config.rs` 管理配置和 XDG 路径，`agent.rs` 与 `src/agent/` 提供 Claude/Codex agent 上下文注入、启动器和设置管理，`git.rs`、`credential.rs`、`credential_command.rs`、`github.rs`、`signing.rs` 分别封装 Git、Git 凭据助手协议、用户指定的取件命令、gh CLI 和 SSH 签名逻辑。
- `tests/` 放置 CLI、wrapper、worktree、凭据、SSH 和并发行为的集成测试。
- 如果新增、移动或删除源码目录、测试目录、脚本或打包入口，必须同步更新本节和相关开发命令，保持本指南与仓库实际结构一致。

## 构建与开发命令

项目使用 `rust-toolchain.toml` 指定工具链。常用命令：

```bash
cargo build                 # 开发构建
cargo build --release       # 发布构建
cargo fmt --all -- --check  # 检查格式
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all -- --test-threads=1
cargo audit                 # 检查已知安全漏洞
cargo deny check            # 检查许可证、来源和重复依赖
```

## 编码与命名

遵循 rustfmt 默认风格，使用 4 个空格缩进；函数和模块使用 `snake_case`，类型和 trait 使用 `PascalCase`，常量使用 `SCREAMING_SNAKE_CASE`。优先复用现有模块边界和错误类型，注释只解释非显然的设计约束。手工修改文件使用 `apply_patch`，默认保持 ASCII。

## 测试要求

测试函数使用行为描述命名；跨进程场景放在 `tests/`，纯逻辑放在对应源文件的 `#[cfg(test)]` 模块。涉及配置写入的测试必须使用临时 `HOME`、`XDG_CONFIG_HOME`、`XDG_CACHE_HOME`、`XDG_STATE_HOME`，不得读取或修改真实用户配置、token 或 SSH socket。

## 提交与合并请求

提交遵循 Conventional Commits，例如 `feat: 实现 GitHub 身份切换器 v0.1.0`、`test: 隔离 XDG 配置测试环境`。正文用短项目符号说明实际改动。合并请求应说明行为变化、测试命令和配置/安全影响。不得提交凭据、私钥或真实配置文件。

## 安全与配置

生产代码通过 `gh`、`git`、用户指定的取件命令和 1Password Agent 使用外部凭据，私钥和 token 不应落盘。修改配置路径、wrapper 或 credential helper 时，必须验证错误时不会回退到其他身份，并保留身份预览和脱敏输出。

### 凭据模式

`Profile.credential_mode` 决定 HTTPS 凭据由谁提供，共三态，缺省为 `manage`。

- `manage` 与 `command` 写入完全相同的助手字符串，仅助手进程内部的取件来源不同。这保证 `is_managed_credential_helper`、`remove_managed_credential_helpers` 与 `diagnostics::is_ghis_helper` 三处识别逻辑无需区分模式。新增模式时不要引入第二种助手字符串形状，否则 `unbind` 将无法清理自身写入的配置。
- `passthrough` 不写入任何凭据配置，**包括那个用于截断继承链的空值**。该模式存在的意义就是保留继承链，补上那一行等于取消模式。相应地，命令行的凭据助手覆盖检查对它放宽：`manage` 与 `command` 仍然拒绝。
- `passthrough` 与 `command` 都要在任何解析或联网失败时停止，不得回退到终端提示或另一个账号。`passthrough` 的差别只在于它根本不解析凭据。

### 取件命令的约定

`credential_command` 以 argv 形式保存，逐项传递给子进程，不经过 shell，也不得改写成 shell 字符串。执行前先清除继承的 `GH_TOKEN` 等 GitHub CLI 变量，否则宿主的 token 会覆盖命令的输出。捕获的 stdout 一律包 `Zeroizing`，stderr 经 `Redactor` 后才能进入错误信息。失败时先写 `quit=true\n\n` 并 flush，再返回错误。

校验只拒绝 argv 无法表达的内容：空元素、超长元素、控制字符，以及程序名以 `-` 开头。参数允许以 `-` 开头，`sh -c` 与 `--format` 都是正常用法；配置完全来自用户自己的文件，不构成注入面。
