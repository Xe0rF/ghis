#!/bin/sh
set -eu

keep=0
shell_mode=0
line_mode=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --keep) keep=1 ;;
        --shell) shell_mode=1 ;;
        --line) line_mode=1 ;;
        -h|--help)
            printf '%s\n' \
                'usage: scripts/onboard-sandbox.sh [--keep] [--shell] [--line]' \
                '' \
                '  --keep   退出后保留隔离目录' \
                '  --shell  进入隔离 shell，不自动运行向导' \
                '  --line   强制使用逐行兼容模式'
            exit 0
            ;;
        *) printf '未知参数：%s\n' "$1" >&2; exit 64 ;;
    esac
    shift
done

root=$(mktemp -d "${TMPDIR:-/tmp}/ghis-onboard.XXXXXX")
cleanup() {
    if [ "$keep" -eq 1 ]; then
        printf '已保留 sandbox：%s\n' "$root"
    else
        rm -rf "$root"
    fi
}
trap cleanup EXIT HUP INT TERM

mkdir -p "$root/bin" "$root/home" "$root/config" "$root/cache" "$root/state" "$root/repo"
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

git -C "$root/repo" init -q
git -C "$root/repo" remote add origin https://github.com/alice/example.git

export HOME="$root/home"
export XDG_CONFIG_HOME="$root/config"
export XDG_CACHE_HOME="$root/cache"
export XDG_STATE_HOME="$root/state"
export PATH="$root/bin:$PATH"
export GHIS_SANDBOX_ROOT="$root"
export GHIS_SANDBOX_REPO="$root/repo"
unset GHIS_CONFIG GHIS_PROFILE GH_TOKEN GITHUB_TOKEN SSH_AUTH_SOCK
if [ "$line_mode" -eq 1 ]; then
    export GHIS_ONBOARD_LINE_MODE=1
fi

printf 'ghis onboarding sandbox\n'
printf '  root: %s\n' "$root"
printf '  repo: %s\n\n' "$GHIS_SANDBOX_REPO"

if [ "$shell_mode" -eq 1 ]; then
    exec "${SHELL:-/bin/sh}"
fi

cargo run --quiet -- onboard --repo "$GHIS_SANDBOX_REPO"
