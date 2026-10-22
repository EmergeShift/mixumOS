# PoC：QEMU virt 启动 linux-mixum full

## 实测（2026-10-22）

```text
Linux version 6.18.35-mixum ... #1 SMP PREEMPT aarch64
Freeing unused kernel memory: 13248K
Run /init as init process
FULL_KERNEL_BOOT_OK
Linux (none) 6.18.35-mixum ... aarch64 Linux
~ #
```

## 运行

```bash
./tests/poc/qemu-full/run.sh \
  /path/to/kernel/out/linux-full-arm64/Image 1024
```

脚本会从 Alpine 取静态 musl 的 busybox 与 loader，组一个最小 initramfs；
内核启动后执行 `/init`，打印 `FULL_KERNEL_BOOT_OK` 并给 shell。

## 判定标准

- 串口出现 `Linux version 6.18.35-mixum`
- 出现 `FULL_KERNEL_BOOT_OK`
- `uname -a` 显示 `6.18.35-mixum`、`aarch64`
- 无 `Kernel panic`

## 边界

这是内核 + 最小 initramfs 的启动验收，不是发行版 rootfs 验收；
Apple Container 仍不能直接启动 full（它需要 kata 风格的 guest initramfs）。
