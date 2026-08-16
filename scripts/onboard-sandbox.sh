#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
keep=0
shell_mode=0
line_mode=0
run_command=0

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
        --line)
            line_mode=1
            shift
            ;;
        --)
            shift
            run_command=1
            break
            ;;
        -h|--help)
            cat <<'USAGE'
用法：scripts/onboard-sandbox.sh [选项] [-- 命令 参数...]

创建隔离的 HOME、XDG 目录、Git 配置、fake gh 和临时 Git 仓库。
不带选项时直接启动 ghis onboard；--shell 进入隔离 shell。

选项：
  --shell       进入临时隔离 shell，手动运行 ghis 命令
  --line        强制 onboarding 使用逐行兼容模式
  --keep        退出后保留临时目录，并打印其路径
  --            将后续命令放到隔离环境中执行

示例：
  scripts/onboard-sandbox.sh
  scripts/onboard-sandbox.sh --line
  scripts/onboard-sandbox.sh --shell
  scripts/onboard-sandbox.sh -- cargo test --all -- --test-threads=1
USAGE
            exit 0
            ;;
        *)
            printf '未知选项：%s；使用 --help 查看用法。\n' "$1" >&2
            exit 2
            ;;
    esac
done

root=$(mktemp -d "${TMPDIR:-/tmp}/ghis-onboard.XXXXXX")
cleanup() {
    status=$?
    if [ "$keep" -eq 1 ]; then
        printf '已保留 sandbox：%s\n' "$root" >&2
    else
        rm -rf -- "$root"
    fi
    exit "$status"
}
trap cleanup EXIT HUP INT TERM

mkdir -p "$root/bin" "$root/home" "$root/config" "$root/cache" "$root/state" "$root/repo"
chmod 700 "$root" "$root/bin" "$root/home" "$root/config" "$root/cache" "$root/state" "$root/repo"
cat >"$root/bin/gh" <<'EOF'
#!/bin/sh
set -eu
case "$*" in
    'auth status --json hosts')
        printf '%s\n' '{"hosts":{"github.com":[{"host":"github.com","login":"alice","active":true,"state":"success"}]}}'
        ;;
    'auth token --hostname github.com --user alice')
        printf '%s\n' 'sandbox-token'
        ;;
    'api user')
        printf '%s\n' '{"id":12345,"login":"alice"}'
        ;;
    'api user/emails')
        printf '%s\n' '[{"email":"alice@example.test","primary":true,"verified":true}]'
        ;;
    *)
        printf 'sandbox fake gh 不支持：%s\n' "$*" >&2
        exit 64
        ;;
esac
EOF
chmod +x "$root/bin/gh"

# 仅使用 sandbox 内的 Git 配置，避免宿主设置影响测试或被测试写入。
GIT_CONFIG_GLOBAL="$root/gitconfig"
: >"$GIT_CONFIG_GLOBAL"
chmod 600 "$GIT_CONFIG_GLOBAL"
export GIT_CONFIG_GLOBAL
export GIT_CONFIG_SYSTEM=/dev/null

git -C "$root/repo" init -q
git -C "$root/repo" config user.name 'Sandbox User'
git -C "$root/repo" config user.email 'sandbox@example.test'
git -C "$root/repo" remote add origin https://github.com/alice/example.git

export HOME="$root/home"
export XDG_CONFIG_HOME="$root/config"
export XDG_CACHE_HOME="$root/cache"
export XDG_STATE_HOME="$root/state"
export PATH="$root/bin:$PATH"
export GHIS_SANDBOX_ROOT="$root"
export GHIS_SANDBOX_REPO="$root/repo"
export NO_COLOR="${NO_COLOR:-1}"
unset GHIS_CONFIG GHIS_PROFILE GHIS_BANNER_SHOWN GHIS_WRAPPER_ACTIVE GHIS_BYPASS
unset GHIS_DISABLE_CHPWD GHIS_CHPWD_ENABLED GHIS_REPO_PROFILE GHIS_REPO_PROFILE_DISPLAY GHIS_REPO_ROOT
unset GH_TOKEN GITHUB_TOKEN GH_ENTERPRISE_TOKEN GITHUB_ENTERPRISE_TOKEN GH_HOST SSH_AUTH_SOCK
if [ "$line_mode" -eq 1 ]; then
    export GHIS_ONBOARD_LINE_MODE=1
else
    unset GHIS_ONBOARD_LINE_MODE
fi

printf 'ghis onboarding sandbox\n'
printf '  root: %s\n' "$root"
printf '  repo: %s\n\n' "$GHIS_SANDBOX_REPO"

if [ "$shell_mode" -eq 1 ]; then
    printf '%s\n' '隔离 shell 中可运行：' >&2
    printf '%s\n' '  cargo run --quiet -- onboard --repo "$GHIS_SANDBOX_REPO"' >&2
    printf '%s\n' '  cargo run --quiet -- status --json' >&2
    printf '%s\n' '  git -C "$GHIS_SANDBOX_REPO" config --local --list' >&2
    (cd "$project_dir" && "${SHELL:-/bin/sh}" -i)
elif [ "$run_command" -eq 1 ]; then
    if [ "$#" -eq 0 ]; then
        printf '`--` 后需要提供要执行的命令。\n' >&2
        exit 64
    fi
    (cd "$project_dir" && "$@")
else
    cd "$project_dir"
    cargo run --quiet -- onboard --repo "$GHIS_SANDBOX_REPO"
fi
