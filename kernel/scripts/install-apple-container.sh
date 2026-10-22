#!/usr/bin/env bash
# 把构建好的 linux-mixum-micro 安装为 Apple Container 的 guest 内核。
#
# 这是系统级设置：会影响本机所有 Apple Container 容器。
# 用法: ./kernel/scripts/install-apple-container.sh <Image> [--yes]
set -euo pipefail

KERNEL="${1:-}"
YES=0
[[ "${2:-}" == "--yes" ]] && YES=1

if [[ -z "$KERNEL" || ! -f "$KERNEL" ]]; then
  echo "用法: $0 <Image> [--yes]" >&2
  exit 2
fi
command -v container >/dev/null || { echo "未找到 container CLI（Apple Container）" >&2; exit 1; }

cat <<EOF
即将把以下内核安装为 Apple Container 的系统级 guest 内核：
  $KERNEL

影响：本机所有 Apple Container 容器都会使用这个内核启动。
卸载/回滚：重新执行 container system kernel set --binary 指向原内核，
或参考 docs 里的回滚步骤。
EOF

if [[ "$YES" != "1" ]]; then
  read -r -p "继续？[y/N] " answer
  [[ "$answer" == "y" || "$answer" == "Y" ]] || { echo "已取消"; exit 1; }
fi

container system kernel set --binary "$KERNEL"
container system status
echo "完成。新建容器会使用新内核；已运行容器不受影响。"
