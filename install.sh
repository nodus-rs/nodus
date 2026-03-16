#!/usr/bin/env bash

set -euo pipefail

REPO_SLUG="WendellXY/nodus"
BIN_NAME="nodus"
INSTALL_DIR="${NODUS_INSTALL_DIR:-$HOME/.local/bin}"
REQUESTED_VERSION="${NODUS_VERSION:-}"
VERIFY_CHECKSUMS=1
MODE="install"
TEMP_DIR=""

usage() {
  cat <<'EOF'
Install nodus from GitHub release assets.

Usage:
  ./install.sh [--version <tag>] [--install-dir <path>] [--no-verify]
  ./install.sh --uninstall [--install-dir <path>]

Options:
  --version <tag>       Install a specific release tag, for example v0.1.0.
  --install-dir <path>  Install the binary into this directory.
  --no-verify           Skip SHA-256 checksum verification.
  --uninstall           Remove the installed binary from the install directory.
  -h, --help            Show this help text.

Environment:
  NODUS_VERSION         Same as --version.
  NODUS_INSTALL_DIR     Same as --install-dir.
EOF
}

log() {
  printf '%s\n' "$*"
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --version)
        [ "$#" -ge 2 ] || fail "--version requires a value"
        REQUESTED_VERSION="$2"
        shift 2
        ;;
      --install-dir)
        [ "$#" -ge 2 ] || fail "--install-dir requires a value"
        INSTALL_DIR="$2"
        shift 2
        ;;
      --no-verify)
        VERIFY_CHECKSUMS=0
        shift
        ;;
      --uninstall)
        MODE="uninstall"
        shift
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        fail "unknown argument: $1"
        ;;
    esac
  done
}

normalize_version() {
  if [ -n "${REQUESTED_VERSION}" ] && [ "${REQUESTED_VERSION#v}" = "${REQUESTED_VERSION}" ]; then
    REQUESTED_VERSION="v${REQUESTED_VERSION}"
  fi
}

detect_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Darwin)
      case "$arch" in
        arm64) TARGET="aarch64-apple-darwin" ;;
        x86_64) TARGET="x86_64-apple-darwin" ;;
        *) fail "unsupported macOS architecture: $arch" ;;
      esac
      ARCHIVE_EXT="tar.gz"
      ;;
    Linux)
      case "$arch" in
        x86_64|amd64) TARGET="x86_64-unknown-linux-gnu" ;;
        *) fail "unsupported Linux architecture: $arch" ;;
      esac
      ARCHIVE_EXT="tar.gz"
      ;;
    *)
      fail "unsupported operating system: $os"
      ;;
  esac
}

resolve_version() {
  if [ -n "${REQUESTED_VERSION}" ]; then
    VERSION="${REQUESTED_VERSION}"
    return
  fi

  local latest_url
  need_cmd curl
  latest_url="$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/${REPO_SLUG}/releases/latest")"
  VERSION="${latest_url##*/}"
  [ -n "${VERSION}" ] || fail "could not determine the latest release tag"
}

download() {
  local url output
  url="$1"
  output="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$output"
    return
  fi
  if command -v wget >/dev/null 2>&1; then
    wget -q "$url" -O "$output"
    return
  fi
  fail "missing required command: curl or wget"
}

checksum_cmd() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
    return
  fi
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
    return
  fi
  fail "missing required command: sha256sum or shasum"
}

verify_checksum() {
  local archive checksum_file expected actual
  archive="$1"
  checksum_file="$2"

  expected="$(awk '{print $1}' "$checksum_file")"
  [ -n "${expected}" ] || fail "checksum file did not contain a hash"
  actual="$(checksum_cmd "$archive")"
  [ "${actual}" = "${expected}" ] || fail "checksum verification failed for ${ASSET_NAME}"
}

extract_archive() {
  local archive destination
  archive="$1"
  destination="$2"

  case "${ARCHIVE_EXT}" in
    tar.gz)
      tar -xzf "$archive" -C "$destination"
      ;;
    zip)
      need_cmd unzip
      unzip -q "$archive" -d "$destination"
      ;;
    *)
      fail "unsupported archive type: ${ARCHIVE_EXT}"
      ;;
  esac
}

install_binary() {
  local extracted_dir source_bin
  extracted_dir="$1"
  source_bin="${extracted_dir}/${BIN_NAME}"

  [ -f "${source_bin}" ] || fail "archive did not contain ${BIN_NAME}"
  mkdir -p "${INSTALL_DIR}"
  install -m 755 "${source_bin}" "${INSTALL_DIR}/${BIN_NAME}"
}

warn_if_not_on_path() {
  case ":$PATH:" in
    *:"${INSTALL_DIR}":*)
      ;;
    *)
      log "Installed to ${INSTALL_DIR}/${BIN_NAME}"
      log "Add ${INSTALL_DIR} to your PATH to run ${BIN_NAME} directly."
      ;;
  esac
}

cleanup() {
  if [ -n "${TEMP_DIR}" ] && [ -d "${TEMP_DIR}" ]; then
    rm -rf "${TEMP_DIR}"
  fi
}

uninstall_binary() {
  local installed_bin
  installed_bin="${INSTALL_DIR}/${BIN_NAME}"

  if [ ! -e "${installed_bin}" ]; then
    log "${BIN_NAME} is not installed in ${INSTALL_DIR}"
    return
  fi

  rm -f "${installed_bin}"
  log "Removed ${installed_bin}"
}

main() {
  parse_args "$@"
  normalize_version

  if [ "${MODE}" = "uninstall" ]; then
    uninstall_binary
    return
  fi

  need_cmd uname
  need_cmd mktemp
  need_cmd tar
  need_cmd awk
  need_cmd install

  detect_target
  resolve_version

  ASSET_NAME="${BIN_NAME}-${VERSION}-${TARGET}.${ARCHIVE_EXT}"
  CHECKSUM_NAME="${ASSET_NAME}.sha256"
  ASSET_URL="https://github.com/${REPO_SLUG}/releases/download/${VERSION}/${ASSET_NAME}"
  CHECKSUM_URL="https://github.com/${REPO_SLUG}/releases/download/${VERSION}/${CHECKSUM_NAME}"

  local archive_path checksum_path extracted_root extracted_dir
  TEMP_DIR="$(mktemp -d)"
  trap cleanup EXIT

  archive_path="${TEMP_DIR}/${ASSET_NAME}"
  checksum_path="${TEMP_DIR}/${CHECKSUM_NAME}"
  extracted_root="${TEMP_DIR}/extract"
  extracted_dir="${extracted_root}/${BIN_NAME}-${VERSION}-${TARGET}"

  log "Downloading ${ASSET_NAME}"
  download "${ASSET_URL}" "${archive_path}"

  if [ "${VERIFY_CHECKSUMS}" -eq 1 ]; then
    if download "${CHECKSUM_URL}" "${checksum_path}"; then
      log "Verifying download"
      verify_checksum "${archive_path}" "${checksum_path}"
    else
      log "Checksum unavailable for ${VERSION}; continuing without verification."
    fi
  fi

  mkdir -p "${extracted_root}"
  extract_archive "${archive_path}" "${extracted_root}"
  install_binary "${extracted_dir}"
  warn_if_not_on_path
  log "Installed ${BIN_NAME} ${VERSION}"
  log "Run '${BIN_NAME} --help' to get started."
}

main "$@"
