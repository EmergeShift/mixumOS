#!/bin/bash
# 跨内核实验的源侧(受控协议第①步):在源内核 dump 静态计数器,并把
# 源端元数据(CRIU 版本/内核/arch)写入镜像目录的 meta.txt,供目标端
# restore 前断言变量受控。协议见 docs/criu-feasibility.md §5。
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
# 受控协议:记录源端元数据;目标端 restore 前断言 CRIU 版本一致
{
    echo "criu_version=$(criu --version | awk 'NR==1{sub(/^Version: */,""); print}')"
    echo "kernel=$(uname -r)"
    echo "arch=$(uname -m)"
} >"$OUT/meta.txt"
echo "meta: $(tr '\n' ' ' <"$OUT/meta.txt")"
echo "images: $OUT"
ls "$OUT" | head
