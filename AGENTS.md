# Repository Guidelines

## 项目结构

- `src/` 是 Rust 核心代码：`main.rs` 提供 CLI/TUI 入口，`app.rs` 负责身份解析与仓库绑定，`config.rs` 管理配置和 XDG 路径，`git.rs`、`credential.rs`、`github.rs`、`signing.rs` 分别封装 Git、凭据、gh CLI 和 SSH 签名逻辑。
- `src/tui.rs` 和 `src/snapshots/` 包含 TUI 状态、Vim 风格按键处理及 insta 快照。
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

测试函数使用行为描述命名；跨进程场景放在 `tests/`，纯逻辑放在对应源文件的 `#[cfg(test)]` 模块。涉及配置写入的测试必须使用临时 `HOME`、`XDG_CONFIG_HOME`、`XDG_CACHE_HOME`、`XDG_STATE_HOME`，不得读取或修改真实用户配置、token 或 SSH socket。TUI 变化需要更新 insta 快照并检查窄终端和 Unicode 输入。

## 提交与合并请求

提交遵循 Conventional Commits，例如 `feat: 实现 GitHub 身份切换器 v0.1.0`、`test: 隔离 XDG 配置测试环境`。正文用短项目符号说明实际改动。合并请求应说明行为变化、测试命令和配置/安全影响。若 TUI 改变布局、可见文本、颜色或交互结果，在合并请求描述中附终端截图或 `insta` 快照差异，方便审查视觉回归；纯逻辑改动无需截图。不得提交凭据、私钥或真实配置文件。

## 安全与配置

生产代码通过 `gh`、`git` 和 1Password Agent 使用外部凭据，私钥和 token 不应落盘。修改配置路径、wrapper 或 credential helper 时，必须验证错误时不会回退到其他身份，并保留身份预览和脱敏输出。
