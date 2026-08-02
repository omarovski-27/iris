#!/usr/bin/env bash
# Builds the Windows release binary and packages it into a portable,
# double-click-to-install zip.
#
# Usage: scripts/package-windows.sh [output-dir]     (default: dist/)
#
# Produces <output-dir>/iris-<version>-windows-x64.zip containing:
#   iris.exe       - the app, with the prism icon and version metadata embedded
#   install.ps1    - per-user installer: Start Menu shortcut, optional
#                     Desktop shortcut / run-at-login (see README.md INSTALL)
#
# No MSVC, no Windows SDK, no zip package install: cross-compiles with the
# mingw-w64 toolchain already required to link this target
# (docs/dev-windows.md), and archives with `python3 -m zipfile`.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

out_dir="${1:-dist}"
mkdir -p "$out_dir"
out_dir="$(cd "$out_dir" && pwd)"

cargo build --release --target x86_64-pc-windows-gnu -p iris-app

exe="target/x86_64-pc-windows-gnu/release/iris.exe"
if [[ ! -f "$exe" ]]; then
    echo "error: $exe was not produced by the build" >&2
    exit 1
fi

version="$(grep -m1 '^version' Cargo.toml | sed -E 's/version = "(.*)"/\1/')"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

cp "$exe" "$stage/iris.exe"
cp "packaging/windows/install.ps1" "$stage/install.ps1"

zip_path="$out_dir/iris-${version}-windows-x64.zip"
rm -f "$zip_path"
( cd "$stage" && python3 -m zipfile -c "$zip_path" iris.exe install.ps1 )

echo "Wrote $zip_path"
