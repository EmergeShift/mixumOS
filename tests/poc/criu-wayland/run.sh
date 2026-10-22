#!/usr/bin/env bash
# 在 Apple Container + linux-mixum-micro 上复现 CRIU + Wayland PoC。
# 需要：container CLI、--cap-add ALL、已安装自定义内核。
set -euo pipefail
cd "$(dirname "$0")"
container build -t criu-poc:1 .
container run -d --name criu-poc --cap-add ALL criu-poc:1
trap 'container stop criu-poc >/dev/null 2>&1 || true' EXIT

container exec criu-poc sh -c \
  'su -s /bin/sh demo -c "nohup /usr/local/bin/session.sh >/tmp/session.log 2>&1 &"'
sleep 6

session=$(container exec criu-poc pgrep -f /usr/local/bin/session.sh | head -1)
echo "session=$session"
container exec criu-poc sh -c "rm -rf /tmp/d; mkdir -p /tmp/d; \
  criu dump --tree $session --images-dir /tmp/d --shell-job --ext-unix-sk \
  --ghost-limit 2147483648 --file-locks -v3"

container exec criu-poc sh -c 'pkill -9 -f /usr/local/bin/session.sh || true; pkill -9 sway || true; pkill -9 foot || true'
sleep 2
container exec criu-poc criu restore --images-dir /tmp/d --shell-job --ext-unix-sk -v3

echo "--- 恢复后的窗口树 ---"
container exec criu-poc sh -c 'export XDG_RUNTIME_DIR=/tmp/xdg; \
  export SWAYSOCK=$(ls /tmp/xdg/sway-ipc.*.sock | head -1); \
  swaymsg -t get_tree | grep -E "app_id|name" | head'
