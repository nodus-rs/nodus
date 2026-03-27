#!/usr/bin/env bash

set -euo pipefail

REPO_SLUG="nodus-rs/nodus"
BIN_NAME="nodus"
INSTALL_MARKER_NAME="${BIN_NAME}.install.json"
INSTALL_DIR="${NODUS_INSTALL_DIR:-}"
REQUESTED_VERSION="${NODUS_VERSION:-}"
VERIFY_CHECKSUMS=0
MODE="install"
TEMP_DIR=""
TARGET=""
ARCHIVE_EXT=""
EXECUTABLE_NAME="${BIN_NAME}"
PLATFORM="unix"

usage() {
  cat <<'EOF'
Install nodus from GitHub release assets.

Usage:
  ./install.sh [--version <tag>] [--install-dir <path>] [--verify]
  ./install.sh --uninstall [--install-dir <path>]

Options:
  --version <tag>       Install a specific release tag, for example v0.1.0.
  --install-dir <path>  Install the binary into this directory.
  --verify              Verify the downloaded archive with a release checksum when available.
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

json_escape() {
  printf '%s' "$1" | awk '
    BEGIN { ORS=""; print "\"" }
    {
      gsub(/\\/,"\\\\");
      gsub(/"/,"\\\"");
      gsub(/\t/,"\\t");
      gsub(/\r/,"\\r");
      gsub(/\n/,"\\n");
      print;
    }
    END { print "\"" }
  '
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
      --verify)
        VERIFY_CHECKSUMS=1
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

to_unix_path() {
  local path
  path="$1"

  if [ "${PLATFORM}" = "windows" ]; then
    command -v cygpath >/dev/null 2>&1 || fail "missing required command: cygpath"
    case "$path" in
      [A-Za-z]:\\*|[A-Za-z]:/*|\\\\*)
        cygpath -u "$path"
        return
        ;;
    esac
  fi

  printf '%s\n' "$path"
}

to_windows_path() {
  local path
  path="$1"

  if [ "${PLATFORM}" = "windows" ]; then
    command -v cygpath >/dev/null 2>&1 || fail "missing required command: cygpath"
    cygpath -w "$path"
    return
  fi

  printf '%s\n' "$path"
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
      EXECUTABLE_NAME="${BIN_NAME}"
      ;;
    Linux)
      case "$arch" in
        x86_64|amd64) TARGET="x86_64-unknown-linux-gnu" ;;
        aarch64|arm64) TARGET="aarch64-unknown-linux-gnu" ;;
        *) fail "unsupported Linux architecture: $arch" ;;
      esac
      ARCHIVE_EXT="tar.gz"
      EXECUTABLE_NAME="${BIN_NAME}"
      ;;
    CYGWIN*|MINGW*|MSYS*)
      case "$arch" in
        x86_64|amd64) TARGET="x86_64-pc-windows-msvc" ;;
        aarch64|arm64) TARGET="aarch64-pc-windows-msvc" ;;
        *) fail "unsupported Windows architecture: $arch" ;;
      esac
      ARCHIVE_EXT="zip"
      EXECUTABLE_NAME="${BIN_NAME}.exe"
      PLATFORM="windows"
      ;;
    *)
      fail "unsupported operating system: $os"
      ;;
  esac
}

resolve_install_dir() {
  local base_dir

  if [ -n "${INSTALL_DIR}" ]; then
    INSTALL_DIR="$(to_unix_path "${INSTALL_DIR}")"
    return
  fi

  if [ "${PLATFORM}" = "windows" ]; then
    base_dir="${LOCALAPPDATA:-${APPDATA:-}}"
    [ -n "${base_dir}" ] || fail "failed to determine the default local application data path on Windows"
    INSTALL_DIR="$(to_unix_path "${base_dir}")/Programs/nodus/bin"
    return
  fi

  INSTALL_DIR="${HOME}/.local/bin"
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
  local extracted_dir extracted_root source_bin installed_bin
  extracted_dir="$1"
  extracted_root="$2"
  source_bin="${extracted_dir}/${EXECUTABLE_NAME}"
  if [ ! -f "${source_bin}" ]; then
    source_bin="${extracted_root}/${EXECUTABLE_NAME}"
  fi
  installed_bin="${INSTALL_DIR}/${EXECUTABLE_NAME}"

  [ -f "${source_bin}" ] || fail "archive did not contain ${BIN_NAME}"
  mkdir -p "${INSTALL_DIR}"
  cp "${source_bin}" "${installed_bin}"
  if [ "${PLATFORM}" != "windows" ]; then
    chmod 755 "${installed_bin}"
  fi
}

install_marker_path() {
  printf '%s\n' "${INSTALL_DIR}/${INSTALL_MARKER_NAME}"
}

write_install_marker() {
  local marker_path installed_bin marker_bin
  marker_path="$(install_marker_path)"
  installed_bin="${INSTALL_DIR}/${EXECUTABLE_NAME}"
  marker_bin="$(to_windows_path "${installed_bin}")"
  cat > "${marker_path}" <<EOF
{
  "install_method": $(json_escape "github_release"),
  "repo_slug": $(json_escape "${REPO_SLUG}"),
  "binary_name": $(json_escape "${BIN_NAME}"),
  "binary_path": $(json_escape "${marker_bin}")
}
EOF
}

warn_if_not_on_path() {
  case ":$PATH:" in
    *:"${INSTALL_DIR}":*)
      ;;
    *)
      log "Installed to ${INSTALL_DIR}/${EXECUTABLE_NAME}"
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
  local installed_bin marker_path
  installed_bin="${INSTALL_DIR}/${EXECUTABLE_NAME}"
  marker_path="$(install_marker_path)"

  if [ ! -e "${installed_bin}" ] && [ ! -e "${marker_path}" ]; then
    log "${BIN_NAME} is not installed in ${INSTALL_DIR}"
    return
  fi

  rm -f "${installed_bin}"
  rm -f "${marker_path}"
  log "Removed ${installed_bin}"
}

main() {
  parse_args "$@"
  normalize_version

  need_cmd uname
  need_cmd mktemp
  need_cmd awk

  detect_target
  resolve_install_dir

  if [ "${MODE}" = "uninstall" ]; then
    uninstall_binary
    return
  fi

  case "${ARCHIVE_EXT}" in
    tar.gz) need_cmd tar ;;
    zip) need_cmd unzip ;;
  esac

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
  install_binary "${extracted_dir}" "${extracted_root}"
  write_install_marker
  warn_if_not_on_path
  log "Installed ${BIN_NAME} ${VERSION}"
  log "Run '${BIN_NAME} --help' to get started."
}

main "$@"
