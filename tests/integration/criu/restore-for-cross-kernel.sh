#!/bin/bash
# 跨内核恢复(受控协议第③步):仅当
#   ① 源端 meta.txt 的 CRIU 版本与本机完全一致,且
#   ② 目标端 control-ok 存在(同内核本地 dump/restore 已 PASS)
# 时才执行 restore。任何一条不满足即中止并说明原因。
#
# 受控实验协议见 docs/criu-feasibility.md §5。
# 用法(在目标 VM 内): sudo tests/integration/criu/restore-for-cross-kernel.sh <镜像目录>
set -euo pipefail

if [ "$(id -u)" != "0" ]; then
    echo "需要 root" >&2
    exit 1
fi
command -v criu >/dev/null || { echo "缺少 criu" >&2; exit 1; }
[ $# -ge 1 ] || { echo "用法: restore-for-cross-kernel.sh <镜像目录>" >&2; exit 1; }
IMGS="$1"
META="$IMGS/meta.txt"
CTRL="$IMGS/control-ok"
[ -f "$META" ] || { echo "缺少 $META(先跑 dump-for-cross-kernel.sh)" >&2; exit 1; }
[ -f "$CTRL" ] || {
    echo "缺少 $CTRL:目标环境 control 未通过——按协议不得进入跨内核步骤" >&2
    exit 1
}

get() { awk -F= -v k="$1" '$1==k{sub(/^[^=]*=/,""); print}' "$2"; }
SRC_CRIU=$(get criu_version "$META")
LOCAL_CRIU=$(criu --version | awk 'NR==1{sub(/^Version: */,""); print}')
CTRL_CRIU=$(get criu_version "$CTRL")
SRC_KERNEL=$(get kernel "$META")
SRC_ARCH=$(get arch "$META")
LOCAL_KERNEL=$(uname -r)
LOCAL_ARCH=$(uname -m)

echo "源:   kernel=$SRC_KERNEL arch=$SRC_ARCH criu=$SRC_CRIU"
echo "目标: kernel=$LOCAL_KERNEL arch=$LOCAL_ARCH criu=$LOCAL_CRIU"

[ "$SRC_CRIU" = "$LOCAL_CRIU" ] || {
    echo "ABORT: CRIU 版本不一致($SRC_CRIU vs $LOCAL_CRIU)——变量不受控,实验无效" >&2
    exit 1
}
[ "$CTRL_CRIU" = "$LOCAL_CRIU" ] || {
    echo "ABORT: control-ok 的 CRIU 版本($CTRL_CRIU)与本机不一致" >&2
    exit 1
}
[ "$SRC_ARCH" = "$LOCAL_ARCH" ] || {
    echo "ABORT: 架构不一致——架构不是本实验的变量" >&2
    exit 1
}
if [ "$SRC_KERNEL" = "$LOCAL_KERNEL" ]; then
    echo "注意: 两端内核相同——这是同内核恢复,不是跨内核实验" >&2
fi

echo "== 执行跨内核 restore(唯一变量:kernel)=="
criu restore --images-dir "$IMGS" --shell-job --restore-detached -v4 -o restore.log && {
    echo "PASS: 跨内核 restore 成功(请核对恢复进程计数继续,并记录内核对)"
    exit 0
}
echo "FAIL: 跨内核 restore 失败(源 kernel=$SRC_KERNEL → 目标 kernel=$LOCAL_KERNEL)" >&2
tail -20 "$IMGS/restore.log" 2>/dev/null || true
echo "请按协议 §5 对失败进行分类(环境/版本/内核)后再下结论" >&2
exit 1
