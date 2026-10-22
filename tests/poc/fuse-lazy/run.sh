#!/bin/sh
# 在 Docker/Colima 里跑 FUSE 惰性挂载 PoC（需要 --privileged 暴露 /dev/fuse）。
set -e
[ -x /poc/passthrough ] || cc -O2 -o /poc/passthrough "$(dirname "$0")/passthrough.c" -lfuse3
mkdir -p /src /mnt
echo hello > /src/hello.txt
dd if=/dev/zero of=/src/big.bin bs=1M count=32 status=none
: > /tmp/fuse-reads.log
/poc/passthrough /src /mnt -f &
pid=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do
  [ -e /mnt/hello.txt ] && break
  sleep 0.1
done
start=$(date +%s%N)
ls /mnt >/dev/null
end=$(date +%s%N)
echo "mount-ready-us=$(( (end - start) / 1000 ))"
cat /mnt/hello.txt
cat /mnt/big.bin >/dev/null
echo "--- read log (first 5) ---"
head -5 /tmp/fuse-reads.log
echo "read-entries=$(wc -l < /tmp/fuse-reads.log)"
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
