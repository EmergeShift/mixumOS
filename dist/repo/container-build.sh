#!/bin/bash
# 在 Arch 容器内部执行,两段式:
#   A. 自建 wlroots0.18 包 → 入 mini 仓库 → 容器 pacman 认领 [mixum] 源
#      (官方源已无 wlroots0.18,依赖必须先自给,见包内注释);
#   B. 按 PKGBUILD 声明装依赖 → makepkg 四包 → repo-add。
# 由 dist/repo/build-repo.sh 经 docker cp 注入 /build/incoming 后在容器里调用;
# 不在宿主机直接跑。  用法:container-build.sh <架构名>        # aarch64 | x86_64
# 约定:全部输入(build-repo.sh 经 docker cp 注入)在 /build/incoming/;
#       产物目录读写挂 /out(输出方向;若宿主回读陈旧,build-repo.sh 负责 docker cp 回收)。
set -euo pipefail

ARCH="${1:?用法:container-build.sh <aarch64|x86_64>}"
IN="/build/incoming"
GIT_SRC="$IN"
BUILD="/build/pkgs"
OUT_REPO="/out/repo-$ARCH"
COMMIT="${_commit:-HEAD}"

echo "==> [container] 架构 $ARCH,commit ${COMMIT:0:12}"

# ---- 0. 沙箱与密钥环(幂等;ALARM rootfs 的 keyring 未初始化,必须先做) --
# 容器(尤其 qemu 模拟)里 pacman 的沙箱起不来:6.x 用整开关,7.x 起拆成
# Filesystem/Syscalls 两个细粒度开关,分别处理
PACMAN_MAJOR="$(pacman -Q pacman | sed 's/^pacman \([0-9]*\).*/\1/')"
if [ "$PACMAN_MAJOR" -lt 7 ] && ! grep -q '^DisableSandbox$' /etc/pacman.conf; then
	sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf
fi
if [ "$PACMAN_MAJOR" -ge 7 ]; then
	sed -i -E 's/^#+DisableSandboxSyscalls/DisableSandboxSyscalls/' /etc/pacman.conf
	grep -q '^DisableSandboxFilesystem' /etc/pacman.conf ||
		sed -i '/^\[options\]/a DisableSandboxFilesystem' /etc/pacman.conf
fi
mkdir -p /etc/pacman.d/gnupg
# 无条件 init(幂等):新版 archlinux:base 自带 pubring.kbx 但无私钥,
# 跳过 init 会让 --populate 本地签名报「no secret key available to sign with」
pacman-key --init || { sleep 2; pacman-key --init; }
for kr in /usr/share/pacman/keyrings/*.gpg; do
	pacman-key --populate "$(basename "$kr" .gpg)" >/dev/null ||
		{ sleep 2; pacman-key --populate "$(basename "$kr" .gpg)" >/dev/null; }
done
# ALARM rootfs 默认镜像走 geo 重定向,部分网络经代理不可达;可覆盖为可达镜像
# (如 TUNA 的 archlinuxarm 同步),经 lib.sh 的 MIXUM_ARM_MIRROR 传入
if [ -n "${MIXUM_ARM_MIRROR:-}" ]; then
	echo "Server = ${MIXUM_ARM_MIRROR}/\$arch/\$repo" > /etc/pacman.d/mirrorlist
fi
pacman -Syu --noconfirm --needed base-devel git >/dev/null
git config --system --add safe.directory "$GIT_SRC"

# 按 PKGBUILD 声明装依赖(不硬编码,避免两处漂移)
install_pkgbuild_deps() {
	(
		cd "$1"
		# shellcheck disable=SC1091
		source ./PKGBUILD
		# Some packages are built locally and installed with pacman -U before
		# this point (criu-mixum); they are not in a sync database, so pacman
		# -S cannot resolve them even though they are installed.
		local missing=()
		local dep
		for dep in ${depends[@]+"${depends[@]}"} \
			${makedepends[@]+"${makedepends[@]}"} \
			${checkdepends[@]+"${checkdepends[@]}"}; do
			pacman -Q "$dep" >/dev/null 2>&1 || missing+=("$dep")
		done
		[ ${#missing[@]} -eq 0 ] || pacman -S --noconfirm --needed "${missing[@]}"
	)
}

# ---- A. wlroots0.18 自给 ------------------------------------------------
WL_FILE="$(ls "$OUT_REPO"/wlroots0.18-*.pkg.tar.* 2>/dev/null | head -1 || true)"
if [ -z "$WL_FILE" ]; then
	echo "==> [container] stage A:makepkg wlroots0.18"
	rm -rf "$BUILD/wlroots0.18"
	id builder >/dev/null 2>&1 || useradd -m builder
	install -d "$BUILD"
	cp -a "$GIT_SRC/pkgs/wlroots0.18" "$BUILD/wlroots0.18"
	chown -R builder:builder "$BUILD/wlroots0.18"
	install_pkgbuild_deps "$BUILD/wlroots0.18"
	(
		cd "$BUILD/wlroots0.18"
		runuser -u builder -- makepkg -f --noconfirm
	)
	install -d "$OUT_REPO"
	mv "$BUILD/wlroots0.18/"wlroots0.18-*.pkg.tar.* "$OUT_REPO/"
	WL_FILE="$(ls "$OUT_REPO"/wlroots0.18-*.pkg.tar.* | head -1)"
	repo-add "$OUT_REPO/mixum.db.tar.zst" "$WL_FILE"
else
	echo "==> [container] stage A:wlroots0.18 已在仓库,跳过($WL_FILE)"
fi

# 容器认领 [mixum] 仓库(幂等追加)
if ! grep -q '^\[mixum\]' /etc/pacman.conf; then
	cat >> /etc/pacman.conf <<EOF

[mixum]
SigLevel = Never
Server = file://$OUT_REPO
EOF
fi
pacman -Sy >/dev/null

# ---- B. 四包构建 ---------------------------------------------------------
# cargo 缓存持久卷(挂载于 $BUILD 之外,rm -rf $BUILD 不影响):
#   /cargo-home = CARGO_HOME(registry,跨架构共享) /cargo-target = 编译产物(按架构)
# 二次构建走增量:fetch 秒级、只编改动 crate(见 distro.md §7.3.1 演进项)
export MIXUM_CARGO_HOME=/cargo-home
export MIXUM_CARGO_TARGET_DIR=/cargo-target
echo "==> [container] stage B:按 PKGBUILD 声明装依赖"
rm -rf /build/src "$BUILD"
install -d "$BUILD"
# mixcomp 的 git 源:宿主打的单文件 bundle(经 docker cp 注入)——
# 目录逐文件拷贝在 macOS 共享挂载上曾静默丢对象,clone 自 bundle 完整性由 git 保证
git clone --bare --quiet "$IN/src.bundle" /build/src
git -C /build/src cat-file -e "$(git -C /build/src rev-parse HEAD^{tree})"  # bundle 完整性闸门
git config --system --add safe.directory /build/src
chmod -R a+rX /build/src
cp -a "$GIT_SRC/pkgs/." "$BUILD/"
rm -rf "$BUILD/wlroots0.18"
# mixum-agentd 由 PKGBUILD(cargo)在容器内原生构建:同步 rust/ 与交叉协议向量
rm -rf "$BUILD/mixum-agentd/rust" "$BUILD/mixum-agentd/tests"
cp -a "$IN/rust" "$BUILD/mixum-agentd/rust"
install -d "$BUILD/mixum-agentd/tests"
cp -a "$IN/cross-protocol" "$BUILD/mixum-agentd/tests/cross-protocol"
# mixum-ctl 也从同一 Rust workspace 构建;包目录独立,避免 makepkg 串源。
rm -rf "$BUILD/mixum-ctl/rust"
cp -a "$IN/rust" "$BUILD/mixum-ctl/rust"
id builder >/dev/null 2>&1 || useradd -m builder
# 卷根属主(builder 每次写入);卷内容上轮已属 builder,不必递归
install -d -o builder -g builder "$MIXUM_CARGO_HOME" "$MIXUM_CARGO_TARGET_DIR" 2>/dev/null ||
	{ mkdir -p "$MIXUM_CARGO_HOME" "$MIXUM_CARGO_TARGET_DIR"; chown builder:builder "$MIXUM_CARGO_HOME" "$MIXUM_CARGO_TARGET_DIR"; }
chown -R builder:builder "$BUILD"

# 三个组件包:装依赖 → makepkg(mixum-base 依赖它们,放最后单独处理)
for p in criu mixcomp mixum-agentd mixum-ctl; do
	echo "==> [container] deps:$p"
	install_pkgbuild_deps "$BUILD/$p"
	echo "==> [container] makepkg $p"
	(
		cd "$BUILD/$p"
		runuser -u builder -- env MIXUM_CARGO_HOME="$MIXUM_CARGO_HOME" \
			MIXUM_CARGO_TARGET_DIR="$MIXUM_CARGO_TARGET_DIR" _commit="$COMMIT" \
			makepkg -f --noconfirm
	)
done

echo "==> [container] 安装四包后构建元包 mixum-base"
pacman -U --noconfirm \
	"$BUILD"/criu/*.pkg.tar.* \
	"$BUILD"/mixcomp/*.pkg.tar.* \
	"$BUILD"/mixum-agentd/*.pkg.tar.* \
	"$BUILD"/mixum-ctl/*.pkg.tar.* >/dev/null
echo "==> [container] deps:mixum-base"
install_pkgbuild_deps "$BUILD/mixum-base"
echo "==> [container] makepkg mixum-base"
(
	cd "$BUILD/mixum-base"
	runuser -u builder -- env _commit="$COMMIT" makepkg -f --noconfirm
)

# ---- 产物核验:动态依赖可解析、无 vendored 库 ----------------------------
TMPV="$(mktemp -d)"
bsdtar -xf "$BUILD/mixcomp/"mixcomp-[0-9]*.pkg.tar.* -C "$TMPV" usr/bin/mixcomp
if ldd "$TMPV/usr/bin/mixcomp" | grep -q 'not found'; then
	echo "mixcomp 存在未解析的动态依赖:" >&2
	ldd "$TMPV/usr/bin/mixcomp" | grep 'not found' >&2
	exit 1
fi
if bsdtar -tf "$BUILD/mixcomp/"mixcomp-[0-9]*.pkg.tar.* | grep -Eq '\.(a|so)(\.|$)'; then
	echo "mixcomp 包内出现库文件(应动态链接系统库,不许 vendored):" >&2
	bsdtar -tf "$BUILD/mixcomp/"mixcomp-[0-9]*.pkg.tar.* | grep -E '\.(a|so)(\.|$)' >&2
	exit 1
fi
rm -rf "$TMPV"

# ---- 入库 ----------------------------------------------------------------
install -d "$OUT_REPO"
mv "$BUILD"/*/*.pkg.tar.* "$OUT_REPO/"
repo-add "$OUT_REPO/mixum.db.tar.zst" "$OUT_REPO"/*.pkg.tar.*

# ---- 构建环境记录(BUILDINFO 用) ----------------------------------------
{
	echo "== $ARCH $(date -u +%FT%TZ) commit=$COMMIT"
	pacman -Q pacman gcc git || true
	pacman -Q wlroots0.18 || true
} > "/out/build/envinfo-$ARCH.txt"

echo "==> [container] 完成:$OUT_REPO"
ls -1 "$OUT_REPO"
