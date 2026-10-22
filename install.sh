#!/bin/sh
# Install the wikiknit binary from a GitHub release: picks the archive for this machine,
# checks its SHA-256, and copies `wikiknit` into a bin folder.
#
#   curl -fsSL https://raw.githubusercontent.com/victor-kwak/wikiknit/main/install.sh | sh
#
# Environment: WIKIKNIT_VERSION (a tag such as v0.8.1; default: the latest release),
# WIKIKNIT_INSTALL_DIR (default: ~/.local/bin). Linux (x86_64, aarch64) and macOS (arm64,
# x86_64). On Windows, download the .zip from the releases page.
set -eu

repo="victor-kwak/wikiknit"
dir="${WIKIKNIT_INSTALL_DIR:-$HOME/.local/bin}"

fail() { echo "wikiknit install: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || fail "needs $1"; }
need curl
need tar
need uname

case "$(uname -s)" in
  Linux) os=unknown-linux-musl ;;
  Darwin) os=apple-darwin ;;
  *) fail "no prebuilt binary for $(uname -s); use: cargo install --locked --git https://github.com/$repo" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) fail "no prebuilt binary for $(uname -m); use: cargo install --locked --git https://github.com/$repo" ;;
esac
target="$arch-$os"

version="${WIKIKNIT_VERSION:-}"
if [ -z "$version" ]; then
  # The latest release's tag, from the redirect of /releases/latest (no API token needed).
  url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest") ||
    fail "cannot reach GitHub"
  version="${url##*/}"
  case "$version" in v*) ;; *) fail "no release found ($url)" ;; esac
fi

name="wikiknit-$version-$target"
base="https://github.com/$repo/releases/download/$version"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "wikiknit install: $name"
curl -fsSL "$base/$name.tar.gz" -o "$tmp/$name.tar.gz" || fail "no archive $name.tar.gz in release $version"
curl -fsSL "$base/$name.tar.gz.sha256" -o "$tmp/$name.tar.gz.sha256" || fail "no checksum for $name.tar.gz"

want=$(cut -d' ' -f1 < "$tmp/$name.tar.gz.sha256")
if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)
elif command -v shasum >/dev/null 2>&1; then
  got=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1)
else
  fail "needs sha256sum or shasum to check the download"
fi
[ "$want" = "$got" ] || fail "checksum mismatch for $name.tar.gz (expected $want, got $got)"

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$dir"
cp "$tmp/$name/wikiknit" "$dir/wikiknit.new"
chmod 755 "$dir/wikiknit.new"
mv -f "$dir/wikiknit.new" "$dir/wikiknit"
echo "wikiknit install: $("$dir/wikiknit" --version) in $dir"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "wikiknit install: $dir is not on PATH; add it, or the git hook cannot find wikiknit" >&2 ;;
esac
