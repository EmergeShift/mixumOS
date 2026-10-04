#!/bin/sh
# 把 mixumd 的自签证书加入 macOS 登录钥匙串信任,供浏览器直连 wss://。
# 仅限本机开发。用法:trust-cert.sh [容器名] [端口]
set -e
CONTAINER=${1:-mixum-e2e}
PORT=${2:-17601}

command -v openssl >/dev/null || { echo "需要 openssl"; exit 1; }
docker ps --format '{{.Names}}' | grep -qx "$CONTAINER" || {
  echo "容器 $CONTAINER 未运行;先 docker run -d -p $PORT:7601 $CONTAINER"; exit 1
}
# 端口要映射到本机;容器名仅用于存在性检查

OUT=$(mktemp -d /tmp/mixum-cert.XXXXXX)
echo | openssl s_client -connect 127.0.0.1:$PORT 2>/dev/null \
  | awk '/BEGIN CERTIFICATE/,/END CERTIFICATE/' > "$OUT/server.pem"

if [ ! -s "$OUT/server.pem" ]; then
  echo "取证书失败:openssl s_client 连 127.0.0.1:$PORT 没有返回证书"; exit 1
fi

echo "证书 SHA-256 指纹:"
openssl x509 -in "$OUT/server.pem" -noout -fingerprint -sha256

security add-trusted-cert -r trustRoot \
  -k "$HOME/Library/Keychains/login.keychain-db" "$OUT/server.pem"
echo "已加入登录钥匙串信任(系统可能弹一次确认框)。"
echo "浏览器直连 wss://127.0.0.1:$PORT 应可用;控制台设置里填该地址即可。"
