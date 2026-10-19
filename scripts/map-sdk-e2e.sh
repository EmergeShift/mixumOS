#!/usr/bin/env bash
# MAP SDK(TypeScript)↔ mixum-agentd 全环 e2e。
#
#   scripts/map-sdk-e2e.sh
#
# 依赖:node ≥22、cargo、已安装的 SDK 依赖(npm ci,首次会安装)。
# 环境变量:
#   MAP_SDK_DIR  TS SDK 目录(默认 ../Mixum-Agent-Protocol/sdk/typescript)
#   CARGO_REGISTRIES_CRATES_IO_INDEX  默认 rsproxy(见 HANDOFF §0)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SDK_DIR="${MAP_SDK_DIR:-"$ROOT/../Mixum-Agent-Protocol/sdk/typescript"}"
export CARGO_REGISTRIES_CRATES_IO_INDEX="${CARGO_REGISTRIES_CRATES_IO_INDEX:-sparse+https://rsproxy.cn/index/}"

[ -d "$SDK_DIR" ] || {
	echo "找不到 TS SDK:$SDK_DIR(用 MAP_SDK_DIR 指定)" >&2
	exit 1
}

echo "==> 构建 SDK"
(
	cd "$SDK_DIR"
	[ -d node_modules ] || npm ci --no-audit --no-fund
	npm run build
)

echo "==> 构建 agentd 测试宿主(明文 WS,临时 dataDir)"
(
	cd "$ROOT/rust"
	cargo build -p mixum-agentd --example map_harness
)
HARNESS="$ROOT/rust/target/debug/examples/map_harness"
OUT="$(mktemp -t map-harness.XXXXXX)"
ERR="$(mktemp -t map-harness-err.XXXXXX)"
"$HARNESS" >"$OUT" 2>"$ERR" &
PID=$!
cleanup() {
	kill "$PID" 2>/dev/null || true
	rm -f "$OUT" "$ERR"
}
trap cleanup EXIT

for _ in $(seq 1 100); do
	grep -q '^READY ' "$OUT" 2>/dev/null && break
	sleep 0.1
done
LINE="$(grep '^READY ' "$OUT" || true)"
if [ -z "$LINE" ]; then
	echo "agentd 测试宿主未就绪;stderr:" >&2
	cat "$ERR" >&2
	exit 1
fi
read -r URL TOKEN < <(node -e 'const j=JSON.parse(process.argv[1].slice(6)); console.log(j.url+" "+j.token)' "$LINE")

echo "==> 运行 SDK e2e($URL)"
cd "$SDK_DIR"
MAP_URL="$URL" MAP_TOKEN="$TOKEN" node test/e2e-agentd.mjs
