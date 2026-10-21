#!/bin/bash
# 跨内核实验的目标端 control:必须先在目标环境完成本地(同内核) dump/restore,
# PASS 才允许进入跨内核恢复步骤;否则目标环境判定无效,实验终止。
#
# 受控实验协议见 docs/criu-feasibility.md §5。
# 用法(在目标 VM 内): sudo tests/integration/criu/target-control.sh <镜像目录>
set -euo pipefail

if [ "$(id -u)" != "0" ]; then
    echo "需要 root" >&2
    exit 1
fi
command -v criu >/dev/null || { echo "缺少 criu" >&2; exit 1; }
command -v python3 >/dev/null || { echo "缺少 python3" >&2; exit 1; }
[ $# -ge 1 ] || { echo "用法: target-control.sh <镜像目录>" >&2; exit 1; }
IMGS="$1"
[ -d "$IMGS" ] || { echo "镜像目录不存在: $IMGS" >&2; exit 1; }

ROOT=$(cd "$(dirname "$0")" && pwd)
CRIU_VER=$(criu --version | awk 'NR==1{sub(/^Version: */,""); print}')
DIR=$(mktemp -d)
mkdir -p "$DIR/imgs"
trap 'rm -rf "$DIR"' EXIT

echo "== 目标环境 control: kernel=$(uname -r) arch=$(uname -m) criu=$CRIU_VER =="
if ! criu check >/dev/null 2>&1; then
    echo "FAIL: 目标环境 criu check 未通过——环境无效,实验终止" >&2
    exit 1
fi

python3 "$ROOT/counter.py" "$DIR/pid" "$DIR/counter" </dev/null >"$DIR/proc.log" 2>&1 &
for _ in $(seq 1 50); do [ -s "$DIR/pid" ] && break; sleep 0.1; done
PID=$(cat "$DIR/pid")

if ! criu dump --tree "$PID" --images-dir "$DIR/imgs" --shell-job -v4 -o "$DIR/imgs/dump.log" >"$DIR/dump.out" 2>&1; then
    tail -20 "$DIR/imgs/dump.log" 2>/dev/null || true
    echo "FAIL: 目标环境本地 dump 失败——环境无效" >&2
    exit 1
fi
sleep 1
BEFORE=$(cat "$DIR/counter"); sleep 1; AFTER=$(cat "$DIR/counter")
if [ "$BEFORE" != "$AFTER" ]; then
    echo "FAIL: 目标环境本地 dump 后进程仍在运行——环境无效" >&2
    exit 1
fi

if ! criu restore --images-dir "$DIR/imgs" --shell-job --restore-detached -v4 -o "$DIR/imgs/restore.log" >"$DIR/restore.out" 2>&1; then
    tail -20 "$DIR/imgs/restore.log" 2>/dev/null || true
    echo "FAIL: 目标环境本地 restore 失败——环境无效" >&2
    exit 1
fi
sleep 1
RESTORED=$(cat "$DIR/counter")
if [ "$RESTORED" -le "$AFTER" ]; then
    tail -20 "$DIR/imgs/restore.log" 2>/dev/null || true
    echo "FAIL: 目标环境本地 restore 后计数未继续——环境无效,实验终止" >&2
    exit 1
fi

# control 通过:写标记与目标元数据,restore 阶段据此断言
{
    echo "criu_version=$CRIU_VER"
    echo "kernel=$(uname -r)"
    echo "arch=$(uname -m)"
} >"$IMGS/control-ok"
echo "PASS: 目标环境 control(本地 dump/restore)——control-ok 已写入 $IMGS"
