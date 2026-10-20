# CRIU 可行性实验脚本

配套报告:[`docs/criu-feasibility.md`](../../../docs/criu-feasibility.md)。
受控实验协议(§5)由脚本强制,顺序不可省略。

## 协议三步(跨内核实验)

```bash
# ① 源内核:dump 静态计数器,写 meta.txt(CRIU 版本/内核/arch)
sudo tests/integration/criu/dump-for-cross-kernel.sh /tmp/criu-imgs-cross
```

把镜像目录带进第二内核 VM(rootfs/initramfs),然后在该 VM 内:

```bash
# ② 目标端 control:必须先 PASS(本地同内核 dump/restore)
sudo tests/integration/criu/target-control.sh /tmp/criu-imgs-cross

# ③ 跨内核 restore:脚本断言「CRIU 版本一致 + control-ok 存在」后才执行
sudo tests/integration/criu/restore-for-cross-kernel.sh /tmp/criu-imgs-cross
```

- ② 失败 ⇒ 目标环境无效,实验终止,**不得**进入 ③;
- ③ 的 CRIU 版本与 meta.txt 不一致 ⇒ 脚本中止(版本是混杂变量,不允许);
- ③ 失败后按报告 §5 对失败分类(环境/版本/内核)才能下结论。

## 同内核基线(可重复,需 root)

```bash
sudo tests/integration/criu/baseline-same-kernel.sh
```

要求 `criu`、`python3`,且 `criu check` 通过。脚本会 dump 一个独立会话计数器,
确认进程被杀后 restore,并检查计数继续递增。

## 第二内核环境(报告的复现要点)

- 内核:与镜像来源不同的版本(实验用 Debian 6.1.0-50-amd64);
  需要与内核**严格匹配**的 modules 包;
- rootfs:含 CRIU(与源端**同版本**;实验中的 3.16.1 vs 4.2.1 已判定为
  方法学错误)的 rootfs,打成 initramfs;
- 加载模块:`crc32c_generic` → `libcrc32c` → `nf_tables` / `ip_tables`;
- 打开 loopback(`ip link set lo up`),否则 CRIU 的 kerndat 探测失败;
- 用 QEMU/KVM 启动:
  `qemu-system-x86_64 -enable-kvm -kernel vmlinuz -initrd rootfs.cpio.gz -append "console=ttyS0 rdinit=/init"`。

## 文件

| 文件 | 用途 |
|---|---|
| `baseline-same-kernel.sh` + `counter.py` | 同内核 dump/restore 基线 |
| `dump-for-cross-kernel.sh` + `static_counter.c` | 源端 dump + 元数据(协议①) |
| `target-control.sh` + `counter.py` | 目标端本地 control(协议②) |
| `restore-for-cross-kernel.sh` | 受控跨内核 restore(协议③) |
