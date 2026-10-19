#!/bin/sh
# e2e 容器入口（Rust 变体）：起 mixum-agentd + mixcomp，可选起一个 wayland GUI 应用。
# 与 entrypoint.sh 一致；mixumd 即 mixum-agentd 二进制。
set -e

DATA=/var/lib/mixum
WORK=/var/lib/mixum/work
mkdir -p "$WORK" /run/mixum
export XDG_RUNTIME_DIR=/run/mixum
chmod 700 "$XDG_RUNTIME_DIR"

# 1. mixumd（后台，证书自签、令牌落盘）
mixumd --config /dev/null &
MIXUMD_PID=$!

# 等令牌生成
for i in $(seq 1 50); do
	[ -f "$DATA/admin-token" ] && break
	sleep 0.2
done

# 2. mixcomp（连接 mixumd 控制通道）。e2eclient（gorilla 默认读缓冲 4096）
# 扛不住大帧， JPEG 质量压低以保单帧 <4KiB。
mixcomp --sock /run/mixum/display.sock --quality "${MIXUM_JPEG_QUALITY:-90}" &
MIXCOMP_PID=$!
sleep 1.5

# 3. GUI 应用（测试窗口流）：foot 跑一个长驻 shell
if command -v foot >/dev/null 2>&1; then
	foot --override=main.initial-window-size-pixels=300x200 --hold cat /etc/alpine-release &
	sleep 1
fi

echo "READY token=$(cat "$DATA/admin-token")"
wait $MIXUMD_PID
