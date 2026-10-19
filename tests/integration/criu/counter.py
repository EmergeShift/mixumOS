#!/usr/bin/env python3
"""独立会话的计数器进程,供 CRIU 基线实验使用。

用法: counter.py <pidfile> <counterfile>
进程会成为会话首进程并关闭继承 fd(避免 tty 记录进入 CRIU 镜像)。
"""
import os
import sys
import time

pidfile, counterfile = sys.argv[1], sys.argv[2]

try:
    os.setsid()
except OSError:
    if os.fork():
        os._exit(0)
    os.setsid()

for fd in range(3, 256):
    try:
        os.close(fd)
    except OSError:
        pass

with open(pidfile, "w") as handle:
    handle.write(str(os.getpid()))

i = 0
while True:
    i += 1
    with open(counterfile, "w") as handle:
        handle.write(str(i))
    time.sleep(0.2)
