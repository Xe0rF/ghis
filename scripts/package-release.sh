#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$project_dir"

usage() {
  echo "用法：$0 [--target <Rust target triple>]" >&2
  exit 2
}

requested_target=${GHIS_TARGET:-}
while [ "$#" -gt 0 ]; do
  case "$1" in
    --target)
      [ "$#" -ge 2 ] || usage
      target_argument=$2
      shift 2
      ;;
    --target=*)
      target_argument=${1#--target=}
      [ -n "$target_argument" ] || usage
      shift
      ;;
    *)
      usage
      ;;
  esac

  if [ -n "${target_argument:-}" ]; then
    if [ -n "$requested_target" ] && [ "$requested_target" != "$target_argument" ]; then
      echo "GHIS_TARGET 与 --target 指定的目标不一致" >&2
      exit 2
    fi
    requested_target=$target_argument
    unset target_argument
  fi
done

host_target=$(rustc -vV | sed -n 's/^host: //p')
if [ -z "$host_target" ]; then
  echo "无法从 rustc 读取本机目标" >&2
  exit 2
fi
target=${requested_target:-"$host_target"}

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
if [ -z "$version" ]; then
  echo "无法从 Cargo.toml 读取版本" >&2
  exit 2
fi

if [ -n "${GHIS_RELEASE_TAG:-}" ] && [ "$GHIS_RELEASE_TAG" != "v$version" ]; then
  echo "发布标签 $GHIS_RELEASE_TAG 与 Cargo.toml 版本 v$version 不一致" >&2
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

# Honour an explicitly selected Cargo target directory; otherwise use the
# project's conventional target directory.
if [ "${CARGO_TARGET_DIR+x}" = x ]; then
  target_dir=$CARGO_TARGET_DIR
else
  target_dir="$project_dir/target"
  CARGO_TARGET_DIR=$target_dir
  export CARGO_TARGET_DIR
fi

cargo build --release --locked --target "$target"

binary_name=ghis
case "$target" in
  *-windows-*) binary_name=ghis.exe ;;
esac
host_binary_name=ghis
case "$host_target" in
  *-windows-*) host_binary_name=ghis.exe ;;
esac
binary="$target_dir/$target/release/$binary_name"
if [ ! -x "$binary" ]; then
  echo "没有找到 release 二进制：$binary" >&2
  exit 2
fi

completion_binary=$binary
if [ "$target" != "$host_target" ]; then
  cargo build --release --locked --target "$host_target"
  completion_binary="$target_dir/$host_target/release/$host_binary_name"
fi
if [ ! -x "$completion_binary" ]; then
  echo "没有找到用于生成补全的本机 release 二进制：$completion_binary" >&2
  exit 2
fi

dist_dir=${GHIS_DIST_DIR:-"$project_dir/dist"}
mkdir -p "$dist_dir"
stage=$(mktemp -d "${TMPDIR:-/tmp}/ghis-package.XXXXXX")
trap 'rm -r -- "$stage"' EXIT HUP INT TERM

package="ghis-v${version}-${target}"
package_dir="$stage/$package"
install -d -m 0755 "$package_dir" "$package_dir/completions"
install -m 0755 "$binary" "$package_dir/$binary_name"
install -m 0644 README.md LICENSE "$package_dir/"
"$completion_binary" completion zsh > "$package_dir/completions/_ghis"
zsh -n "$package_dir/completions/_ghis"

archive_format=tar.gz
case "$target" in
  *-windows-*) archive_format=zip ;;
esac
archive="$dist_dir/$package.$archive_format"
rm -f -- "$archive" "$archive.sha256"
python3 - "$stage" "$package" "$archive" "$SOURCE_DATE_EPOCH" "$archive_format" <<'PY'
from datetime import datetime, timezone
from gzip import GzipFile
from hashlib import sha256
from pathlib import Path
import stat
import sys
import tarfile
import zipfile

stage = Path(sys.argv[1])
package = sys.argv[2]
archive = Path(sys.argv[3])
epoch = int(sys.argv[4])
archive_format = sys.argv[5]
package_dir = stage / package
paths = [package_dir, *sorted(package_dir.rglob("*"))]
archive.parent.mkdir(parents=True, exist_ok=True)

if archive_format == "tar.gz":
    with archive.open("wb") as output:
        with GzipFile(filename="", mode="wb", fileobj=output, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.GNU_FORMAT) as tar:
                for path in paths:
                    info = tar.gettarinfo(path, arcname=str(path.relative_to(stage)))
                    info.uid = 0
                    info.gid = 0
                    info.uname = ""
                    info.gname = ""
                    info.mtime = epoch
                    if info.isfile():
                        with path.open("rb") as source:
                            tar.addfile(info, source)
                    else:
                        tar.addfile(info)
elif archive_format == "zip":
    zip_epoch = max(epoch, 315532800)  # ZIP timestamps cannot predate 1980.
    timestamp = datetime.fromtimestamp(zip_epoch, timezone.utc)
    date_time = (
        timestamp.year,
        timestamp.month,
        timestamp.day,
        timestamp.hour,
        timestamp.minute,
        timestamp.second & ~1,
    )
    with zipfile.ZipFile(archive, mode="w", compression=zipfile.ZIP_DEFLATED) as zip_file:
        for path in paths:
            relative = path.relative_to(stage).as_posix()
            is_directory = path.is_dir()
            info = zipfile.ZipInfo(relative + "/" if is_directory else relative, date_time)
            info.create_system = 3
            mode = stat.S_IMODE(path.stat().st_mode)
            info.external_attr = ((stat.S_IFDIR if is_directory else stat.S_IFREG) | mode) << 16
            if is_directory:
                info.external_attr |= 0x10
                zip_file.writestr(info, b"")
            else:
                info.compress_type = zipfile.ZIP_DEFLATED
                zip_file.writestr(info, path.read_bytes())
else:
    raise SystemExit(f"unsupported archive format: {archive_format}")

digest = sha256(archive.read_bytes()).hexdigest()
checksum = archive.with_suffix(archive.suffix + ".sha256")
checksum.write_text(f"{digest}  {archive.name}\n", encoding="ascii")
if sha256(archive.read_bytes()).hexdigest() != digest:
    raise SystemExit("归档 SHA-256 校验失败")
if archive_format == "tar.gz":
    with tarfile.open(archive, mode="r:gz") as tar:
        if not tar.getmembers():
            raise SystemExit("归档为空")
else:
    with zipfile.ZipFile(archive) as zip_file:
        if not zip_file.infolist():
            raise SystemExit("归档为空")
PY

printf '%s\n' "$archive" "$archive.sha256"
