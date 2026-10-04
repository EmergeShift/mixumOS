#!/bin/bash
# 在 Arch 容器内部执行:从 mixum 仓库 pacstrap 出 mixumOS rootfs 并打包。
#   用法:container-image.sh <架构名>        # aarch64 | x86_64
# 约定挂载:仓库只读挂 /build/mixumos,产物目录读写挂 /out;
# 前置条件:/out/repo-$ARCH 已由 dist/repo/build-repo.sh 产出。
set -euo pipefail

ARCH="${1:?用法: container-image.sh <aarch64|x86_64>}"
REPO="/out/repo-$ARCH"
TARGET=/target
VERSION="${VERSION:?需要 VERSION 环境变量(mixum-base 的 pkgver)}"

[ -d "$REPO" ] || { echo "缺仓库目录 $REPO,先跑 dist/repo/build-repo.sh" >&2; exit 1; }

# ---- 1. pacstrap 与压缩工具(幂等) ------------------------------------
pacman -Syu --noconfirm --needed arch-install-scripts zstd python >/dev/null

# ---- 2. pacman.conf:容器侧(引导用)与目标侧(烤进镜像)同一模板 -----
# [mixum] 仓库 SigLevel=Never:v1 不签名(签名后置,见 docs/distro.md)。
write_pacman_conf() {  # $1 = mixum 仓库 Server
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
}
write_pacman_conf "file://$REPO" > /etc/pacman.conf
mkdir -p "$TARGET"

# ---- 3. pacstrap:base + mixum-base(锚点,拉齐四包)+ foot(e2e GUI) --
# pacstrap 在容器内对 /target 装机:脚本安装段(scriptlet)chroot 执行,
# sysusers/tmpfiles 由 systemd 的安装脚本落好。
echo "==> [container] pacstrap base mixum-base foot"
rm -rf "$TARGET" && mkdir -p "$TARGET"
pacstrap -K -c "$TARGET" base mixum-base foot ttf-dejavu

# ---- 4. 仓库与 pacman.conf 烤进镜像(v1 分发:仓库随镜像内置,离线可升)--
install -d "$TARGET/srv/mixum/repo"
cp -a "$REPO"/. "$TARGET/srv/mixum/repo/"
write_pacman_conf "file:///srv/mixum/repo" > "$TARGET/etc/pacman.conf"

# ---- 5. e2e 入口(容器冒烟;正式可启动镜像由 systemd 单元接管) --------
install -m755 /build/mixumos/tests/integration/entrypoint.sh "$TARGET/entrypoint.sh"

# ---- 6. 目录布局:容器内无 systemd 引导,手动过一遍 tmpfiles ------------
systemd-tmpfiles --root "$TARGET" --create

# ---- 7. 打包:未压缩 tar(OCI 层原料)+ zstd(产物 B) -------------------
echo "==> [container] 打包 rootfs($ARCH)"
tar -C "$TARGET" --numeric-owner -cf "/out/rootfs-$ARCH.tar" .
zstd -T0 -q -f "/out/rootfs-$ARCH.tar" -o "/out/mixumos-$VERSION-$ARCH-rootfs.tar.zst"

echo "==> [container] rootfs 就绪:"
ls -lh "/out/rootfs-$ARCH.tar" "/out/mixumos-$VERSION-$ARCH-rootfs.tar.zst"
