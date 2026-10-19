#!/bin/bash
# 跨内核实验的源侧:在源内核 dump 静态计数器,镜像目录交给第二内核 restore。
#
# 用法: sudo tests/integration/criu/dump-for-cross-kernel.sh [镜像目录]
set -euo pipefail

if [ "$(id -u)" != "0" ]; then
    echo "需要 root" >&2
    exit 1
fi
command -v criu >/dev/null || { echo "缺少 criu" >&2; exit 1; }
command -v cc >/dev/null || { echo "缺少 cc(用于静态被测进程)" >&2; exit 1; }

ROOT=$(cd "$(dirname "$0")" && pwd)
OUT=${1:-/tmp/criu-imgs-cross}
BIN=/tmp/criu-static-counter

cc -static -O2 -o "$BIN" "$ROOT/static_counter.c"
rm -rf "$OUT" /tmp/criu-pid /tmp/criu-counter /tmp/criu-proc.log
mkdir -p "$OUT"

"$BIN" </dev/null >/tmp/criu-proc.log 2>&1 &
for _ in $(seq 1 50); do [ -s /tmp/criu-pid ] && break; sleep 0.1; done
PID=$(cat /tmp/criu-pid)
echo "source kernel: $(uname -r)  pid=$PID"

criu dump --tree "$PID" --images-dir "$OUT" --shell-job -v4 -o dump.log >/tmp/criu-dump-cross.out 2>&1
echo "DUMP_EXIT=$?"
sleep 1
echo "COUNTER_AFTER_DUMP=$(cat /tmp/criu-counter 2>/dev/null || echo missing)"
echo "images: $OUT"
ls "$OUT" | head
