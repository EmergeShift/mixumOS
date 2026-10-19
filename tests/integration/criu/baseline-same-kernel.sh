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

criu dump --tree "$PID" --images-dir "$IMGS" --shell-job -v4 -o dump.log >"$DIR/dump.out" 2>&1
echo "DUMP_EXIT=$?"
sleep 1
BEFORE=$(cat "$COUNTERFILE")
sleep 1
AFTER=$(cat "$COUNTERFILE")
echo "AFTER_DUMP before=$BEFORE after=$AFTER(应相同:进程已被杀死)"
if [ "$BEFORE" != "$AFTER" ]; then
    echo "FAIL: dump 后进程仍在运行" >&2
    exit 1
fi

criu restore --images-dir "$IMGS" --shell-job --restore-detached -v4 -o restore.log >"$DIR/restore.out" 2>&1
echo "RESTORE_EXIT=$?"
sleep 1
RESTORED=$(cat "$COUNTERFILE")
echo "RESTORED_COUNTER=$RESTORED(应大于 $AFTER)"
if [ "$RESTORED" -le "$AFTER" ]; then
    tail -20 "$IMGS/restore.log" 2>/dev/null || true
    echo "FAIL: 恢复后计数未继续" >&2
    exit 1
fi
echo "PASS: 同内核 CRIU dump/restore"
