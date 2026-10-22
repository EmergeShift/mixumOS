#!/usr/bin/env bash
# 在 QEMU virt 上启动 linux-mixum full（静态 busybox initramfs）。
# 依赖：qemu-system-aarch64、docker（只用来取 Alpine 的 busybox + musl loader）、cpio、gzip。
set -euo pipefail
IMAGE="${1:?用法: run.sh <Image> [内存MB]}"
MEM="${2:-1024}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"; pkill -P $$ qemu-system-aarch64 2>/dev/null || true' EXIT

mkdir -p "$WORK/root"/{bin,lib,proc,sys,dev,tmp}
docker create --name mixum-qemu-busy alpine:3.20 >/dev/null
docker cp mixum-qemu-busy:/bin/busybox "$WORK/root/bin/busybox" >/dev/null
docker run --rm alpine:3.20 cat /lib/ld-musl-aarch64.so.1 > "$WORK/root/lib/ld-musl-aarch64.so.1"
docker rm mixum-qemu-busy >/dev/null
chmod +x "$WORK/root/lib/ld-musl-aarch64.so.1" "$WORK/root/bin/busybox"
ln -s ld-musl-aarch64.so.1 "$WORK/root/lib/libc.musl-aarch64.so.1"

cat > "$WORK/root/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox mount -t proc none /proc
/bin/busybox mount -t sysfs none /sys
/bin/busybox mount -t devtmpfs none /dev
echo FULL_KERNEL_BOOT_OK
/bin/busybox uname -a
exec /bin/busybox sh
INIT
chmod +x "$WORK/root/init"

cd "$WORK/root"
find . | cpio -o -H newc 2>/dev/null | gzip -1 > "$WORK/initramfs.cpio.gz"

echo "QEMU：${IMAGE}（内存 ${MEM}MB），Ctrl-A X 退出"
exec qemu-system-aarch64 -M virt -cpu max -m "$MEM" -smp 2 \
  -kernel "$IMAGE" -initrd "$WORK/initramfs.cpio.gz" \
  -append 'console=ttyAMA0 rdinit=/init' -nographic -no-reboot
