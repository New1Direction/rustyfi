#!/bin/sh
# Install the `rustyfi` CLI from the latest GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/New1Direction/rustyfi/main/install.sh | sh
#
# Overrides:  RUSTYFI_REPO=owner/repo  RUSTYFI_VERSION=v0.1.0  RUSTYFI_BIN_DIR=~/bin
set -eu

REPO="${RUSTYFI_REPO:-New1Direction/rustyfi}"
BIN_DIR="${RUSTYFI_BIN_DIR:-$HOME/.local/bin}"

say() { printf '\033[1;33mrustyfi\033[0m %s\n' "$1"; }
err() { printf '\033[1;31merror:\033[0m %s\n' "$1" >&2; exit 1; }

os="$(uname -s)"
arch="$(uname -m)"
case "$arch" in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) err "unsupported architecture: $arch" ;;
esac
case "$os" in
  Linux) target="${arch}-unknown-linux-gnu" ;;
  Darwin) target="${arch}-apple-darwin" ;;
  *) err "unsupported OS: $os — try: cargo install --git https://github.com/$REPO rustyfi-cli" ;;
esac

if [ -n "${RUSTYFI_VERSION:-}" ]; then
  tag="$RUSTYFI_VERSION"
else
  tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | grep '"tag_name"' | head -1 | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
fi
[ -n "$tag" ] || err "could not resolve the latest release — set RUSTYFI_VERSION"

asset="rustyfi-${tag}-${target}.tar.gz"
url="https://github.com/$REPO/releases/download/$tag/$asset"
say "downloading $asset"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fsSL "$url" -o "$tmp/rustyfi.tar.gz" || err "download failed: $url"
tar -xzf "$tmp/rustyfi.tar.gz" -C "$tmp"
bin="$(find "$tmp" -type f -name rustyfi | head -1)"
[ -n "$bin" ] || err "rustyfi binary not found inside the archive"

mkdir -p "$BIN_DIR"
chmod +x "$bin"
mv "$bin" "$BIN_DIR/rustyfi"
say "installed $tag ($target) → $BIN_DIR/rustyfi"

case ":$PATH:" in
  *":$BIN_DIR:"*) : ;;
  *) say "add it to your PATH:  export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac
say "run:  rustyfi --help"
