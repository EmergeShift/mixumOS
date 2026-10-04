#!/bin/bash
# 在 Arch 容器内部执行:依赖 → Go 占位二进制 → makepkg 四包 → repo-add。
# 由 dist/repo/build-repo.sh 在容器里调用;不在宿主机直接跑。
#   用法:container-build.sh <架构名>        # aarch64 | x86_64
# 约定挂载:仓库只读挂 /build/mixumos,产物目录读写挂 /out。
set -euo pipefail

ARCH="${1:?用法: container-build.sh <aarch64|x86_64>}"
GIT_SRC="/build/mixumos"
BUILD="/build/pkgs"
OUT_REPO="/out/repo-$ARCH"
COMMIT="${_commit:-HEAD}"

echo "==> [container] Arch 架构 $ARCH,commit ${COMMIT:0:12}"

# git 对只读挂载仓库做 clone 时的所有权豁免
git config --system --add safe.directory "$GIT_SRC"

# ---- 1. 构建依赖(幂等) ----------------------------------------------
# 装哪些包以 PKGBUILD 的声明为准,不在此硬编码,避免两处漂移。
pacman -Syu --noconfirm --needed base-devel git go
for d in "$GIT_SRC"/dist/pkgs/*/; do
	(
		cd "$d"
		# shellcheck disable=SC1091
		source ./PKGBUILD
		pacman -S --noconfirm --needed \
			${depends[@]+"${depends[@]}"} \
			${makedepends[@]+"${makedepends[@]}"} \
			${checkdepends[@]+"${checkdepends[@]}"}
	)
done

# ---- 2. Go 占位二进制(reference/mixumd,CGO 关,纯静态) -------------
mkdir -p /out/build
(
	cd "$GIT_SRC/reference"
	env CGO_ENABLED=0 GOFLAGS=-buildvcs=false \
		go build -trimpath -o "/out/build/mixumd-$ARCH" ./cmd/mixumd
)
echo "==> [container] mixumd 占位二进制就绪:$(sha256sum "/out/build/mixumd-$ARCH" | cut -c1-16)…(全量见 BUILDINFO)"

# ---- 3. 构建树:PKGBUILD 副本(容器内可写)+ 占位源 --------------------
rm -rf /build/src "$BUILD"
cp -a "$GIT_SRC" /build/src
chmod -R a+rX /build/src
git config --system --add safe.directory /build/src
cp -a "$GIT_SRC/dist/pkgs" "$BUILD"
install -d "$BUILD/mixum-agentd/placeholder"
cp "/out/build/mixumd-$ARCH" "$BUILD/mixum-agentd/placeholder/mixumd"

id builder >/dev/null 2>&1 || useradd -m builder
chown -R builder:builder "$BUILD"

# ---- 4. makepkg 四连(mixum-base 最后,它依赖前三者) ------------------
for p in mixcomp mixum-agentd mixum-ctl mixum-base; do
	echo "==> [container] makepkg $p"
	(
		cd "$BUILD/$p"
		runuser -u builder -- env _commit="$COMMIT" makepkg -f --noconfirm
	)
done

# ---- 5. 产物核验:无 vendored 库、动态链接可解析 ----------------------
TMPV="$(mktemp -d)"
bsdtar -xf "$BUILD/mixcomp/"mixcomp-*.pkg.tar.zst -C "$TMPV" usr/bin/mixcomp
if ldd "$TMPV/usr/bin/mixcomp" | grep -q 'not found'; then
	echo "mixcomp 存在未解析的动态依赖:" >&2
	ldd "$TMPV/usr/bin/mixcomp" | grep 'not found' >&2
	exit 1
fi
if bsdtar -tf "$BUILD/mixcomp/"mixcomp-*.pkg.tar.zst | grep -Eq '\.(a|so)(\.|$)'; then
	echo "mixcomp 包内出现库文件(应动态链接系统库,不许 vendored):" >&2
	bsdtar -tf "$BUILD/mixcomp/"mixcomp-*.pkg.tar.zst | grep -E '\.(a|so)(\.|$)' >&2
	exit 1
fi
rm -rf "$TMPV"

# ---- 6. 入库 repo-add ------------------------------------------------
install -d "$OUT_REPO"
mv "$BUILD"/*/*.pkg.tar.zst "$OUT_REPO/"
repo-add "$OUT_REPO/mixum.db.tar.zst" "$OUT_REPO"/*.pkg.tar.zst

# ---- 7. 构建环境记录(BUILDINFO 用) ----------------------------------
{
	echo "== $ARCH $(date -u +%FT%TZ) commit=$COMMIT"
	pacman -Q pacman gcc go git || true
	go version || true
} > "/out/build/envinfo-$ARCH.txt"

echo "==> [container] 完成:$OUT_REPO"
ls -1 "$OUT_REPO"
