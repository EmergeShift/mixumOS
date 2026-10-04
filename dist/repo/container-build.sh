#!/bin/bash
# 在 Arch 容器内部执行,两段式:
#   A. 自建 wlroots0.18 包 → 入 mini 仓库 → 容器 pacman 认领 [mixum] 源
#      (官方源已无 wlroots0.18,依赖必须先自给,见包内注释);
#   B. 按 PKGBUILD 声明装依赖 → makepkg 四包 → repo-add。
# 由 dist/repo/build-repo.sh 在容器里调用;不在宿主机直接跑。
#   用法:container-build.sh <架构名>        # aarch64 | x86_64
# 约定挂载:仓库只读挂 /build/mixumos,产物目录读写挂 /out;
# 前置:宿主已用 go 交叉编译出 /out/build/mixumd-<goarch>(占位源)。
set -euo pipefail

ARCH="${1:?用法: container-build.sh <aarch64|x86_64>}"
GIT_SRC="/build/mixumos"
BUILD="/build/pkgs"
OUT_REPO="/out/repo-$ARCH"
COMMIT="${_commit:-HEAD}"
case "$ARCH" in
	aarch64) GOARCH=arm64 ;;
	x86_64)  GOARCH=amd64 ;;
	*) echo "未知架构:$ARCH" >&2; exit 1 ;;
esac
MIXUMD_BIN="/out/build/mixumd-$GOARCH"

echo "==> [container] 架构 $ARCH,commit ${COMMIT:0:12}"

# ---- 0. 沙箱与密钥环(幂等;ALARM rootfs 的 keyring 未初始化,必须先做) --
# 容器里 pacman 的 Landlock 沙箱可能被 seccomp 拦,双保险关掉沙箱
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
pacman -Syu --noconfirm --needed base-devel git >/dev/null
git config --system --add safe.directory "$GIT_SRC"

[ -f "$MIXUMD_BIN" ] || {
	echo "缺宿主构建的占位二进制 $MIXUMD_BIN(宿主 go 交叉编译,见 build-repo.sh)" >&2
	exit 1
}

# 按 PKGBUILD 声明装依赖(不硬编码,避免两处漂移)
install_pkgbuild_deps() {
	(
		cd "$1"
		# shellcheck disable=SC1091
		source ./PKGBUILD
		pacman -S --noconfirm --needed \
			${depends[@]+"${depends[@]}"} \
			${makedepends[@]+"${makedepends[@]}"} \
			${checkdepends[@]+"${checkdepends[@]}"}
	)
}

# ---- A. wlroots0.18 自给 ------------------------------------------------
WL_FILE="$(ls "$OUT_REPO"/wlroots0.18-*.pkg.tar.* 2>/dev/null | head -1 || true)"
if [ -z "$WL_FILE" ]; then
	echo "==> [container] stage A:makepkg wlroots0.18"
	rm -rf "$BUILD/wlroots0.18"
	id builder >/dev/null 2>&1 || useradd -m builder
	install -d "$BUILD"
	cp -a "$GIT_SRC/dist/pkgs/wlroots0.18" "$BUILD/wlroots0.18"
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
echo "==> [container] stage B:按 PKGBUILD 声明装依赖"
rm -rf /build/src "$BUILD"
install -d "$BUILD"
# mixcomp 的 git 源:用宿主准备好的裸仓库(见 build-repo.sh),拷进容器
cp -a /out/src-bare.git /build/src
git config --system --add safe.directory /build/src
chmod -R a+rX /build/src
cp -a "$GIT_SRC/dist/pkgs/." "$BUILD/"
rm -rf "$BUILD/wlroots0.18"
install -d "$BUILD/mixum-agentd/placeholder"
# /out 是宿主挂载,执行位可能丢失,显式补
install -m755 "$MIXUMD_BIN" "$BUILD/mixum-agentd/placeholder/mixumd"
id builder >/dev/null 2>&1 || useradd -m builder
chown -R builder:builder "$BUILD"

# 三个组件包:装依赖 → makepkg(mixum-base 依赖它们,放最后单独处理)
for p in mixcomp mixum-agentd mixum-ctl; do
	echo "==> [container] deps:$p"
	install_pkgbuild_deps "$BUILD/$p"
	echo "==> [container] makepkg $p"
	(
		cd "$BUILD/$p"
		runuser -u builder -- env _commit="$COMMIT" makepkg -f --noconfirm
	)
done

echo "==> [container] 安装三包后构建元包 mixum-base"
pacman -U --noconfirm \
	"$BUILD"/mixcomp/*.pkg.tar.* \
	"$BUILD"/mixum-agentd/*.pkg.tar.* \
	"$BUILD"/mixum-ctl/*.pkg.tar.* >/dev/null
echo "==> [container] makepkg mixum-base"
(
	cd "$BUILD/mixum-base"
	runuser -u builder -- env _commit="$COMMIT" makepkg -f --noconfirm
)

# ---- 产物核验:动态依赖可解析、无 vendored 库 ----------------------------
TMPV="$(mktemp -d)"
bsdtar -xf "$BUILD/mixcomp/"mixcomp-*.pkg.tar.* -C "$TMPV" usr/bin/mixcomp
if ldd "$TMPV/usr/bin/mixcomp" | grep -q 'not found'; then
	echo "mixcomp 存在未解析的动态依赖:" >&2
	ldd "$TMPV/usr/bin/mixcomp" | grep 'not found' >&2
	exit 1
fi
if bsdtar -tf "$BUILD/mixcomp/"mixcomp-*.pkg.tar.* | grep -Eq '\.(a|so)(\.|$)'; then
	echo "mixcomp 包内出现库文件(应动态链接系统库,不许 vendored):" >&2
	bsdtar -tf "$BUILD/mixcomp/"mixcomp-*.pkg.tar.* | grep -E '\.(a|so)(\.|$)' >&2
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
