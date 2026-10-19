# CRIU 可行性实验脚本

配套报告:[`docs/criu-feasibility.md`](../../../docs/criu-feasibility.md)。

## 同内核基线(可重复,需 root)

```bash
sudo tests/integration/criu/baseline-same-kernel.sh
```

要求 `criu`、`python3`,且 `criu check` 通过。脚本会 dump 一个独立会话计数器,
确认进程被杀后 restore,并检查计数继续递增。

## 跨内核素材

```bash
sudo tests/integration/criu/dump-for-cross-kernel.sh /tmp/criu-imgs-cross
```

在源内核上编译静态计数器并 dump。镜像目录随后放进第二内核的 rootfs/initramfs,
在该内核里执行:

```bash
criu restore --images-dir /tmp/criu-imgs-cross --shell-job --restore-detached
```

## 第二内核环境(报告中实验的复现要点)

- 内核:与镜像来源不同的版本(实验用 Debian 6.1.0-50-amd64);
  需要与内核**严格匹配**的 modules 包;
- rootfs:含 CRIU(实验用 Ubuntu 22.04)的 rootfs,打成 initramfs;
- 加载模块:`crc32c_generic` → `libcrc32c` → `nf_tables` / `ip_tables`;
- 打开 loopback(`ip link set lo up`),否则 CRIU 的 kerndat 探测失败;
- 用 QEMU/KVM 启动:
  `qemu-system-x86_64 -enable-kvm -kernel vmlinuz -initrd rootfs.cpio.gz -append "console=ttyS0 rdinit=/init"`。

实验的失败点(tty 会话、mount 命名空间、内核特性)见报告 §4;这些不是脚本
能绕过的“参数问题”,而是 CRIU 镜像与源环境强耦合的体现。

## 文件

| 文件 | 用途 |
|---|---|
| `baseline-same-kernel.sh` + `counter.py` | 同内核 dump/restore 基线 |
| `dump-for-cross-kernel.sh` + `static_counter.c` | 跨内核实验的源侧 dump |
