#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/ghis-package-test.XXXXXX")
trap 'rm -r -- "$tmpdir"' EXIT HUP INT TERM

project_dir="$tmpdir/project"
tool_dir="$tmpdir/tools"
release_version=0.3.0
release_sha256=1545b2bc52a646491cbab6879c955e896deb57902d93b15d46e9d3accfc574b7
mkdir -p "$project_dir/scripts" "$project_dir/packaging/arch" "$tool_dir"
cp "$script_dir/package-release.sh" "$project_dir/scripts/package-release.sh"
cp "$script_dir/../packaging/arch/PKGBUILD" "$project_dir/packaging/arch/PKGBUILD"
[ "$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$script_dir/../Cargo.toml" | head -n 1)" = "$release_version" ]
printf '%s\n' '[package]' 'name = "ghis"' "version = \"$release_version\"" > "$project_dir/Cargo.toml"
printf '%s\n' 'Test release notes.' > "$project_dir/README.md"
printf '%s\n' 'Test license.' > "$project_dir/LICENSE"

cat > "$tool_dir/rustc" <<'EOF'
#!/bin/sh
printf '%s\n' 'rustc 1.97.1' 'host: x86_64-unknown-linux-gnu'
EOF
chmod +x "$tool_dir/rustc"

cat > "$tool_dir/cargo" <<'EOF'
#!/bin/sh
set -eu

target=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --target)
      target=$2
      shift 2
      ;;
    *) shift ;;
  esac
done

[ -n "$target" ]
target_dir=${CARGO_TARGET_DIR:-target}
binary_name=ghis
case "$target" in
  *-windows-*) binary_name=ghis.exe ;;
esac
mkdir -p "$target_dir/$target/release"
if [ "$binary_name" = ghis ]; then
  cat > "$target_dir/$target/release/$binary_name" <<'SCRIPT'
#!/bin/sh
[ "$1" = completion ]
case "$2" in
  zsh) printf '%s\n' '#compdef ghis' '_ghis() { :; }' ;;
  bash) printf '%s\n' '_ghis() { :; }' 'complete -F _ghis ghis' ;;
  fish) printf '%s\n' 'complete -c ghis' ;;
  powershell) printf '%s\n' "Register-ArgumentCompleter -CommandName 'ghis' -ScriptBlock { }" ;;
  *) exit 2 ;;
esac
SCRIPT
else
  printf '%s\n' 'cross-target binary' > "$target_dir/$target/release/$binary_name"
fi
chmod +x "$target_dir/$target/release/$binary_name"
EOF
chmod +x "$tool_dir/cargo"

cat > "$tool_dir/zsh" <<'EOF'
#!/bin/sh
[ "$1" = -n ] && [ -f "$2" ]
EOF
chmod +x "$tool_dir/zsh"

run_package() {
  PATH="$tool_dir:$PATH" \
    SOURCE_DATE_EPOCH=1700000000 \
    GHIS_DIST_DIR="$project_dir/dist" \
    "$project_dir/scripts/package-release.sh" "$@"
}

assert_windows_archive() {
  archive=$1
  target=$2
  python3 - "$archive" "$target" <<'PY'
from hashlib import sha256
from pathlib import Path
import sys
import zipfile

archive = Path(sys.argv[1])
target = sys.argv[2]
package = f"ghis-v0.3.0-{target}"
with zipfile.ZipFile(archive) as zip_file:
    names = set(zip_file.namelist())
    expected = {
        f"{package}/",
        f"{package}/ghis.exe",
        f"{package}/completions/",
        f"{package}/completions/_ghis",
        f"{package}/completions/ghis.bash",
        f"{package}/completions/ghis.fish",
        f"{package}/completions/ghis.ps1",
        f"{package}/README.md",
        f"{package}/LICENSE",
    }
    if names != expected:
        raise SystemExit(f"unexpected ZIP contents: {sorted(names)}")
    if zip_file.getinfo(f"{package}/ghis.exe").date_time != (2023, 11, 14, 22, 13, 20):
        raise SystemExit("ZIP timestamp is not reproducible")
checksum = archive.with_suffix(archive.suffix + ".sha256").read_text(encoding="ascii")
if checksum != f"{sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n":
    raise SystemExit("ZIP checksum does not match")
PY
}

assert_arch_pkgbuild() {
  archive=$1
  srcdir="$tmpdir/arch-src"
  pkgdir="$tmpdir/arch-pkg"
  mkdir -p "$srcdir" "$pkgdir"
  tar -xzf "$archive" -C "$srcdir"

  bash -s -- "$project_dir/packaging/arch" "$srcdir" "$pkgdir" "$archive" "$release_version" "$release_sha256" <<'BASH'
set -eu

startdir=$1
srcdir=$2
pkgdir=$3
archive=$4
expected_version=$5
expected_sha256=$6
. "$startdir/PKGBUILD"

[ "$pkgname" = ghis ]
[ "$pkgver" = "$expected_version" ]
[ "${#arch[@]}" -eq 1 ]
[ "${arch[0]}" = x86_64 ]
[ "${#source[@]}" -eq 1 ]
expected_source="file://${startdir}/../../dist/ghis-v${pkgver}-x86_64-unknown-linux-gnu.tar.gz"
[ "${source[0]}" = "$expected_source" ]
[ "${#sha256sums[@]}" -eq 1 ]
[ "${sha256sums[0]}" = "$expected_sha256" ]
[ "$(readlink -f "${source[0]#file://}")" = "$(readlink -f "$archive")" ]

package
for path in \
  usr/bin/ghis \
  usr/share/zsh/site-functions/_ghis \
  usr/share/doc/ghis/README.md \
  usr/share/licenses/ghis/LICENSE; do
  [ -f "$pkgdir/$path" ]
done
[ -x "$pkgdir/usr/bin/ghis" ]
# The Arch package declares zsh as its supported completion integration. The
# release archive carries all renderers, but PKGBUILD must not accidentally
# install an unowned Bash, Fish, or PowerShell completion.
for path in \
  usr/share/bash-completion/completions/ghis \
  usr/share/fish/vendor_completions.d/ghis.fish \
  usr/share/powershell/Modules/ghis/ghis.ps1; do
  [ ! -e "$pkgdir/$path" ]
done
cmp "$srcdir/ghis-v${pkgver}-x86_64-unknown-linux-gnu/ghis" "$pkgdir/usr/bin/ghis"
cmp "$srcdir/ghis-v${pkgver}-x86_64-unknown-linux-gnu/completions/_ghis" \
  "$pkgdir/usr/share/zsh/site-functions/_ghis"
cmp "$srcdir/ghis-v${pkgver}-x86_64-unknown-linux-gnu/README.md" \
  "$pkgdir/usr/share/doc/ghis/README.md"
cmp "$srcdir/ghis-v${pkgver}-x86_64-unknown-linux-gnu/LICENSE" \
  "$pkgdir/usr/share/licenses/ghis/LICENSE"
BASH
}

for windows_target in aarch64-pc-windows-msvc x86_64-pc-windows-gnu; do
  windows_archive="$project_dir/dist/ghis-v0.3.0-$windows_target.zip"
  if [ "$windows_target" = x86_64-pc-windows-gnu ]; then
    custom_target_dir="$tmpdir/custom-target"
    CARGO_TARGET_DIR="$custom_target_dir" run_package --target "$windows_target" >/dev/null
    [ -x "$custom_target_dir/$windows_target/release/ghis.exe" ]
    [ -x "$custom_target_dir/x86_64-unknown-linux-gnu/release/ghis" ]
  else
    run_package --target "$windows_target" >/dev/null
  fi
  cp "$windows_archive" "$tmpdir/first-windows-$windows_target.zip"
  if [ "$windows_target" = x86_64-pc-windows-gnu ]; then
    CARGO_TARGET_DIR="$custom_target_dir" run_package --target "$windows_target" >/dev/null
  else
    run_package --target "$windows_target" >/dev/null
  fi
  cmp "$tmpdir/first-windows-$windows_target.zip" "$windows_archive"
  assert_windows_archive "$windows_archive" "$windows_target"
done

unix_target=x86_64-unknown-linux-gnu
unix_archive="$project_dir/dist/ghis-v0.3.0-$unix_target.tar.gz"
run_package >/dev/null
cp "$unix_archive" "$tmpdir/first-unix.tar.gz"
run_package >/dev/null
cmp "$tmpdir/first-unix.tar.gz" "$unix_archive"
python3 - "$unix_archive" <<'PY'
from hashlib import sha256
from pathlib import Path
import sys
import tarfile

archive = Path(sys.argv[1])
package = "ghis-v0.3.0-x86_64-unknown-linux-gnu"
with tarfile.open(archive, mode="r:gz") as tar:
    members = {member.name: member for member in tar.getmembers()}
    expected = {
        package,
        f"{package}/ghis",
        f"{package}/completions",
        f"{package}/completions/_ghis",
        f"{package}/completions/ghis.bash",
        f"{package}/completions/ghis.fish",
        f"{package}/completions/ghis.ps1",
        f"{package}/README.md",
        f"{package}/LICENSE",
    }
    if set(members) != expected:
        raise SystemExit(f"unexpected tar contents: {sorted(members)}")
    if any(member.mtime != 1700000000 for member in members.values()):
        raise SystemExit("tar timestamp is not reproducible")
checksum = archive.with_suffix(archive.suffix + ".sha256").read_text(encoding="ascii")
if checksum != f"{sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n":
    raise SystemExit("tar checksum does not match")
PY
if [ "$(uname -s)" = Linux ]; then
  assert_arch_pkgbuild "$unix_archive"
else
  printf '%s\n' 'skipping Arch PKGBUILD install test outside Linux' >&2
fi

printf '%s\n' 'package release smoke tests passed'
