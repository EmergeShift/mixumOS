#!/bin/bash
# 同内核 CRIU 基线:dump → 确认进程被杀 → restore → 确认计数继续。
#
# 要求:root、criu、python3;内核需支持 CRIU(criu check 通过)。
# 用法: sudo tests/integration/criu/baseline-same-kernel.sh
set -euo pipefail

if [ "$(id -u)" != "0" ]; then
    echo "需要 root" >&2
    exit 1
fi
command -v criu >/dev/null || { echo "缺少 criu" >&2; exit 1; }
command -v python3 >/dev/null || { echo "缺少 python3" >&2; exit 1; }
if ! criu check >/dev/null 2>&1; then
    echo "criu check 未通过" >&2
    exit 1
fi

ROOT=$(cd "$(dirname "$0")" && pwd)
DIR=$(mktemp -d)
IMGS="$DIR/imgs"
PIDFILE="$DIR/pid"
COUNTERFILE="$DIR/counter"
mkdir -p "$IMGS"
trap 'rm -rf "$DIR"' EXIT

python3 "$ROOT/counter.py" "$PIDFILE" "$COUNTERFILE" </dev/null >"$DIR/proc.log" 2>&1 &
for _ in $(seq 1 50); do [ -s "$PIDFILE" ] && break; sleep 0.1; done
PID=$(cat "$PIDFILE")
echo "PID=$PID  counter=$(cat "$COUNTERFILE")"

if ! criu dump --tree "$PID" --images-dir "$IMGS" --shell-job -v4 -o "$IMGS/dump.log" >"$DIR/dump.out" 2>&1; then
    echo "DUMP_EXIT=1" >&2
    tail -20 "$IMGS/dump.log" 2>/dev/null || true
    exit 1
fi
echo "DUMP_EXIT=0"
sleep 1
BEFORE=$(cat "$COUNTERFILE")
sleep 1
AFTER=$(cat "$COUNTERFILE")
echo "AFTER_DUMP before=$BEFORE after=$AFTER(应相同:进程已被杀死)"
if [ "$BEFORE" != "$AFTER" ]; then
    echo "FAIL: dump 后进程仍在运行" >&2
    exit 1
fi

if ! criu restore --images-dir "$IMGS" --shell-job --restore-detached -v4 -o "$IMGS/restore.log" >"$DIR/restore.out" 2>&1; then
    echo "RESTORE_EXIT=1" >&2
    tail -20 "$IMGS/restore.log" 2>/dev/null || true
    exit 1
fi
echo "RESTORE_EXIT=0"
sleep 1
RESTORED=$(cat "$COUNTERFILE")
echo "RESTORED_COUNTER=$RESTORED(应大于 $AFTER)"
if [ "$RESTORED" -le "$AFTER" ]; then
    tail -20 "$IMGS/restore.log" 2>/dev/null || true
    echo "FAIL: 恢复后计数未继续" >&2
    exit 1
fi
echo "PASS: 同内核 CRIU dump/restore"
