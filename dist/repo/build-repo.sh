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
# 代理仅显式提供时启用(MIXUM_PROXY);缺省直连——CN 镜像(TUNA/USTC)直连最优。
# 旧默认 http://192.168.5.2:7897 是特定开发机地址,换机即全量 502(2026-10-18)
PROXY="${MIXUM_PROXY:-}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# git-bash:Windows 形式路径(git -C 与 docker -v 都要);Linux 无 cygpath 原样保留
ROOT="$(cygpath -m "$ROOT" 2>/dev/null || echo "$ROOT")"
OUT="$ROOT/dist/out"
mkdir -p "$OUT/build"
# shellcheck source=../../scripts/lib.sh
. "$ROOT/scripts/lib.sh"

COMMIT="$(git -C "$ROOT" rev-parse HEAD)"

# 容器内 makepkg 的 git 源:worktree 的 .git 是指向主仓库的 gitfile,
# 容器里断链;在宿主侧做一份独立裸仓库,随 /out 挂载进容器
echo "==> 准备源码 bundle(dist/out/src.bundle)"
rm -rf "$OUT/src-bare.git" "$OUT/src.bundle"
# 单文件 bundle 而非裸仓库目录:逐文件 cp 跨 bind mount 曾静默丢对象
# (1507 缺 598,容器端 makepkg「unable to read tree」;bundle 原子且自包含)
git bundle create --quiet "$OUT/src.bundle" --all

echo "==> 导出 rust/ 与交叉协议向量(容器内 cargo 构建 mixum-agentd)"
rm -rf "$OUT/build/rust"
cp -a "$ROOT/rust" "$OUT/build/rust"
rm -rf "$OUT/build/rust/target" "$OUT/build/rust/.cargo-home"
cp -a "$ROOT/tests/cross-protocol" "$OUT/build/cross-protocol"

for ARCH in $ARCHS; do
	echo "==> $ARCH:准备构建容器"
	ensure_build_container "$ARCH" "$PROXY"
	# 输入经 docker cp(走 daemon,字节精确):macOS 目录共享(VirtioFS)对
	# 秒级新文件是最终一致视图——容器曾读到缺对象的源与旧脚本
	# (1507 对象只到 909,bundle 也中招),见 apple-container.md §8
	docker exec "mixum-build-$ARCH" rm -rf /build/incoming
	docker exec "mixum-build-$ARCH" mkdir -p /build/incoming
	docker cp "$OUT/src.bundle" "mixum-build-$ARCH:/build/incoming/src.bundle"
	docker cp "$OUT/build/rust" "mixum-build-$ARCH:/build/incoming/rust"
	docker cp "$OUT/build/cross-protocol" "mixum-build-$ARCH:/build/incoming/cross-protocol"
	docker cp "$ROOT/dist/pkgs" "mixum-build-$ARCH:/build/incoming/pkgs"
	docker cp "$ROOT/dist/repo/container-build.sh" \
		"mixum-build-$ARCH:/build/incoming/container-build.sh"
	echo "==> $ARCH:两段式构建(自建 wlroots0.18 → 四包)+ repo-add"
	docker exec -e _commit="$COMMIT" "mixum-build-$ARCH" \
		bash /build/incoming/container-build.sh "$ARCH"
done

{
	echo "== dist/repo 构建记录"
	echo "date=$(date -u +%FT%TZ) commit=$COMMIT"
} > "$OUT/BUILDINFO"

echo "==> 仓库就绪:"
for ARCH in $ARCHS; do
	ls -1 "$OUT/repo-$ARCH" | sed "s/^/   repo-$ARCH\//"
done
