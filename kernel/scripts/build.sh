#!/usr/bin/env bash
# linux-mixum / linux-mixum-micro 构建脚本。
#
# 用法：
#   ./kernel/scripts/build.sh --variant micro --arch arm64
#   ./kernel/scripts/build.sh --variant full  --arch x86_64 --base arch
#   ./kernel/scripts/build.sh --variant micro --arch arm64 --base file:/path/kata.config
#   ./kernel/scripts/build.sh --verify-config-only --variant micro
#
# 宿主机只需要 Docker；脚本会把自身重新拉进构建容器执行。
set -euo pipefail

ORIG_ARGS=("$@")
VERSION="${MIXUM_KERNEL_VERSION:-6.18.35}"
VARIANT="micro"
ARCH="$(uname -m)"
BASE="defconfig"
JOBS="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"
MODULES=0
VERIFY_ONLY=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --variant) VARIANT="$2"; shift 2 ;;
    --arch) ARCH="$2"; shift 2 ;;
    --base) BASE="$2"; shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    --jobs) JOBS="$2"; shift 2 ;;
    --modules) MODULES=1; shift ;;
    --verify-config-only) VERIFY_ONLY=1; shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done

case "$VARIANT" in
  micro|full) ;;
  *) echo "--variant 必须是 micro 或 full" >&2; exit 2 ;;
esac
case "$ARCH" in
  arm64|aarch64) ARCH=arm64 ;;
  x86_64|amd64) ARCH=x86_64 ;;
  *) echo "--arch 必须是 arm64 或 x86_64" >&2; exit 2 ;;
esac

KERNEL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_DIR="$(cd "$KERNEL_DIR/.." && pwd)"
CACHE="$KERNEL_DIR/.cache"
BUILD_ROOT="$KERNEL_DIR/.build"
OUT="$KERNEL_DIR/out/linux-${VARIANT}-${ARCH}"

if [[ "${MIXUM_KERNEL_BUILDER:-0}" != "1" ]]; then
  command -v docker >/dev/null || { echo "需要 Docker" >&2; exit 1; }
  # 在宿主上解压：Docker Desktop 的 bind mount 对内核 tarball 里的 symlink
  # 可能返回 EPERM，导致半个源码树。
  mkdir -p "$CACHE" "$BUILD_ROOT"
  SRC_TAR="$CACHE/linux-${VERSION}.tar.xz"
  SRC_DIR="$BUILD_ROOT/linux-${VERSION}"
  if [[ ! -f "$SRC_TAR" ]]; then
    URL="https://cdn.kernel.org/pub/linux/kernel/v${VERSION%%.*}.x/linux-${VERSION}.tar.xz"
    echo "下载内核源码: $URL"
    curl -fL --retry 3 --connect-timeout 20 -o "$SRC_TAR.part" "$URL"
    mv "$SRC_TAR.part" "$SRC_TAR"
  fi
  docker build -q -t mixum-kernel-builder:bookworm -f "$KERNEL_DIR/Dockerfile" "$KERNEL_DIR" >/dev/null
  exec docker run --rm \
    -v "$KERNEL_DIR:/work/kernel" -w /work/kernel \
    -e MIXUM_KERNEL_BUILDER=1 \
    mixum-kernel-builder:bookworm \
    bash scripts/build.sh "${ORIG_ARGS[@]}"
fi

BUILD_ROOT="/build"
mkdir -p "$CACHE" "$BUILD_ROOT" "$OUT"

SRC_TAR="$CACHE/linux-${VERSION}.tar.xz"
SRC_DIR="$BUILD_ROOT/linux-${VERSION}"

if [[ ! -f "$SRC_TAR" ]]; then
  URL="https://cdn.kernel.org/pub/linux/kernel/v${VERSION%%.*}.x/linux-${VERSION}.tar.xz"
  echo "下载内核源码: $URL"
  curl -fL --retry 3 --connect-timeout 20 -o "$SRC_TAR.part" "$URL"
  mv "$SRC_TAR.part" "$SRC_TAR"
fi

if [[ ! -d "$SRC_DIR" ]]; then
  echo "解压源码"
  tar -C "$BUILD_ROOT" -xf "$SRC_TAR"
fi

cd "$SRC_DIR"

# ---- 基底 config ----
case "$BASE" in
  defconfig)
    echo "基底: defconfig"
    make ARCH="$ARCH" defconfig
    ;;
  arch)
    ARCH_CFG="$CACHE/arch-linux.config"
    if [[ ! -f "$ARCH_CFG" ]]; then
      echo "下载 Arch Linux 内核 config"
      curl -fL --retry 3 -o "$ARCH_CFG.part" \
        "https://gitlab.archlinux.org/archlinux/packaging/packages/linux/-/raw/main/config"
      mv "$ARCH_CFG.part" "$ARCH_CFG"
    fi
    cp "$ARCH_CFG" .config
    ;;
  file:*)
    BASE_FILE="${BASE#file:}"
    [[ "$BASE_FILE" != /* ]] && BASE_FILE="$KERNEL_DIR/$BASE_FILE"
    cp "$BASE_FILE" .config
    ;;
  *)
    echo "--base 只支持 defconfig | arch | file:<path>" >&2
    exit 2
    ;;
esac

# ---- 合并 fragment ----
COMMON="$KERNEL_DIR/configs/common.fragment"
VARIANT_FRAGMENT="$KERNEL_DIR/configs/${VARIANT}.fragment"
echo "合并配置: common + $VARIANT"
scripts/kconfig/merge_config.sh -m .config "$COMMON" "$VARIANT_FRAGMENT" >/dev/null
make ARCH="$ARCH" olddefconfig

# ---- 配置断言 ----
check_config() {
  local key="$1"
  if ! grep -q "^${key}=y$" .config; then
    echo "配置缺失: ${key}=y" >&2
    return 1
  fi
}
REQUIRED=(
  CONFIG_CHECKPOINT_RESTORE
  CONFIG_USERFAULTFD
  CONFIG_SECURITY_LANDLOCK
  CONFIG_FUSE_FS
  CONFIG_VIRTIO_FS
  CONFIG_KSM
  CONFIG_ZRAM
  CONFIG_BPF_LSM
  CONFIG_DEBUG_INFO_BTF
  CONFIG_VSOCKETS
  CONFIG_NET_9P
)
for key in "${REQUIRED[@]}"; do check_config "$key"; done
grep -q '^CONFIG_LSM=.*landlock' .config || { echo "CONFIG_LSM 未包含 landlock" >&2; exit 1; }

if [[ "$VERIFY_ONLY" == "1" ]]; then
  echo "配置断言通过（未编译）"
  exit 0
fi

# ---- 交叉编译参数 ----
HOST_ARCH="$(uname -m)"
CROSS=""
if [[ "$ARCH" == "arm64" && "$HOST_ARCH" != "aarch64" && "$HOST_ARCH" != "arm64" ]]; then
  CROSS="aarch64-linux-gnu-"
fi
if [[ "$ARCH" == "x86_64" && "$HOST_ARCH" != "x86_64" ]]; then
  CROSS="x86_64-linux-gnu-"
fi
MAKE_ARGS=(ARCH="$ARCH" -j"$JOBS")
[[ -n "$CROSS" ]] && MAKE_ARGS+=("CROSS_COMPILE=$CROSS")

if [[ "$ARCH" == "arm64" ]]; then
  TARGET=Image
else
  TARGET=bzImage
fi

echo "编译 $TARGET（$ARCH，$JOBS 并发）"
make "${MAKE_ARGS[@]}" "$TARGET"
if [[ "$MODULES" == "1" || "$VARIANT" == "full" ]]; then
  make "${MAKE_ARGS[@]}" modules
  # 全量内核给外设用：把模块装进输出目录并打成一个包，镜像里解到 /lib/modules。
  make "${MAKE_ARGS[@]}" modules_install INSTALL_MOD_PATH="$OUT/modules" DEPMOD=true
  tar -C "$OUT/modules" -c . | zstd -T0 -q -o "$OUT/modules.tar.zst"
  rm -rf "$OUT/modules"
  echo "模块: $OUT/modules.tar.zst"
fi

# ---- 产物 ----
cp "arch/${ARCH}/boot/${TARGET}" "$OUT/${TARGET}"
cp vmlinux "$OUT/vmlinux"
cp .config "$OUT/config"
cp System.map "$OUT/System.map"
echo "产物: $OUT"
echo "uname 预期: $(grep '^CONFIG_LOCALVERSION=' .config | cut -d'"' -f2)，内核版本 ${VERSION}"
