#!/usr/bin/env bash
# Builds the Windows release binary and packages it into a portable,
# double-click-to-install zip.
#
# Usage: scripts/package-windows.sh [output-dir]     (default: dist/)
#
# Produces <output-dir>/iris-<version>-windows-x64.zip containing:
#   iris.exe       - the app, with the prism icon and version metadata embedded
#   install.ps1    - per-user installer: Start Menu shortcut, optional
#                     Desktop shortcut / run-at-login
#   README.md      - packaging/windows/README.md: the install-only walkthrough
#                     install.ps1 points at. Deliberately not the repo README,
#                     whose links and "build the zip first" step are dead
#                     inside the zip.
#   LICENSE        - the licence the shipped binary is under
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

# Asked of cargo rather than assumed to be ./target: both CARGO_TARGET_DIR and
# `build.target-dir` move it, which WSL setups do routinely to get the build
# off the 9p mount. Guessing wrong reports "the build produced no exe" when
# the build was fine.
target_dir="$(cargo metadata --format-version 1 --no-deps \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
exe="$target_dir/x86_64-pc-windows-gnu/release/iris.exe"
if [[ ! -f "$exe" ]]; then
    echo "error: $exe was not produced by the build" >&2
    exit 1
fi

# From cargo, not from a grep of Cargo.toml: this is the same value the build
# stamped into the exe as CARGO_PKG_VERSION, and a parse failure here is loud
# rather than a silently mis-named zip.
version="$(cargo pkgid -p iris-app | sed -E 's/.*[#@]([0-9][^ ]*)$/\1/')"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+ ]]; then
    echo "error: could not read the iris-app version from cargo (got '$version')" >&2
    exit 1
fi

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

cp "$exe" "$stage/iris.exe"
cp "packaging/windows/install.ps1" "$stage/install.ps1"
cp "packaging/windows/README.md" "$stage/README.md"
cp "LICENSE" "$stage/LICENSE"

zip_path="$out_dir/iris-${version}-windows-x64.zip"
rm -f "$zip_path"
( cd "$stage" && python3 -m zipfile -c "$zip_path" iris.exe install.ps1 README.md LICENSE )

echo "Wrote $zip_path"
