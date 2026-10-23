#!/bin/bash
# mixumOS rootfs → WSL2 发行版一键导入(Windows 宿主,零 Docker)。
# 用法(在 git-bash/WSL 可用的 shell 里):
#   bash dist/os/wsl-import.sh [rootfs.tar] [发行版名]
# 缺省:dist/out/rootfs-x86_64.tar,发行版名 mixumos
# 前置:Windows WSL2(WSL 0.x/1.x/2.x 均可,systemd 需 WSL ≥0.67.6)。
# 产物:systemd 启用、mixumd 开机自启;令牌打印在最后。
# 窗口流:GUI 应用以 mixum 用户连接 WAYLAND_DISPLAY=wayland-0
#         (XDG_RUNTIME_DIR=/run/mixum);需自行安装应用与字体
#         (pacman -S foot ttf-dejavu)。
set -euo pipefail

TAR="${1:-$(cd "$(dirname "$0")/../.." && pwd)/dist/out/rootfs-x86_64.tar}"
NAME="${2:-mixumos}"
INSTALL_DIR="${MIXUM_WSL_DIR:-C:\\mixumos-wsl}"

[ -f "$TAR" ] || { echo "rootfs 不存在:$TAR(先 make dist-image ARCH=x86_64)" >&2; exit 1; }

WTAR="$(cygpath -w "$TAR" 2>/dev/null || echo "$TAR")"

echo "==> 导入 $WTAR → WSL 发行版 $NAME"
wsl --import "$NAME" "$INSTALL_DIR" "$WTAR"
echo "==> 启用 systemd(重启生效)"
wsl -d "$NAME" -u root -- sh -c \
	'printf "[boot]\nsystemd=true\n\n[user]\ndefault=root\n" > /etc/wsl.conf'
wsl --terminate "$NAME"
sleep 2
echo "==> 启动 mixumd(开机自启)"
wsl -d "$NAME" -u root -- sh -c \
	'systemctl enable --now pulseaudio-mixum 2>/dev/null || true
	 systemctl enable --now mixumd 2>&1; sleep 2; systemctl is-active mixumd'
echo "==> (可选)窗口流:安装测试应用与字体,并启用 mixcomp"
wsl -d "$NAME" -u root -- sh -c \
	'pacman -Sy --noconfirm --needed foot ttf-dejavu noto-fonts-cjk noto-fonts noto-fonts-emoji >/dev/null 2>&1 || echo "foot 安装失败(不影响 mixumd)"
	 systemctl enable --now mixcomp 2>&1 >/dev/null' || true

TOKEN="$(wsl -d "$NAME" -u root -- cat /var/lib/mixum/admin-token 2>/dev/null | tr -d '\r\n')"
echo "==> 完成。Windows 直连: wss://127.0.0.1:7601"
echo "==> 能力令牌: $TOKEN"
echo "==> 快照验证: node tests/integration/wsl-probe.mjs <令牌> [snap1|restore <id>]"
