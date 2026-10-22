#!/bin/sh
# e2e 容器入口（Rust 变体）：起 mixum-agentd + mixcomp，可选起一个 wayland GUI 应用。
# 与 entrypoint.sh 一致；mixumd 即 mixum-agentd 二进制。
set -e

DATA=/var/lib/mixum
WORK=/var/lib/mixum/work
HOME_DIR=/var/lib/mixum/home
mkdir -p "$DATA" "$WORK" "$HOME_DIR" /run/mixum /run/mixum/pulse
# 数据目录只有 root 可读（token/key/snapshot）；agent 只能碰 work/home。
chmod 0711 "$DATA"
chown root:root "$DATA" 2>/dev/null || true
chown mixum:mixum "$WORK" "$HOME_DIR" /run/mixum /run/mixum/pulse 2>/dev/null || true
chmod 0750 "$WORK" "$HOME_DIR"
export XDG_RUNTIME_DIR=/run/mixum
chmod 0750 "$XDG_RUNTIME_DIR"

# 1. mixumd（后台，证书自签、令牌落盘）
GW_ARGS=""
[ -n "$MIXUM_GATEWAY_URL" ] && GW_ARGS="$GW_ARGS --gateway $MIXUM_GATEWAY_URL"
[ -n "$MIXUM_GATEWAY_NAME" ] && GW_ARGS="$GW_ARGS --gateway-name $MIXUM_GATEWAY_NAME"
[ -n "$MIXUM_GATEWAY_TOKEN" ] && GW_ARGS="$GW_ARGS --gateway-token $MIXUM_GATEWAY_TOKEN"
mixumd --config /dev/null $GW_ARGS &
MIXUMD_PID=$!

# 等令牌生成
for i in $(seq 1 50); do
	[ -f "$DATA/admin-token" ] && break
	sleep 0.2
done

# 2. PulseAudio（可选）：null sink + 本机 socket；Apple Container 无 systemd，
#    由入口直接拉起。没有 pulseaudio 的旧镜像/精简环境自动跳过。
if command -v pulseaudio >/dev/null 2>&1; then
	mkdir -p /run/mixum/pulse
	chown mixum:mixum /run/mixum/pulse 2>/dev/null || true
	if id mixum >/dev/null 2>&1; then
		su -s /bin/sh mixum -c 'HOME=/tmp XDG_RUNTIME_DIR=/run/mixum PULSE_RUNTIME_PATH=/run/mixum/pulse exec pulseaudio --daemonize=no --exit-idle-time=-1 -n --file=/etc/mixum/pulse.pa' >/tmp/pulse.log 2>&1 &
	fi
fi

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
