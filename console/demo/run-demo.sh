#!/bin/sh
# 演示容器:mixumd + fakecomp(模拟窗口帧),供控制台窗口画布开发演示。
# 不动 mixum-e2e 调试容器,独立起一个,端口 17602。
# 用法:console/demo/run-demo.sh(需已构建 mixumos-e2e 镜像)
# 结束:docker rm -f mixum-demo
set -e
cd "$(dirname "$0")"

IMG=mixumos-e2e:arm64
NAME=mixum-demo
PORT=17602

docker rm -f "$NAME" 2>/dev/null || true
docker run -d --name "$NAME" -p "$PORT:7601" --entrypoint /bin/sh "$IMG" -c '
  mkdir -p /run/mixum && chmod 700 /run/mixum
  export XDG_RUNTIME_DIR=/run/mixum
  mixumd --config /dev/null > /tmp/mixumd.log 2>&1 &
  for i in $(seq 1 50); do
    grep -q READY /tmp/mixumd.log 2>/dev/null && break
    sleep 0.2
  done
  echo READY
  sleep infinity
'
sleep 0.5

# 拷入 fakecomp,容器内编译后后台运行
docker cp fakecomp.c "$NAME":/tmp/fakecomp.c
docker exec "$NAME" sh -c '
  gcc /tmp/fakecomp.c -o /usr/local/bin/fakecomp -ljpeg -lm
  nohup fakecomp > /tmp/fakecomp.log 2>&1 &
  sleep 1
  head -2 /tmp/fakecomp.log
'

TOKEN=$(docker exec "$NAME" sh -c 'cat /var/lib/mixum/admin-token')
echo
echo "地址: wss://127.0.0.1:$PORT"
echo "令牌: $TOKEN"
echo "把上面两行粘进控制台「设置」即可看到模拟窗口。"
