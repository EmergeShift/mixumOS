# CRIU 跨内核迁移可行性验证

状态:验证完成(2026-10-19),结论:**同内核可用,跨内核透明迁移不可行**。
关联:roadmap 阶段 5 最后一项;`docs/snapshots.md` §6;`mixum-snapshot/src/criu.rs`。

## 1. 结论摘要

- **同内核 dump → restore 实测可用**:WSL2 6.18 上 dump 一个持续写计数器的
  进程,进程被冻结/杀死后从镜像恢复并继续计数。
- **跨内核恢复失败**:在 QEMU/KVM 启动的第二内核(Debian 6.1.0-50)里恢复
  WSL 6.18 的镜像,CRIU 在会话/终端与内核特性耦合处失败,无法完成恢复。
- 失败不是单一的“版本号比较”,而是 **内核特性集 + 原进程的
  tty/session/mount 命名空间环境** 共同耦合。这正是现有
  `Engine::restore` 用 `kernel`/`arch` 精确指纹拒绝跨环境恢复的原因。
- **不建议**为跨内核恢复放宽指纹;工程上采用同内核节点池 + 精确调度,
  不匹配时降级为 disk 快照 + 重启。

## 2. 实验环境

| 环境 | 内核 | CRIU | 用途 |
|---|---|---|---|
| WSL2(Arch) | 6.18.54.1-microsoft-standard-WSL2 | 4.2.1 | dump 源 + 同内核基线 |
| Docker Desktop / kind 节点 | 同上(共享 WSL2 内核) | — | 说明容器与宿主同内核 |
| QEMU/KVM 第二内核 VM | 6.1.0-50-amd64(Debian) | 3.16.1 + 匹配 modules | 跨内核恢复目标 |

第二内核用 Debian netboot `vmlinuz` + 以 Ubuntu 22.04 rootfs(含 CRIU 3.16)
构建的 initramfs 启动;CRIU 依赖的 `crc32c_generic`/`libcrc32c`/`nf_tables`/
`ip_tables` 模块来自与内核严格匹配的 `linux-image-6.1.0-50` 包。

## 3. 实验一:同内核基线(PASS)

流程(脚本见 `tests/integration/criu/baseline-same-kernel.sh`):

1. 启动一个独立会话(`setsid`,无控制终端)的计数器进程;
2. `criu dump --tree <pid> --images-dir <dir> --shell-job`;
3. 确认进程被杀、计数停止;
4. `criu restore --images-dir <dir> --shell-job --restore-detached`;
5. 确认进程恢复且计数继续递增。

实测(WSL 6.18,CRIU 4.2.1):

```text
DUMP_EXIT=0
AFTER_DUMP  before=6 after=6     # 进程已被杀死,计数停住
RESTORE_EXIT=0
RESTORED_COUNTER=14              # 恢复后继续计数
Restore finished successfully. Tasks resumed.
```

要点(踩坑记录):

- `--images-dir` **必须预先存在**,CRIU 不会替你创建;
- 被 dump 进程必须是 **独立会话首进程且无控制终端**,否则恢复报
  `tty: No task found with sid ...`;
- 容器/壳层继承的 fd(尤其 pty)在 dump 前应关闭,否则 tty 记录会跟着镜像走;
- 静态链接的被测进程更适合跨环境实验(不依赖目标 rootfs 的 libc)。

## 4. 实验二:跨内核恢复(FAIL)

拓扑:WSL 6.18 上 dump 静态计数器 → 把镜像放进第二内核 VM 的 initramfs →
在 6.1 VM 内 `criu restore`。

结果与归因:

1. **内核特性依赖**:VM 初始缺 netfilter 模块时,CRIU 在
   `kerndat_has_nftables_concat` 阶段直接失败(`Can't wait or bad status`);
   补 `crc32c_generic`/`libcrc32c`/`nf_tables`/`ip_tables` 后特性探测才通过。
   说明 CRIU 的可用性取决于**内核配置/模块集**,不只是版本号。
2. **会话/终端状态耦合**:镜像恢复在
   `tty: No task found with sid 277` 处失败。dump 时进程虽已是会话首进程,
   镜像仍携带源环境的 tty/session 引用;目标 VM 没有同 sid 的任务,
   恢复直接失败。
3. **VM 内本地 dump 也未产出可用镜像**(`Unexpected EOF on (empty-image)`),
   说明最小 initramfs 环境与 CRIU 的运行时假设仍有差距。

结论:在第二内核里“透明恢复”源内核的完整进程状态,需要同时复现
**内核版本、内核特性/模块、tty/会话与 mount 命名空间**;这实际上等于
复现源宿主的运行时,不能作为跨设备迁移的通用手段。

## 5. 与现有实现的关系

- `mixum-snapshot/src/engine.rs`:dump 时记录 `kernel`/`arch`,restore 时
  精确比较并返回 `KernelMismatch`/`ArchMismatch`。本次验证支持保留该闸门。
- `mixum-snapshot/src/criu.rs`:`--shell-job` 的用法正确;同内核路径可用。
- `docs/snapshots.md` §6 的“跨内核恢复不可靠”结论得到实测支持。

## 6. 工程对策

1. **保留精确指纹**:kernel release + arch 必须完全相同才允许 memory restore;
   错误信息明确提示“需要同内核节点”,不做静默降级。
2. **同内核节点池**:节点打 `mixumos.io/kernel` 标签(如 operator 的
   `arch` nodeSelector 同款机制),memory 快照的 restore 只调度到同内核节点;
   disk 快照不受此限。
3. **降级路径**:目标内核不同 → memory 快照不可用,改用 disk 快照 +
   任务重启(记录 RTO);用户可在控制台看到降级原因。
4. **同内核热迁移**:笔记本 → 云端使用同一发行版内核镜像(`dist` 固定
   内核版本)时,CRIU 迁移是可行的;这是近期可落地的“合盖上云”形态。
5. **长期方案**:需要真正跨内核时,走 VM 级 live migration
   (Firecracker/Cloud Hypervisor)或在快照中携带可引导内核,
   而不是放宽 CRIU 指纹。
6. **测试策略**:同内核 dump/restore 可进 privileged nightly e2e;
   跨内核只保留实验脚本,不作为 CI 门槛。

## 7. 复现

```bash
# 同内核基线(需 root + criu)
sudo tests/integration/criu/baseline-same-kernel.sh

# 跨内核素材:在源内核上 dump 静态计数器,镜像交给第二内核 VM
sudo tests/integration/criu/dump-for-cross-kernel.sh

# 第二内核 VM:按 tests/integration/criu/README.md 组装 initramfs,
# 用 qemu-system-x86_64 -enable-kvm 启动后执行 criu restore
```

脚本只包含可移植的测试逻辑;内核、rootfs、CRIU 包不入库。
