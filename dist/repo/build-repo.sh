#!/bin/bash
# mixumOS pacman 仓库构建(宿主机侧编排):
#   导出 rust/ 与交叉协议向量 → 起每架构构建容器 →
#   容器内两段式(自建 wlroots0.18 入库 → makepkg 四包,mixum-agentd 由
#   PKGBUILD 在容器内 cargo 构建)→ repo-add。
# 用法:ARCHS="aarch64 [x86_64]" bash dist/repo/build-repo.sh
# 幂等:重复执行只重打包并刷新仓库 db;产物在 dist/out/(不入库)。
# 删掉 dist/out/repo-<arch>/wlroots0.18-*.pkg.tar.* 可强制重建 wlroots。
set -euo pipefail

ARCHS="${ARCHS:-aarch64}"
PROXY="${MIXUM_PROXY:-http://192.168.5.2:7897}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$ROOT/dist/out"
mkdir -p "$OUT/build"
# shellcheck source=../../scripts/lib.sh
. "$ROOT/scripts/lib.sh"

COMMIT="$(git -C "$ROOT" rev-parse HEAD)"

# 容器内 makepkg 的 git 源:worktree 的 .git 是指向主仓库的 gitfile,
# 容器里断链;在宿主侧做一份独立裸仓库,随 /out 挂载进容器
echo "==> 准备裸仓库源(dist/out/src-bare.git)"
rm -rf "$OUT/src-bare.git"
git clone --bare --quiet "$ROOT" "$OUT/src-bare.git"

echo "==> 导出 rust/ 与交叉协议向量(容器内 cargo 构建 mixum-agentd)"
rm -rf "$OUT/build/rust"
cp -a "$ROOT/rust" "$OUT/build/rust"
rm -rf "$OUT/build/rust/target" "$OUT/build/rust/.cargo-home"
cp -a "$ROOT/tests/cross-protocol" "$OUT/build/cross-protocol"

for ARCH in $ARCHS; do
	echo "==> $ARCH:准备构建容器"
	ensure_build_container "$ARCH" "$PROXY"
	echo "==> $ARCH:两段式构建(自建 wlroots0.18 → 四包)+ repo-add"
	docker exec -e _commit="$COMMIT" "mixum-build-$ARCH" \
		bash /build/mixumos/dist/repo/container-build.sh "$ARCH"
done

{
	echo "== dist/repo 构建记录"
	echo "date=$(date -u +%FT%TZ) commit=$COMMIT"
} > "$OUT/BUILDINFO"

echo "==> 仓库就绪:"
for ARCH in $ARCHS; do
	ls -1 "$OUT/repo-$ARCH" | sed "s/^/   repo-$ARCH\//"
done
