#!/bin/bash

set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This script must be run on macOS."
  exit 1
fi

SCRIPT_DIR="$(cd -- "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
TAURI_CONF="${REPO_ROOT}/src-tauri/tauri.conf.json"

read_tauri_value() {
  local key="$1"
  python3 - "$TAURI_CONF" "$key" <<'PY'
import json
import sys

config_path, key = sys.argv[1], sys.argv[2]
with open(config_path, "r", encoding="utf-8") as f:
    data = json.load(f)
print(data[key])
PY
}

DEFAULT_PRODUCT_NAME="$(read_tauri_value "productName")"
DEFAULT_VERSION="$(read_tauri_value "version")"

detect_arch_suffix() {
  local target="${TARGET:-}"
  local machine
  machine="$(uname -m)"

  case "${target}:${machine}" in
    aarch64-apple-darwin:*|*:arm64|*:aarch64)
      printf '%s\n' "aarch64"
      ;;
    x86_64-apple-darwin:*|*:x86_64|*:amd64)
      printf '%s\n' "x64"
      ;;
    *)
      printf '%s\n' "${machine}"
      ;;
  esac
}

PRODUCT_NAME="${PRODUCT_NAME:-${DEFAULT_PRODUCT_NAME}}"
APP_VERSION="${APP_VERSION:-${DEFAULT_VERSION}}"
APP_NAME="${APP_NAME:-${PRODUCT_NAME}.app}"
PRODUCT_BASENAME="${PRODUCT_BASENAME:-${PRODUCT_NAME// /_}}"
ARCH_SUFFIX="${ARCH_SUFFIX:-$(detect_arch_suffix)}"
TEMP_ROOT="${REPO_ROOT}/.tmp/dmg"
STAGE_DIR="${TEMP_ROOT}/${PRODUCT_NAME}"
PACKAGING_DIR="${REPO_ROOT}/packaging/macos"
OUTPUT_DIR="${REPO_ROOT}/src-tauri/target/release/bundle/dmg"
OUTPUT_DMG="${OUTPUT_DIR}/${PRODUCT_BASENAME}_${APP_VERSION}_${ARCH_SUFFIX}.dmg"
TARGET_LINK="${REPO_ROOT}/src-tauri/target"

ensure_target_dir() {
  if [[ -L "${TARGET_LINK}" ]]; then
    local link_target
    link_target="$(readlink "${TARGET_LINK}")"
    if [[ "${link_target}" != /* ]]; then
      link_target="$(cd -- "$(dirname "${TARGET_LINK}")" && printf '%s/%s\n' "$(pwd)" "${link_target}")"
    fi
    mkdir -p "${link_target}"
  elif [[ ! -e "${TARGET_LINK}" ]]; then
    mkdir -p "${TARGET_LINK}"
  fi
}

echo "==> Building macOS app bundle"
cd "${REPO_ROOT}"
ensure_target_dir
pnpm tauri build --ci --no-sign --bundles app

echo "==> Locating built app bundle"
APP_PATH="$(find "${REPO_ROOT}/src-tauri/target/release/bundle" -type d -name "${APP_NAME}" -print | head -n 1)"

if [[ -z "${APP_PATH}" ]]; then
  echo "Failed to locate ${APP_NAME} under src-tauri/target/release/bundle"
  exit 1
fi

echo "Found app bundle: ${APP_PATH}"

echo "==> Preparing DMG staging directory"
rm -rf "${STAGE_DIR}"
mkdir -p "${STAGE_DIR}"
mkdir -p "${OUTPUT_DIR}"

cp -R "${APP_PATH}" "${STAGE_DIR}/"
cp "${PACKAGING_DIR}/fix-quarantine.command" "${STAGE_DIR}/"
cp "${PACKAGING_DIR}/安装说明.md" "${STAGE_DIR}/"
chmod +x "${STAGE_DIR}/fix-quarantine.command"

ln -sfn /Applications "${STAGE_DIR}/Applications"

rm -f "${OUTPUT_DMG}"

echo "==> Creating DMG"
hdiutil create \
  -volname "${PRODUCT_NAME}" \
  -srcfolder "${STAGE_DIR}" \
  -ov \
  -format UDZO \
  "${OUTPUT_DMG}"

echo
echo "Done."
echo "DMG path: ${OUTPUT_DMG}"
