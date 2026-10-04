#!/bin/bash
# dist 构建共享库:Arch 构建容器的镜像/容器保障(幂等)。
# 被 dist/repo/build-repo.sh 与 dist/os/build-image.sh source。
# 容器命名 mixum-build-<arch>,持久复用(pacman 缓存热);挂载:
#   <worktree> → /build/mixumos(只读)   <worktree>/dist/out → /out(读写)
ensure_build_container() {
	local ARCH="$1" PROXY="${2:-}"
	local DARCH
	case "$ARCH" in
		aarch64) DARCH=arm64 ;;
		x86_64)  DARCH=amd64 ;;
		*) echo "未知架构:$ARCH(aarch64|x86_64)" >&2; return 1 ;;
	esac
	local IMAGE="archlinux:mixum-$ARCH"
	local CN="mixum-build-$ARCH"
	local ROOT
	ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

	if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
		echo "==> 拉取 archlinux:base(linux/$DARCH)"
		docker pull --platform "linux/$DARCH" archlinux:base
		docker tag archlinux:base "$IMAGE"
	fi
	if ! docker container inspect "$CN" >/dev/null 2>&1; then
		docker create --name "$CN" --entrypoint sleep \
			-v "$ROOT:/build/mixumos:ro" \
			-v "$ROOT/dist/out:/out" \
			-e http_proxy="$PROXY" -e https_proxy="$PROXY" \
			-e no_proxy="localhost,127.0.0.1" \
			"$IMAGE" infinity >/dev/null
	fi
	docker start "$CN" >/dev/null
}

# 版本号唯一来源:mixum-base 的 pkgver(镜像/OCI 产物命名用)
dist_version() {
	local ROOT
	ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
	unset _commit
	# shellcheck disable=SC1091
	source "$ROOT/dist/pkgs/mixum-base/PKGBUILD"
	echo "$pkgver"
}
