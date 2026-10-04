#!/bin/sh
# e2e 容器入口：起 mixumd + mixcomp，可选起一个 wayland GUI 应用。
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

# 2. mixcomp（连接 mixumd 控制通道）
mixcomp --sock /run/mixum/display.sock --quality 70 &
MIXCOMP_PID=$!
sleep 1.5

# 3. GUI 应用（测试窗口流）：foot 跑一个长驻 shell
if command -v foot >/dev/null 2>&1; then
	foot --hold cat /etc/alpine-release &
	sleep 1
fi

echo "READY token=$(cat "$DATA/admin-token")"
wait $MIXUMD_PID
