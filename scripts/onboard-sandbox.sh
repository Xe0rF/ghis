#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
keep=0
shell_mode=0
command_args=

while [ "$#" -gt 0 ]; do
    case "$1" in
        --keep)
            keep=1
            shift
            ;;
        --shell)
            shell_mode=1
            shift
            ;;
        --)
            shift
            command_args="$*"
            break
            ;;
        -h|--help)
            cat <<'USAGE'
用法：scripts/onboard-sandbox.sh [选项] [-- 命令 参数...]

创建隔离的 HOME、XDG 目录、fake gh 和临时 Git 仓库。
不带选项时直接启动 ghis onboard；--shell 进入隔离 shell。

选项：
  --shell       进入临时隔离 shell，手动运行 ghis 命令
  --keep        退出后保留临时目录，并打印其路径
  --            将后续命令放到隔离环境中执行

示例：
  scripts/onboard-sandbox.sh
  scripts/onboard-sandbox.sh --shell
  scripts/onboard-sandbox.sh -- cargo test --all -- --test-threads=1
USAGE
            exit 0
            ;;
        *)
            echo "未知选项：$1；使用 --help 查看用法。" >&2
            exit 2
            ;;
    esac
done

stage=$(mktemp -d "${TMPDIR:-/tmp}/ghis-onboard.XXXXXX")
bin_dir="$stage/bin"
repo_dir="$stage/repository"
mkdir -p "$bin_dir" "$repo_dir" "$stage/home"

cleanup() {
    status=$?
    if [ "$keep" -eq 1 ]; then
        printf '%s\n' "已保留隔离环境：$stage" >&2
    else
        rm -rf -- "$stage"
    fi
    exit "$status"
}
trap cleanup EXIT HUP INT TERM

cat > "$bin_dir/gh" <<'FAKE_GH'
#!/bin/sh
set -eu

case "$*" in
    "auth status --json hosts")
        printf '%s\n' '{
          "hosts": {
            "github.com": [
              {
                "host": "github.com",
                "login": "alice",
                "active": true,
                "state": "success"
              }
            ]
          }
        }'
        ;;
    "auth token --hostname github.com --user alice")
        printf '%s\n' 'sandbox-token'
        ;;
    "api user")
        printf '%s\n' '{"id":12345678,"login":"alice"}'
        ;;
    "api user/emails")
        printf '%s\n' '[]'
        ;;
    *)
        printf 'fake gh 不支持调用：%s\n' "$*" >&2
        exit 1
        ;;
esac
FAKE_GH
chmod 0755 "$bin_dir/gh"

# Use only repository-local Git configuration so the host user's Git settings
# cannot affect the sandbox or receive writes from the sandbox.
GIT_CONFIG_GLOBAL="$stage/gitconfig"
: > "$GIT_CONFIG_GLOBAL"
git -C "$repo_dir" init -q
git -C "$repo_dir" config user.name 'Sandbox User'
git -C "$repo_dir" config user.email 'sandbox@example.test'
git -C "$repo_dir" remote add origin https://github.com/example/sandbox.git

export HOME="$stage/home"
export XDG_CONFIG_HOME="$stage/config"
export XDG_CACHE_HOME="$stage/cache"
export XDG_STATE_HOME="$stage/state"
export GHIS_SANDBOX_ROOT="$stage"
export GHIS_SANDBOX_REPO="$repo_dir"
export PATH="$bin_dir:$PATH"
export GIT_CONFIG_GLOBAL
export GIT_CONFIG_SYSTEM=/dev/null
export NO_COLOR="${NO_COLOR:-1}"
unset GHIS_CONFIG GHIS_PROFILE GHIS_BANNER_SHOWN GHIS_WRAPPER_ACTIVE GHIS_BYPASS
unset GHIS_DISABLE_CHPWD GHIS_CHPWD_ENABLED GHIS_REPO_PROFILE GHIS_REPO_PROFILE_DISPLAY GHIS_REPO_ROOT
unset GH_TOKEN GITHUB_TOKEN GH_HOST

printf '%s\n' "ghis sandbox 已准备：$stage" >&2
printf '%s\n' "临时仓库：$repo_dir" >&2
printf '%s\n' 'fake gh：github.com / alice，ID 12345678' >&2

if [ "$shell_mode" -eq 1 ]; then
    printf '%s\n' '隔离 shell 中可运行：' >&2
    printf '%s\n' '  cargo run --quiet -- onboard --repo "$GHIS_SANDBOX_REPO"' >&2
    printf '%s\n' '  cargo run --quiet -- status --json' >&2
    printf '%s\n' '  git -C "$GHIS_SANDBOX_REPO" config --local --list' >&2
    "${SHELL:-/bin/sh}" -i
elif [ -n "$command_args" ]; then
    # command_args is intentionally evaluated only after the caller supplied
    # the explicit `--` separator, matching ordinary shell wrapper behavior.
    # shellcheck disable=SC2086
    (cd "$project_dir" && sh -c "$command_args")
else
    cd "$project_dir"
    cargo run --quiet -- onboard --repo "$repo_dir"
fi
