#!/usr/bin/env bash
set -euo pipefail

REPO="${VAULT_REPO:-adannogueira/vault-axi}"
INSTALL_DIR="${VAULT_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${VAULT_VERSION:-latest}"

info() {
  printf "\033[1;34m==>\033[0m %s\n" "$*"
}

success() {
  printf "\033[1;32m==>\033[0m %s\n" "$*"
}

err() {
  printf "\033[1;31merror:\033[0m %s\n" "$*" >&2
  exit 1
}

# 1. Detect OS and architecture
OS_RAW="$(uname -s)"
ARCH_RAW="$(uname -m)"

case "$OS_RAW" in
  Linux*)
    OS="linux"
    BIN_NAME="vault"
    ;;
  Darwin*)
    OS="macos"
    BIN_NAME="vault"
    ;;
  MINGW*|MSYS*|CYGWIN*)
    OS="windows"
    BIN_NAME="vault.exe"
    ;;
  *)
    err "Unsupported operating system: $OS_RAW"
    ;;
esac

case "$ARCH_RAW" in
  x86_64|amd64)
    ARCH="x86_64"
    ;;
  aarch64|arm64)
    ARCH="aarch64"
    ;;
  *)
    err "Unsupported CPU architecture: $ARCH_RAW"
    ;;
esac

if [ "$OS" = "windows" ]; then
  if [ "$ARCH" != "x86_64" ]; then
    err "Prebuilt Windows binaries are currently only available for x86_64 (detected $ARCH)."
  fi
  ASSET_NAME="vault-windows-x86_64.exe"
else
  ASSET_NAME="vault-${OS}-${ARCH}"
fi

# 2. Build download URLs
if [ "$VERSION" = "latest" ]; then
  BASE_URL="https://github.com/${REPO}/releases/latest/download"
else
  BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
fi

BIN_URL="${BASE_URL}/${ASSET_NAME}"
SHA_URL="${BASE_URL}/${ASSET_NAME}.sha256"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

DOWNLOADED=0
info "Downloading ${ASSET_NAME} from ${REPO} (${VERSION})..."
if command -v curl >/dev/null 2>&1; then
  if curl -fsSL "$BIN_URL" -o "${TMP_DIR}/${ASSET_NAME}" 2>/dev/null; then
    DOWNLOADED=1
    curl -fsSL "$SHA_URL" -o "${TMP_DIR}/${ASSET_NAME}.sha256" 2>/dev/null || true
  fi
elif command -v wget >/dev/null 2>&1; then
  if wget -qO "${TMP_DIR}/${ASSET_NAME}" "$BIN_URL" 2>/dev/null; then
    DOWNLOADED=1
    wget -qO "${TMP_DIR}/${ASSET_NAME}.sha256" "$SHA_URL" 2>/dev/null || true
  fi
else
  err "Neither 'curl' nor 'wget' was found. Please install curl first."
fi

mkdir -p "$INSTALL_DIR"
DEST_PATH="${INSTALL_DIR}/${BIN_NAME}"

if [ "$DOWNLOADED" -eq 1 ]; then
  # 3. Verify SHA-256 checksum if available
  if [ -s "${TMP_DIR}/${ASSET_NAME}.sha256" ]; then
    EXPECTED_SHA="$(awk '{print $1}' "${TMP_DIR}/${ASSET_NAME}.sha256" | tr -d '\r\n')"
    ACTUAL_SHA=""
    if command -v sha256sum >/dev/null 2>&1; then
      ACTUAL_SHA="$(sha256sum "${TMP_DIR}/${ASSET_NAME}" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
      ACTUAL_SHA="$(shasum -a 256 "${TMP_DIR}/${ASSET_NAME}" | awk '{print $1}')"
    fi
    if [ -n "$ACTUAL_SHA" ] && [ -n "$EXPECTED_SHA" ]; then
      if [ "$ACTUAL_SHA" != "$EXPECTED_SHA" ]; then
        err "SHA-256 checksum mismatch! Expected ${EXPECTED_SHA}, got ${ACTUAL_SHA}"
      fi
      info "Verified SHA-256 checksum."
    fi
  fi

  # 4. Install binary
  mv "${TMP_DIR}/${ASSET_NAME}" "$DEST_PATH"
  chmod +x "$DEST_PATH"
else
  info "Prebuilt binary not found at ${BIN_URL}; building from source..."
  if ! command -v cargo >/dev/null 2>&1; then
    if [ -f "$HOME/.cargo/env" ]; then
      # shellcheck disable=SC1091
      . "$HOME/.cargo/env"
    fi
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    info "Installing Rust toolchain via rustup (minimal profile)..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
  CARGO_INSTALL_ROOT="${TMP_DIR}/cargo-root"
  if [ "$VERSION" = "latest" ]; then
    cargo install --git "https://github.com/${REPO}.git" --locked --root "$CARGO_INSTALL_ROOT"
  else
    cargo install --git "https://github.com/${REPO}.git" --tag "$VERSION" --locked --root "$CARGO_INSTALL_ROOT"
  fi
  mv "${CARGO_INSTALL_ROOT}/bin/${BIN_NAME}" "$DEST_PATH"
  chmod +x "$DEST_PATH"
fi

# Remove macOS quarantine attribute if present
if [ "$OS" = "macos" ] && command -v xattr >/dev/null 2>&1; then
  xattr -d com.apple.quarantine "$DEST_PATH" 2>/dev/null || true
fi

success "Installed vault to ${DEST_PATH}"

# 5. Ensure INSTALL_DIR is on PATH
add_to_profile() {
  local profile_file="$1"
  local export_line="$2"
  if [ -f "$profile_file" ] && grep -Fq "$INSTALL_DIR" "$profile_file"; then
    return 0
  fi
  mkdir -p "$(dirname "$profile_file")"
  printf "\n# vault-axi CLI\n%s\n" "$export_line" >> "$profile_file"
  info "Added ${INSTALL_DIR} to PATH in ${profile_file}"
}

case ":${PATH}:" in
  *":${INSTALL_DIR}:"*)
    ;;
  *)
    SHELL_NAME="$(basename "${SHELL:-bash}")"
    EXPORT_LINE="export PATH=\"${INSTALL_DIR}:\$PATH\""
    case "$SHELL_NAME" in
      zsh)
        add_to_profile "$HOME/.zshrc" "$EXPORT_LINE"
        ;;
      bash)
        if [ "$OS" = "macos" ] && [ -f "$HOME/.bash_profile" ]; then
          add_to_profile "$HOME/.bash_profile" "$EXPORT_LINE"
        else
          add_to_profile "$HOME/.bashrc" "$EXPORT_LINE"
        fi
        ;;
      fish)
        add_to_profile "$HOME/.config/fish/config.fish" "fish_add_path \"${INSTALL_DIR}\""
        ;;
      *)
        add_to_profile "$HOME/.profile" "$EXPORT_LINE"
        ;;
    esac
    printf "\nRun \033[1mexport PATH=\"%s:\$PATH\"\033[0m or restart your terminal to use 'vault' immediately.\n" "$INSTALL_DIR"
    ;;
esac

printf "\nGet started:\n  vault register --email you@example.com\n  vault --help\n"
