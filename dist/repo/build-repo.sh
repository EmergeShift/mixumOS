#!/bin/bash
# mixumOS pacman 仓库构建(宿主机侧编排):
#   起 Arch 容器(每架构一个,持久复用)→ 容器内 makepkg 四包 → repo-add。
# 用法:ARCHS="aarch64 [x86_64]" bash dist/repo/build-repo.sh
# 幂等:重复执行只重打包并刷新仓库 db;产物在 dist/out/(不入库)。
set -euo pipefail

ARCHS="${ARCHS:-aarch64}"
PROXY="${MIXUM_PROXY:-http://192.168.5.2:7897}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$ROOT/dist/out"
mkdir -p "$OUT"
# shellcheck source=../scripts/lib.sh
. "$ROOT/scripts/lib.sh"

COMMIT="$(git -C "$ROOT" rev-parse HEAD)"

for ARCH in $ARCHS; do
	echo "==> $ARCH:准备构建容器"
	ensure_build_container "$ARCH" "$PROXY"
	echo "==> $ARCH:构建四包 + repo-add"
	docker exec -e _commit="$COMMIT" "mixum-build-$ARCH" \
		bash /build/mixumos/dist/repo/container-build.sh "$ARCH"
done

echo "==> 仓库就绪:"
for ARCH in $ARCHS; do
	ls -1 "$OUT/repo-$ARCH" | sed "s/^/   repo-$ARCH\//"
done
