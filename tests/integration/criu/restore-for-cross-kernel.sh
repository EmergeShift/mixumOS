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
CTRL_KERNEL=$(get kernel "$CTRL")
CTRL_ARCH=$(get arch "$CTRL")
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
[ "$CTRL_KERNEL" = "$LOCAL_KERNEL" ] || {
    echo "ABORT: control-ok 的 kernel($CTRL_KERNEL)与本机($LOCAL_KERNEL)不一致" >&2
    exit 1
}
[ "$CTRL_ARCH" = "$LOCAL_ARCH" ] || {
    echo "ABORT: control-ok 的 arch($CTRL_ARCH)与本机($LOCAL_ARCH)不一致" >&2
    exit 1
}
[ "$SRC_ARCH" = "$LOCAL_ARCH" ] || {
    echo "ABORT: 架构不一致——架构不是本实验的变量" >&2
    exit 1
}
if [ "$SRC_KERNEL" = "$LOCAL_KERNEL" ]; then
    echo "ABORT: 两端内核相同——这不是跨内核实验(请用 baseline-same-kernel.sh)" >&2
    exit 1
fi

echo "== 执行跨内核 restore(唯一变量:kernel)=="
# 恢复进程按路径重开计数器文件,CRIU 会校验文件大小——用源端 dump 后
# 记录的值预建,保证字节数一致(环境准备,非实验变量)。
SRC_AFTER_DUMP=$(get counter_after_dump "$META")
if [ -n "${SRC_AFTER_DUMP:-}" ] && [ "$SRC_AFTER_DUMP" != "missing" ]; then
    printf '%s' "$SRC_AFTER_DUMP" >/tmp/criu-counter
fi
# 旧尝试留下的 criu-static-counter 可能继续写同一路径，先确认无人在写。
C_PRE=$(cat /tmp/criu-counter 2>/dev/null || echo missing)
sleep 1
C_PRE2=$(cat /tmp/criu-counter 2>/dev/null || echo missing)
if [ "$C_PRE" != "$C_PRE2" ]; then
    echo "ABORT: /tmp/criu-counter 在 restore 前仍在变化，疑似残留进程" >&2
    exit 1
fi
if criu restore --images-dir "$IMGS" --shell-job --restore-detached -v4 -o "$IMGS/restore.log"; then
    # 自证:恢复后的进程应继续写计数器——两次采样严格递增,
    # 且不早于源端 dump 后的值(不依赖人工核对)。
    C1=$(cat /tmp/criu-counter 2>/dev/null || echo missing)
    sleep 2
    C2=$(cat /tmp/criu-counter 2>/dev/null || echo missing)
    echo "计数采样: dump 后=$SRC_AFTER_DUMP 恢复后第一次=$C1 第二次=$C2"
    if [ "$C1" != "missing" ] && [ "$C2" != "missing" ] \
        && [ "$C2" -gt "$C1" ] && [ "$C1" -ge "${SRC_AFTER_DUMP:-0}" ]; then
        echo "PASS: 跨内核 restore 成功且计数继续($C1 → $C2;源 dump 后=$SRC_AFTER_DUMP)"
        exit 0
    fi
    echo "FAIL: restore 命令返回成功但计数未继续(自证失败)——按协议 §5 先归因环境" >&2
    exit 1
fi
echo "FAIL: 跨内核 restore 失败(源 kernel=$SRC_KERNEL → 目标 kernel=$LOCAL_KERNEL)" >&2
tail -20 "$IMGS/restore.log" 2>/dev/null || true
echo "请按协议 §5 对失败进行分类(环境/版本/内核)后再下结论" >&2
exit 1
