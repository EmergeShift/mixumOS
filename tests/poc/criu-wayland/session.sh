#!/bin/sh
export XDG_RUNTIME_DIR=/tmp/xdg
export WAYLAND_DISPLAY=wayland-1
export WLR_BACKENDS=headless
export WLR_LIBINPUT_NO_DEVICES=1
export WLR_RENDERER=pixman
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
sway --unsupported-gpu -c /dev/null >/tmp/sway.log 2>&1 &
sleep 3
foot --hold sh -c 'sleep 1' >/tmp/foot.log 2>&1 &
wait
