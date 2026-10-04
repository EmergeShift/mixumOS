#!/bin/bash
# 在 Arch 容器内部执行:从 mixum 仓库 pacstrap 出 mixumOS rootfs 并打包。
#   用法:container-image.sh <架构名>        # aarch64 | x86_64
# 约定挂载:仓库只读挂 /build/mixumos,产物目录读写挂 /out;
# 前置条件:/out/repo-$ARCH 已由 dist/repo/build-repo.sh 产出。
# 双源:aarch64 容器是 Arch Linux ARM(官方无 arm64,见 lib.sh 注释),
# x86_64 容器是 Arch 官方;镜像内仓库布局随容器发行版自适应。
set -euo pipefail

ARCH="${1:?用法: container-image.sh <aarch64|x86_64>}"
REPO="/out/repo-$ARCH"
TARGET=/target
VERSION="${VERSION:?需要 VERSION 环境变量(mixum-base 的 pkgver)}"

[ -d "$REPO" ] || { echo "缺仓库目录 $REPO,先跑 dist/repo/build-repo.sh" >&2; exit 1; }

# 沙箱与密钥环(幂等;独立跑时也要先自检,ALARM rootfs 需 init)
if ! grep -q '^DisableSandbox' /etc/pacman.conf; then
	sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf
fi
mkdir -p /etc/pacman.d/gnupg
if [ ! -f /etc/pacman.d/gnupg/pubring.kbx ] && [ ! -f /etc/pacman.d/gnupg/pubring.gpg ]; then
	pacman-key --init
fi
for kr in /usr/share/pacman/keyrings/*.gpg; do
	pacman-key --populate "$(basename "$kr" .gpg)" >/dev/null
done
pacman -Syu --noconfirm --needed arch-install-scripts zstd python >/dev/null

# ---- 发行版判定与仓库模板 ----------------------------------------------
IS_ALARM=0
if grep -rq 'archlinuxarm' /etc/pacman.conf /etc/pacman.d/mirrorlist 2>/dev/null; then
	IS_ALARM=1
fi

write_pacman_conf() { # $1 = mixum 仓库 Server;$2 = 1 则 ALARM 布局
	if [ "$2" = "1" ]; then
		cat <<EOF
[options]
HoldPkg = pacman glibc
Architecture = auto
SigLevel = Required DatabaseOptional
LocalFileSigLevel = Optional
ParallelDownloads = 5

[core]
Server = http://mirrors.tuna.tsinghua.edu.cn/archlinuxarm/\$arch/\$repo

[extra]
Server = http://mirrors.tuna.tsinghua.edu.cn/archlinuxarm/\$arch/\$repo

[alarm]
Server = http://mirrors.tuna.tsinghua.edu.cn/archlinuxarm/\$arch/\$repo

[mixum]
SigLevel = Never
Server = $1
EOF
	else
		cat <<EOF
[options]
HoldPkg = pacman glibc
Architecture = auto
SigLevel = Required DatabaseOptional
LocalFileSigLevel = Optional
ParallelDownloads = 5

[core]
Include = /etc/pacman.d/mirrorlist

[extra]
Include = /etc/pacman.d/mirrorlist

[mixum]
SigLevel = Never
Server = $1
EOF
	fi
}

# ---- 容器侧引导配置( pacstrap 读容器的 /etc/pacman.conf) ---------------
write_pacman_conf "file://$REPO" "$IS_ALARM" > /etc/pacman.conf
pacman -Sy >/dev/null
rm -rf "$TARGET" && mkdir -p "$TARGET"

# ---- 引导包清单 ----------------------------------------------------------
# ALARM 的 base 元包依赖 linux-aarch64 内核,容器 rootfs 用不上内核;
# 仅当 base 不含内核时才用 base 元包(与 Arch 官方一致)。
if [ "$IS_ALARM" = "1" ] && pacman -Si base | grep -qE '^Depends.*(linux-|linux )'; then
	echo "==> [container] ALARM base 含内核依赖,改用显式核心清单"
	PKGS=(filesystem bash coreutils glibc pacman systemd ca-certificates
		tar gzip sed grep gawk findutils util-linux procps-ng file tzdata
		mixum-base foot ttf-dejavu)
else
	PKGS=(base mixum-base foot ttf-dejavu)
fi

echo "==> [container] pacstrap $ARCH:base 组件 + mixum-base + foot"
pacstrap "$TARGET" "${PKGS[@]}"

# ---- 仓库与 pacman.conf 烤进镜像(v1 分发:仓库随镜像内置,离线可升) ----
install -d "$TARGET/srv/mixum/repo"
cp -a "$REPO"/. "$TARGET/srv/mixum/repo/"
write_pacman_conf "file:///srv/mixum/repo" "$IS_ALARM" > "$TARGET/etc/pacman.conf"

# ---- e2e 入口(容器冒烟;正式可启动镜像由 systemd 单元接管) -------------
install -m755 /build/mixumos/tests/integration/entrypoint.sh "$TARGET/entrypoint.sh"

# ---- 目录布局:容器内无 systemd 引导,手动过一遍 tmpfiles ----------------
systemd-tmpfiles --root "$TARGET" --create

# ---- 打包:未压缩 tar(OCI 层原料)+ zstd(产物 B) ------------------------
echo "==> [container] 打包 rootfs($ARCH)"
tar -C "$TARGET" --numeric-owner -cf "/out/rootfs-$ARCH.tar" .
zstd -T0 -q -f "/out/rootfs-$ARCH.tar" -o "/out/mixumos-$VERSION-$ARCH-rootfs.tar.zst"

echo "==> [container] rootfs 就绪:"
ls -lh "/out/rootfs-$ARCH.tar" "/out/mixumos-$VERSION-$ARCH-rootfs.tar.zst"
