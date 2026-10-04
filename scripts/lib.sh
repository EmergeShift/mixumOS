#!/bin/bash
# dist 构建共享库:Arch 构建容器的镜像/容器保障(幂等)。
# 被 dist/repo/build-repo.sh 与 dist/os/build-image.sh source。
# 容器命名 mixum-build-<arch>,持久复用(pacman 缓存热);挂载:
#   <worktree> → /build/mixumos(只读)   <worktree>/dist/out → /out(读写)

# 架构名 → Go/OCI 命名(aarch64→arm64,x86_64→amd64)
go_arch() {
	case "$1" in
		aarch64) echo arm64 ;;
		x86_64)  echo amd64 ;;
		*) echo "$1" ;;
	esac
}

# arm64 Arch 容器:Docker Hub 与 ghcr 的官方镜像都只有 amd64 manifest,
# 从 Arch Linux ARM rootfs tar 自起(任务书预案;tar 缓存在 dist/out/alarm/)。
_ensure_alarm_image() {
	local OUT="$1" TAR
	TAR="$OUT/ArchLinuxARM-aarch64-latest.tar.gz"
	if [ ! -f "$TAR" ]; then
		mkdir -p "$OUT/alarm"
		echo "==> 下载 Arch Linux ARM rootfs(约 830MB,缓存于 dist/out/alarm/)"
		# 官方 geo 镜像的 os/ 目录重定向后 404;TUNA 同步且可达
		for URL in \
			"https://mirrors.tuna.tsinghua.edu.cn/archlinuxarm/os/ArchLinuxARM-aarch64-latest.tar.gz" \
			"http://mirror.archlinuxarm.org/os/ArchLinuxARM-aarch64-latest.tar.gz"; do
			if curl -fL --retry 2 --progress-bar -o "$TAR.part" "$URL"; then
				mv "$TAR.part" "$TAR"
				break
			fi
		done
		[ -f "$TAR" ] || { echo "rootfs 下载失败:$URL" >&2; return 1; }
	fi
	docker import "$TAR" archlinux:mixum-aarch64 >/dev/null
}

ensure_build_container() {
	local ARCH="$1" PROXY="${2:-}"
	local DARCH IMAGE CN ROOT
	DARCH="$(go_arch "$ARCH")"
	IMAGE="archlinux:mixum-$ARCH"
	CN="mixum-build-$ARCH"
	ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

	if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
		case "$DARCH" in
			arm64)
				_ensure_alarm_image "$ROOT/dist/out"
				;;
			amd64)
				echo "==> 拉取 Arch 基础镜像(linux/amd64)"
				for SRC in "archlinux:base" "ghcr.io/archlinux/archlinux:base"; do
					if docker pull --platform "linux/$DARCH" "$SRC"; then
						docker tag "$SRC" "$IMAGE"
						break
					fi
				done
				;;
		esac
	fi
	docker image inspect "$IMAGE" >/dev/null 2>&1 ||
		{ echo "拿不到 linux/$DARCH 的 Arch 容器镜像" >&2; return 1; }

	if ! docker container inspect "$CN" >/dev/null 2>&1; then
		# privileged:pacstrap 组装 rootfs 需要 mount /proc /sys /dev 进
		# chroot(默认容器被 Docker 拦),也顺带覆盖 pacman Landlock
		# 沙箱的 seccomp 问题;构建容器跑在专用开发 VM 内,可接受
		docker create --name "$CN" --entrypoint sleep \
			--privileged \
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
