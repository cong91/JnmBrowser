#!/bin/bash

set -euo pipefail

APP_NAME="JnmBrowser.app"

pick_app_path() {
  if [[ $# -gt 0 && -n "${1:-}" ]]; then
    printf '%s\n' "$1"
    return 0
  fi

  if [[ -d "/Applications/${APP_NAME}" ]]; then
    printf '%s\n' "/Applications/${APP_NAME}"
    return 0
  fi

  if [[ -d "${HOME}/Applications/${APP_NAME}" ]]; then
    printf '%s\n' "${HOME}/Applications/${APP_NAME}"
    return 0
  fi

  return 1
}

APP_PATH="$(pick_app_path "${1:-}")" || {
  echo "未找到 ${APP_NAME}。"
  echo "请先把 ${APP_NAME} 拖到 /Applications 或 ~/Applications。"
  echo "如果你放在了别的位置，请在终端里手动执行："
  echo "bash fix-quarantine.command \"/你的/JnmBrowser.app\""
  exit 1
}

echo "目标应用: ${APP_PATH}"
echo "正在移除 macOS 下载隔离标记..."

if [[ -w "${APP_PATH}" ]]; then
  xattr -dr com.apple.quarantine "${APP_PATH}"
else
  sudo xattr -dr com.apple.quarantine "${APP_PATH}"
fi

echo
echo "处理完成。"
echo "如果刚才已经把应用复制到 Applications，现在可以重新打开它。"
