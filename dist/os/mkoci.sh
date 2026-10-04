#!/bin/bash
# rootfs tar → OCI 镜像 tar(纯 shell 实现,不依赖 docker daemon)。
#   用法:mkoci.sh <rootfs.tar> <docker架构> <out.oci.tar> [保留layout目录]
#   或:  mkoci.sh --merge <layout1> <arch1> <layout2> <arch2> ... <out.tar>
# 产物为标准 OCI layout 打包(oci-layout + index.json + blobs/sha256/);
# 写完即自校验:逐 blob 重算 sha256 必须与文件名一致。
# 传第 4 参时把 layout 目录留在该路径(供 --merge 合并多架构)。
set -euo pipefail

LAYER_MT="application/vnd.oci.image.layer.v1.tar+gzip"
CONF_MT="application/vnd.oci.image.config.v1+json"
MANI_MT="application/vnd.oci.image.manifest.v1+json"
IDX_MT="application/vnd.oci.image.index.v1+json"
NOW="$(date -u +%FT%TZ)"

json_write() { # 紧凑写 JSON,$1 目标文件,其余 printf
	printf '%s' "$2" > "$1"
}

# ---------- 单架构模式 ----------
if [ "${1:-}" != "--merge" ]; then
	ROOTFS="${1:?用法: mkoci.sh <rootfs.tar> <arch> <out.oci.tar> [keep-layout]}"
	ARCH="${2:?缺 docker 架构(arm64|amd64)}"
	OCI_TAR="${3:?缺输出路径}"
	KEEP_LAYOUT="${4:-}"
	LAYOUT="$(mktemp -d)"
	if [ -z "$KEEP_LAYOUT" ]; then
		trap 'rm -rf "$LAYOUT"' EXIT
	else
		rm -rf "$KEEP_LAYOUT"
		trap 'mv "$LAYOUT" "$KEEP_LAYOUT"' EXIT
	fi
	mkdir -p "$LAYOUT/blobs/sha256"

	# 层:gzip 压缩整棵 rootfs;diff_id 为未压缩 tar 的 sha256
	LAYER="$(mktemp)"
	gzip -c "$ROOTFS" > "$LAYER"
	LD="$(sha256sum "$LAYER" | cut -d' ' -f1)"
	LSZ="$(stat -c%s "$LAYER")"
	DID="$(sha256sum "$ROOTFS" | cut -d' ' -f1)"
	mv "$LAYER" "$LAYOUT/blobs/sha256/$LD"

	CONF="$(mktemp)"
	json_write "$CONF" "{\"architecture\":\"$ARCH\",\"os\":\"linux\",\"created\":\"$NOW\",\"config\":{\"Entrypoint\":[\"/entrypoint.sh\"],\"ExposedPorts\":{\"7601/tcp\":{}},\"Env\":[\"PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\"]},\"rootfs\":{\"type\":\"layers\",\"diff_ids\":[\"sha256:$DID\"]},\"history\":[{\"created\":\"$NOW\",\"created_by\":\"mixumOS dist 工具链(rootfs 由 pacman 仓库组装)\"}]}"
	CD="$(sha256sum "$CONF" | cut -d' ' -f1)"
	CSZ="$(stat -c%s "$CONF")"
	mv "$CONF" "$LAYOUT/blobs/sha256/$CD"

	MANI="$(mktemp)"
	json_write "$MANI" "{\"schemaVersion\":2,\"mediaType\":\"$MANI_MT\",\"config\":{\"mediaType\":\"$CONF_MT\",\"digest\":\"sha256:$CD\",\"size\":$CSZ},\"layers\":[{\"mediaType\":\"$LAYER_MT\",\"digest\":\"sha256:$LD\",\"size\":$LSZ}],\"annotations\":{\"org.opencontainers.image.title\":\"mixumOS\",\"org.opencontainers.image.description\":\"镜像由 dist 工具链产出(pacman 仓库组装)\"}}"
	MD="$(sha256sum "$MANI" | cut -d' ' -f1)"
	MSZ="$(stat -c%s "$MANI")"
	mv "$MANI" "$LAYOUT/blobs/sha256/$MD"

	json_write "$LAYOUT/index.json" "{\"schemaVersion\":2,\"mediaType\":\"$IDX_MT\",\"manifests\":[{\"mediaType\":\"$MANI_MT\",\"digest\":\"sha256:$MD\",\"size\":$MSZ,\"platform\":{\"architecture\":\"$ARCH\",\"os\":\"linux\"},\"annotations\":{\"org.opencontainers.image.ref.name\":\"mixumos:$ARCH\"}}]}"
	json_write "$LAYOUT/oci-layout" '{"imageLayoutVersion":"1.0.0"}'

	echo "==> mkoci:layer=$LD config=$CD manifest=$MD"
else
	# ---------- 多架构合并 ----------
	shift
	[ "$(($# % 2))" -eq 1 ] || { echo "--merge 参数须为 <layout> <arch> 成对 + 输出" >&2; exit 1; }
	OCI_TAR="${*: -1}"
	set -- "${@:1:$#-1}"
	LAYOUT="$(mktemp -d)"
	trap 'rm -rf "$LAYOUT"' EXIT
	mkdir -p "$LAYOUT/blobs/sha256"
	ENTRIES=""
	while [ "$#" -gt 0 ]; do
		SRC="$1" A="$2"; shift 2
		cp -a "$SRC"/blobs/sha256/. "$LAYOUT/blobs/sha256/"
		MD="$(python3 - "$SRC/index.json" <<'EOF'
import json, sys
idx = json.load(open(sys.argv[1]))
m = idx["manifests"][0]
print(m["digest"] + " " + str(m["size"]))
EOF
)"
		MDIGEST="${MD%% *}"; MDSIZE="${MD##* }"
		ENTRIES="$ENTRIES,{\"mediaType\":\"$MANI_MT\",\"digest\":\"$MDIGEST\",\"size\":$MDSIZE,\"platform\":{\"architecture\":\"$A\",\"os\":\"linux\"}}"
	done
	ENTRIES="${ENTRIES#,}"
	json_write "$LAYOUT/index.json" "{\"schemaVersion\":2,\"mediaType\":\"$IDX_MT\",\"manifests\":[$ENTRIES]}"
	json_write "$LAYOUT/oci-layout" '{"imageLayoutVersion":"1.0.0"}'
	MD="merged($ENTRIES)"
	echo "==> mkoci --merge:$MD"
fi

# ---- 自校验:逐 blob 重算 sha256 必须与文件名一致 ----------------------
( cd "$LAYOUT/blobs/sha256" &&
	for f in *; do printf '%s  %s\n' "$f" "$f"; done | sha256sum -c --quiet ) || {
	echo "OCI 自校验失败:blob 摘要与文件名不一致" >&2
	exit 1
}

mkdir -p "$(dirname "$OCI_TAR")"
tar -C "$LAYOUT" -cf "$OCI_TAR" .
echo "==> OCI 就绪:$OCI_TAR"
