#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$project_dir"

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
if [ -z "$version" ]; then
  echo "无法从 Cargo.toml 读取版本" >&2
  exit 2
fi

if [ -n "${GHIS_RELEASE_TAG:-}" ] && [ "$GHIS_RELEASE_TAG" != "v$version" ]; then
  echo "发布标签 $GHIS_RELEASE_TAG 与 Cargo.toml 版本 v$version 不一致" >&2
  exit 2
fi

target=$(rustc -vV | sed -n 's/^host: //p')
if [ -n "${GHIS_TARGET:-}" ] && [ "$GHIS_TARGET" != "$target" ]; then
  echo "本脚本需要运行生成的二进制，只支持本机目标 $target" >&2
  exit 2
fi

if [ -z "${SOURCE_DATE_EPOCH:-}" ]; then
  SOURCE_DATE_EPOCH=$(git log -1 --format=%ct 2>/dev/null || printf '0')
  export SOURCE_DATE_EPOCH
fi

# Keep build-machine paths and usernames out of panic locations embedded by
# Rust and dependencies. CARGO_ENCODED_RUSTFLAGS preserves paths containing
# spaces; fall back to RUSTFLAGS only when the caller already uses it.
ghis_separator=$(printf '\037')
ghis_project_remap="--remap-path-prefix=$project_dir=/build/ghis"
ghis_home_remap=""
if [ -n "${HOME:-}" ]; then
  ghis_home_remap="--remap-path-prefix=$HOME=/build/home"
fi
if [ -n "${CARGO_ENCODED_RUSTFLAGS:-}" ]; then
  CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS}${ghis_separator}${ghis_project_remap}"
  if [ -n "$ghis_home_remap" ]; then
    CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS}${ghis_separator}${ghis_home_remap}"
  fi
  export CARGO_ENCODED_RUSTFLAGS
elif [ -n "${RUSTFLAGS:-}" ]; then
  RUSTFLAGS="$RUSTFLAGS $ghis_project_remap $ghis_home_remap"
  export RUSTFLAGS
else
  CARGO_ENCODED_RUSTFLAGS="$ghis_project_remap"
  if [ -n "$ghis_home_remap" ]; then
    CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS}${ghis_separator}${ghis_home_remap}"
  fi
  export CARGO_ENCODED_RUSTFLAGS
fi
cargo build --release --locked
binary="target/release/ghis"

if [ ! -x "$binary" ]; then
  echo "没有找到 release 二进制：$binary" >&2
  exit 2
fi

dist_dir=${GHIS_DIST_DIR:-"$project_dir/dist"}
mkdir -p "$dist_dir"
stage=$(mktemp -d "${TMPDIR:-/tmp}/ghis-package.XXXXXX")
trap 'rm -r -- "$stage"' EXIT HUP INT TERM

package="ghis-v${version}-${target}"
package_dir="$stage/$package"
install -d -m 0755 "$package_dir" "$package_dir/completions"
install -m 0755 "$binary" "$package_dir/ghis"
install -m 0644 README.md LICENSE "$package_dir/"
"$binary" completion zsh > "$package_dir/completions/_ghis"
zsh -n "$package_dir/completions/_ghis"

archive="$dist_dir/$package.tar.gz"
rm -f -- "$archive" "$archive.sha256"
epoch=$SOURCE_DATE_EPOCH
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$epoch" \
  -C "$stage" -cf - "$package" | gzip -n -9 > "$archive"

archive_name=$(basename -- "$archive")
(
  cd "$dist_dir"
  sha256sum "$archive_name" > "$archive_name.sha256"
  sha256sum -c "$archive_name.sha256"
)
tar -tzf "$archive" >/dev/null

printf '%s\n' "$archive" "$archive.sha256"
