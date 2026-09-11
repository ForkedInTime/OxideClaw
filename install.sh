#!/bin/bash
# OxideClaw installer — single-binary Claude Code alternative written in Rust
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/ForkedInTime/OxideClaw/main/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/ForkedInTime/OxideClaw/main/install.sh | bash -s v0.4.0
#
# Installs to ~/.local/bin/oxideclaw (or /usr/local/bin with --global)
# Supports Linux (x64, arm64, musl) and macOS (Intel, Apple Silicon)
# For Windows: download .exe from GitHub Releases or use `cargo install --path .`

set -e

REPO="ForkedInTime/OxideClaw"
VERSION="${1:-latest}"
INSTALL_DIR="${OXIDECLAW_INSTALL_DIR:-${RUSTYCLAW_INSTALL_DIR:-$HOME/.local/bin}}"
GLOBAL=false

# Parse flags
for arg in "$@"; do
  case "$arg" in
    --global) GLOBAL=true; INSTALL_DIR="/usr/local/bin" ;;
    v*) VERSION="$arg" ;;
  esac
done

# ── Detect platform ──────────────────────────────────────────────────────────

case "$(uname -s)" in
  Linux)  os="linux" ;;
  Darwin) os="macos" ;;
  MINGW*|MSYS*|CYGWIN*)
    echo "On Windows, download the .exe directly from:"
    echo "  https://github.com/${REPO}/releases/latest"
    echo "Or build from source: cargo install --path ."
    exit 1
    ;;
  *) echo "Unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  x86_64|amd64)   arch="x64" ;;
  aarch64|arm64)   arch="arm64" ;;
  *)               echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

# Detect musl vs glibc (Linux only)
if [ "$os" = "linux" ]; then
  if ldd --version 2>&1 | grep -qi musl || [ -f /lib/libc.musl-*.so.1 ] 2>/dev/null; then
    platform="linux-${arch}-musl"
  else
    platform="linux-${arch}"
  fi
else
  platform="macos-${arch}"
fi

ARTIFACT="oxideclaw-${platform}"

# ── Resolve version ──────────────────────────────────────────────────────────

if [ "$VERSION" = "latest" ]; then
  echo "Fetching latest release..."
  VERSION=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' | head -1 | cut -d'"' -f4)
  if [ -z "$VERSION" ]; then
    echo "Could not determine latest version. Specify one: $0 v0.1.0" >&2
    exit 1
  fi
fi

echo "Installing OxideClaw ${VERSION} (${platform})..."

# ── Download ─────────────────────────────────────────────────────────────────

DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARTIFACT}"
CHECKSUM_URL="${DOWNLOAD_URL}.sha256"

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

curl -fsSL -o "${TMPDIR}/oxideclaw"    "$DOWNLOAD_URL"
curl -fsSL -o "${TMPDIR}/checksum.txt" "$CHECKSUM_URL"

# ── Verify checksum ──────────────────────────────────────────────────────────

EXPECTED=$(cut -d' ' -f1 "${TMPDIR}/checksum.txt")
if command -v sha256sum &>/dev/null; then
  ACTUAL=$(sha256sum "${TMPDIR}/oxideclaw" | cut -d' ' -f1)
elif command -v shasum &>/dev/null; then
  ACTUAL=$(shasum -a 256 "${TMPDIR}/oxideclaw" | cut -d' ' -f1)
else
  echo "Warning: no sha256sum or shasum found — skipping checksum verification"
  ACTUAL="$EXPECTED"
fi

if [ "$EXPECTED" != "$ACTUAL" ]; then
  echo "Checksum verification FAILED" >&2
  echo "  expected: $EXPECTED" >&2
  echo "  got:      $ACTUAL" >&2
  exit 1
fi
echo "Checksum verified."

# ── Install ──────────────────────────────────────────────────────────────────

mkdir -p "$INSTALL_DIR"
if [ "$GLOBAL" = true ]; then
  sudo install -m 755 "${TMPDIR}/oxideclaw" "${INSTALL_DIR}/oxideclaw"
else
  install -m 755 "${TMPDIR}/oxideclaw" "${INSTALL_DIR}/oxideclaw"
fi

# ── Ensure PATH includes install dir ─────────────────────────────────────────

if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
  SHELL_NAME=$(basename "$SHELL")
  case "$SHELL_NAME" in
    zsh)  RC="$HOME/.zshrc" ;;
    bash) RC="$HOME/.bashrc" ;;
    fish) RC="$HOME/.config/fish/config.fish" ;;
    *)    RC="" ;;
  esac
  if [ -n "$RC" ]; then
    if [ "$SHELL_NAME" = "fish" ]; then
      echo "set -gx PATH $INSTALL_DIR \$PATH" >> "$RC"
    else
      echo "export PATH=\"$INSTALL_DIR:\$PATH\"" >> "$RC"
    fi
    echo "Added $INSTALL_DIR to PATH in $RC"
    echo "Run: source $RC  (or open a new terminal)"
  else
    echo "Add $INSTALL_DIR to your PATH manually."
  fi
fi

echo ""
echo "  OxideClaw ${VERSION} installed to ${INSTALL_DIR}/oxideclaw"
echo ""
echo "  Run:  oxideclaw"
echo ""
