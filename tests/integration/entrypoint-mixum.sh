#!/bin/sh
# e2e 容器入口（mixumOS × redesign 集成变体）：
# mixum-agentd（挂 redesign external agent）+ mixcomp。
# 与 entrypoint-rust.sh 的差异：加载 /etc/mixum/mixumd.json（声明 redesign
# external agent），并把 redesign-cli 的模型配置挂进环境。
set -e

DATA=/var/lib/mixum
WORK=/var/lib/mixum/work
mkdir -p "$WORK" /run/mixum
export XDG_RUNTIME_DIR=/run/mixum
chmod 700 "$XDG_RUNTIME_DIR"

# redesign cli 运行环境（模型路由挂载点由 -v 提供，缺省给容器内路径）
export REDESIGN_MODELS_PATH="${REDESIGN_MODELS_PATH:-/etc/redesign/models.json}"
export REDESIGN_MODEL="${REDESIGN_MODEL:-gpt-6-astra}"

# 1. mixumd（读 agents 配置；后台；证书自签、管理令牌落盘）
mixumd --config /etc/mixum/mixumd.json &
MIXUMD_PID=$!

for i in $(seq 1 50); do
	[ -f "$DATA/admin-token" ] && break
	sleep 0.2
done

# 2. mixcomp（连接 mixumd 控制通道）
mixcomp --sock /run/mixum/display.sock --quality 30 &
MIXCOMP_PID=$!
sleep 1.5

echo "READY token=$(cat "$DATA/admin-token")"
wait $MIXUMD_PID
