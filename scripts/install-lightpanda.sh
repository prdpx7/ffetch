#!/usr/bin/env bash
set -euo pipefail

VERSION="nightly"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

detect_platform() {
    local os arch
    os="$(uname -s | tr '[:upper:]' '[:lower:]')"
    arch="$(uname -m)"

    case "$arch" in
        x86_64|amd64) arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *) echo "unsupported arch: $arch" >&2; exit 1 ;;
    esac

    case "$os" in
        linux) os="linux" ;;
        darwin) os="macos" ;;
        *) echo "unsupported os: $os" >&2; exit 1 ;;
    esac

    echo "${arch}-${os}"
}

PLATFORM="$(detect_platform)"
BINARY_NAME="lightpanda-${PLATFORM}"
URL="https://github.com/lightpanda-io/browser/releases/download/${VERSION}/${BINARY_NAME}"

echo "downloading lightpanda (${PLATFORM})..."
mkdir -p "$INSTALL_DIR"
curl -fSL "$URL" -o "${INSTALL_DIR}/lightpanda"
chmod +x "${INSTALL_DIR}/lightpanda"

echo "installed lightpanda to ${INSTALL_DIR}/lightpanda"
"${INSTALL_DIR}/lightpanda" --version 2>/dev/null || true
