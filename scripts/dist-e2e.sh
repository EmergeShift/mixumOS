#!/bin/bash
# e2e 验收:把 dist 产出的 rootfs 导入 docker,跑起 e2e 容器,等 READY
# 令牌,然后用 reference/cmd/e2eclient 验证任务/快照链路。
# 用法:bash scripts/dist-e2e.sh [aarch64]
# 前置:make dist-image(需要 dist/out/rootfs-<arch>.tar)。
set -euo pipefail

ARCH="${1:-aarch64}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/dist/out"
ROOTFS="$OUT/rootfs-$ARCH.tar"
NAME="mixumos-e2e-dist"
IMAGE="mixumos-e2e:$ARCH"
PORT="${E2E_PORT:-17601}"

[ -f "$ROOTFS" ] || { echo "缺 $ROOTFS,先跑 make dist-image" >&2; exit 1; }

docker rm -f "$NAME" >/dev/null 2>&1 || true
echo "==> 导入 rootfs 为镜像 $IMAGE(镜像由 dist 工具链产出)"
docker import \
	--change 'ENTRYPOINT ["/entrypoint.sh"]' \
	--change 'EXPOSE 7601' \
	"$ROOTFS" "$IMAGE" >/dev/null

echo "==> 启动 e2e 容器(宿主端口 $PORT → 容器 7601)"
docker run -d --name "$NAME" -p "$PORT:7601" "$IMAGE" >/dev/null

echo "==> 等待 READY 令牌(最长 90s)"
TOKEN=""
for _ in $(seq 1 90); do
	TOKEN="$(docker logs "$NAME" 2>/dev/null | sed -n 's/^READY token=\(.*\)$/\1/p' | tail -1)"
	[ -n "$TOKEN" ] && break
	if [ "$(docker inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null)" != "true" ]; then
		echo "容器提前退出,日志尾部:" >&2
		docker logs "$NAME" 2>&1 | tail -40 >&2
		exit 1
	fi
	sleep 1
done
[ -n "$TOKEN" ] || {
	echo "超时未拿到 READY 令牌,日志尾部:" >&2
	docker logs "$NAME" 2>&1 | tail -40 >&2
	exit 1
}
echo "==> READY token=${TOKEN:0:12}…"

echo "==> e2eclient:任务/快照链路"
cd "$ROOT"
go run ./reference/cmd/e2eclient -addr "wss://127.0.0.1:$PORT" -token "$TOKEN" -insecure

echo "==> PASS:任务/快照链路验证通过(窗口流视 mixcomp 运行时状态,容器日志 docker logs $NAME)"
