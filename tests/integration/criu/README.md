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

# ③ 跨内核 restore:脚本断言受控条件后才执行
sudo tests/integration/criu/restore-for-cross-kernel.sh /tmp/criu-imgs-cross
```

- ② 失败 ⇒ 目标环境无效,实验终止,**不得**进入 ③;
- ③ 的 CRIU 版本与 meta.txt 不一致 ⇒ 脚本中止(版本是混杂变量,不允许);
- ③ 要求 control-ok 的 CRIU/内核/arch 与当前目标一致且生成于 24h 内;
  两端内核相同 ⇒ 脚本中止(同内核恢复请用 `baseline-same-kernel.sh`);
- ③ 在 restore 前拒绝残留的 `criu-static-counter` 进程,并确认计数器静止,
  避免旧进程造成假 PASS;
- ③ 失败后按报告 §5 对失败分类(环境/版本/内核)才能下结论。

环境对齐要点(2026-10-20 受控重跑踩实):目标端除内核外必须与源端
一致——initramfs 需先挂 devtmpfs;Debian 内核包需构建侧 `depmod`;
被测进程的 stdio/cwd/文件 fd 不能指向源机专用路径(dump 脚本已把
stdio 收进 /dev/null、cwd 固定 `/`);计数器文件需按源端 dump 后值
预建同尺寸(restore 脚本已完成)。这些属于「环境」类,不消除就无法把
失败归因到内核。

## 同内核基线(可重复,需 root)

```bash
sudo tests/integration/criu/baseline-same-kernel.sh
```

要求 `criu`、`python3`,且 `criu check` 通过。脚本会 dump 一个独立会话计数器,
确认进程被杀后 restore,并检查计数继续递增。

## 第二内核环境(报告的复现要点)

- 内核:与镜像来源不同的版本(实验用 Debian 6.1.0-47-amd64);
  需要与内核**严格匹配**的 modules 包;
- rootfs:Arch rootfs + CRIU 4.2.1(与源端**同版本**),打成 initramfs;
  旧的 3.16.1 vs 4.2.1 版本混用实验已判定为方法学错误,不再使用;
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
