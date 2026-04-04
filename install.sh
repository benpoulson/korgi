#!/bin/sh
set -e

REPO="benpoulson/korgi"
INSTALL_DIR="/usr/local/bin"

OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
  darwin) OS="macos" ;;
  linux)  OS="linux" ;;
  *) echo "Unsupported OS: $OS" >&2; exit 1 ;;
esac

case "$ARCH" in
  x86_64|amd64)  ARCH="amd64" ;;
  aarch64|arm64) ARCH="arm64" ;;
  *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

ARTIFACT="korgi-${OS}-${ARCH}"
URL="https://github.com/${REPO}/releases/latest/download/${ARTIFACT}.tar.gz"

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

echo "Downloading korgi for ${OS}/${ARCH}..."
curl -sL "$URL" | tar xz -C "$TMPDIR"

if [ -w "$INSTALL_DIR" ]; then
  mv "$TMPDIR/korgi" "$INSTALL_DIR/korgi"
else
  sudo mv "$TMPDIR/korgi" "$INSTALL_DIR/korgi"
fi

echo "Installed korgi to ${INSTALL_DIR}/korgi"
korgi --version
