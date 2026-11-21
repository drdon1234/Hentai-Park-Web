#!/usr/bin/env bash
set -euo pipefail
mkdir -p test/unpacked
ditto -x -k test/packages/HentaiPark-macOS-adhoc.zip test/unpacked
app="$PWD/test/unpacked/HentaiPark.app"
codesign --verify --deep --strict "$app"
bundle=$(/usr/libexec/PlistBuddy -c 'Print CFBundleIdentifier' "$app/Contents/Info.plist")
open -n "$app"
container="$HOME/Library/Containers/$bundle/Data/Library/Application Support"
direct="$HOME/Library/Application Support/$bundle"
for attempt in {1..60}; do
  db=$(find "$container" "$direct" -type f -name park.db -print 2>/dev/null | head -1 || true)
  if [[ -n "$db" ]] && [[ "$(sqlite3 -readonly "$db" "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='works'" 2>/dev/null)" == 1 ]]; then
    pgrep -x HentaiPark >/dev/null
    echo 'macOS 应用已启动，书库数据表已打开。' | tee test/evidence/macos.txt
    exit 0
  fi
  sleep 2
done
echo 'macOS 应用未完成书库初始化。' >&2
exit 1
