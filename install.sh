#!/bin/sh
# Installer for image-mcp: https://github.com/bkrajendra/image-mcp
#
# Install the latest release:
#   curl -fsSL https://raw.githubusercontent.com/bkrajendra/image-mcp/main/install.sh | sh
#
# Install a specific version:
#   curl -fsSL https://raw.githubusercontent.com/bkrajendra/image-mcp/main/install.sh | IMAGE_MCP_VERSION=v0.1.3 sh
#
# Uninstall:
#   curl -fsSL https://raw.githubusercontent.com/bkrajendra/image-mcp/main/install.sh | sh -s -- uninstall
#
# Supports Linux and macOS (x86_64 and arm64). Windows users should grab the
# .zip asset directly from https://github.com/bkrajendra/image-mcp/releases

set -eu

REPO="bkrajendra/image-mcp"
BIN_NAME="image-mcp"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${IMAGE_MCP_VERSION:-latest}"

info() {
  printf '%s: %s\n' "$BIN_NAME" "$1"
}

error() {
  printf '%s: error: %s\n' "$BIN_NAME" "$1" >&2
  exit 1
}

detect_target() {
  os=$(uname -s)
  arch=$(uname -m)

  case "$os" in
    Darwin) os_part="apple-darwin" ;;
    Linux) os_part="unknown-linux-gnu" ;;
    *) error "unsupported OS '$os'; prebuilt binaries exist for Linux and macOS only. On Windows, download the .zip from https://github.com/${REPO}/releases" ;;
  esac

  case "$arch" in
    x86_64 | amd64) arch_part="x86_64" ;;
    arm64 | aarch64) arch_part="aarch64" ;;
    *) error "unsupported architecture '$arch'" ;;
  esac

  printf '%s-%s\n' "$arch_part" "$os_part"
}

asset_url() {
  target="$1"
  asset="${BIN_NAME}-${target}.tar.gz"

  if [ "$VERSION" = "latest" ]; then
    printf 'https://github.com/%s/releases/latest/download/%s\n' "$REPO" "$asset"
  else
    printf 'https://github.com/%s/releases/download/%s/%s\n' "$REPO" "$VERSION" "$asset"
  fi
}

fetch() {
  url="$1"
  dest="$2"

  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$dest"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "$url" -O "$dest"
  else
    error "curl or wget is required to install ${BIN_NAME}"
  fi
}

do_install() {
  target=$(detect_target)
  url=$(asset_url "$target")

  tmp_dir=$(mktemp -d)
  trap 'rm -rf "$tmp_dir"' EXIT

  info "downloading ${url}"
  fetch "$url" "$tmp_dir/${BIN_NAME}.tar.gz" \
    || error "download failed - is there a published release for ${target}? See https://github.com/${REPO}/releases"

  tar -xzf "$tmp_dir/${BIN_NAME}.tar.gz" -C "$tmp_dir"
  [ -f "$tmp_dir/${BIN_NAME}" ] || error "downloaded archive did not contain a '${BIN_NAME}' binary"

  mkdir -p "$INSTALL_DIR"
  install_path="$INSTALL_DIR/${BIN_NAME}"
  cp "$tmp_dir/${BIN_NAME}" "$install_path"
  chmod +x "$install_path"

  info "installed to ${install_path}"

  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
      info "note: ${INSTALL_DIR} is not on your PATH"
      info "add this to your shell profile: export PATH=\"${INSTALL_DIR}:\$PATH\""
      ;;
  esac

  info "run '${BIN_NAME} --help' to get started"
}

do_uninstall() {
  removed=0

  for dir in "$INSTALL_DIR" "$HOME/.local/bin" "/usr/local/bin"; do
    candidate="$dir/${BIN_NAME}"
    if [ -f "$candidate" ]; then
      rm -f "$candidate"
      info "removed ${candidate}"
      removed=1
    fi
  done

  if [ "$removed" = "0" ]; then
    info "${BIN_NAME} was not found in ${INSTALL_DIR}, \$HOME/.local/bin, or /usr/local/bin"
  else
    info "${BIN_NAME} uninstalled"
  fi
}

usage() {
  cat <<EOF
Usage: install.sh [install|uninstall]

Environment variables:
  INSTALL_DIR         Directory to install into (default: \$HOME/.local/bin)
  IMAGE_MCP_VERSION    Release tag to install, e.g. v0.1.3 (default: latest)
EOF
}

action="${1:-install}"

case "$action" in
  install) do_install ;;
  uninstall) do_uninstall ;;
  -h | --help) usage ;;
  *) error "unknown argument '$action' (expected 'install' or 'uninstall')" ;;
esac
