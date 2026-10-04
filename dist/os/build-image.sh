#!/bin/bash
# mixumOS 镜像组装(宿主机侧编排):
#   Arch 容器内 pacstrap rootfs(自 mixum 仓库)→ OCI tar(+多架构合并)。
# 用法:ARCHS="aarch64 [x86_64]" bash dist/os/build-image.sh
# 前置:dist/repo/build-repo.sh 已产出对应架构仓库;产物在 dist/out/。
set -euo pipefail

ARCHS="${ARCHS:-aarch64}"
PROXY="${MIXUM_PROXY:-http://192.168.5.2:7897}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$ROOT/dist/out"
VERSION="$(bash -c ". '$ROOT/scripts/lib.sh' && dist_version")"
# shellcheck source=../scripts/lib.sh
. "$ROOT/scripts/lib.sh"

echo "==> 版本 $VERSION,架构:$ARCHS"
for ARCH in $ARCHS; do
	[ -d "$OUT/repo-$ARCH" ] || {
		echo "缺仓库 dist/out/repo-$ARCH,先跑:ARCHS=\"$ARCH\" bash dist/repo/build-repo.sh" >&2
		exit 1
	}
	ensure_build_container "$ARCH" "$PROXY"
	case "$ARCH" in
		aarch64) DARCH=arm64 ;;
		x86_64)  DARCH=amd64 ;;
	esac

	echo "==> $ARCH:组装 rootfs"
	docker exec -e VERSION="$VERSION" -e _commit="$(git -C "$ROOT" rev-parse HEAD)" \
		"mixum-build-$ARCH" bash /build/mixumos/dist/os/container-image.sh "$ARCH"

	echo "==> $ARCH:打包 OCI"
	docker exec "mixum-build-$ARCH" \
		bash /build/mixumos/dist/os/mkoci.sh \
		"/out/rootfs-$ARCH.tar" "$DARCH" "/out/mixumos-$VERSION-$ARCH.oci.tar" \
		"/out/build/oci-$ARCH"
done

# 多架构合并:两个架构都就绪时产出 multi 索引(manifest list)
if [ "$(printf '%s\n' $ARCHS | wc -l)" -ge 2 ]; then
	MERGE_ARGS=""
	for ARCH in $ARCHS; do
		case "$ARCH" in
			aarch64) DARCH=arm64 ;;
			x86_64)  DARCH=amd64 ;;
		esac
		LAYOUT="/out/build/oci-$ARCH"
		MERGE_ARGS="$MERGE_ARGS $LAYOUT $DARCH"
	done
	echo "==> 合并多架构 OCI 索引"
	docker exec "mixum-build-aarch64" \
		bash /build/mixumos/dist/os/mkoci.sh --merge $MERGE_ARGS \
		"/out/mixumos-$VERSION-multi.oci.tar"
fi

echo "==> 镜像就绪:"
ls -lh "$OUT"/mixumos-*.oci.tar "$OUT"/mixumos-*rootfs.tar.zst 2>/dev/null | awk '{print "   " $5 "\t" $9}'
