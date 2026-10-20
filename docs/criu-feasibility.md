# CRIU 跨内核迁移可行性验证

状态:**进行中(2026-10-20 更新)—— 同内核基线 PASS;2026-10-19 的跨内核结论
撤回为「证据不足,方法学无效」;受控实验协议见 §5,待按协议重跑。**
关联:roadmap 阶段 5 最后一项;`docs/snapshots.md` §6;`mixum-snapshot/src/criu.rs`。

## 1. 结论摘要

- **同内核 dump → restore 实测可用(有效结论)**:WSL2 6.18 上 dump 一个持续
  写计数器的进程,进程被杀死后从镜像恢复并继续计数(§3)。
- **跨内核:暂无结论。** 2026-10-19 的实验同时混入了两个混杂变量(§4):
  ① 源/目标 CRIU 版本不同(4.2.1 vs 3.16.1);② 目标环境自身未通过
  control(本地 dump 失败)。因此「跨内核不可行」**不成立也不被否定**——
  这是一个开放问题,需按 §5 的受控实验协议重跑。
- **工程策略不依赖上述结论**:精确指纹门禁(kernel/arch 全等)是保守策略,
  与可行性结论无关,继续保留;跨设备需求走 disk 快照 + 重启,远期走
  VM 级迁移(§7)。

## 2. 实验环境

| 环境 | 内核 | CRIU | 用途 | 有效性 |
|---|---|---|---|---|
| WSL2(Arch) | 6.18.54.1-microsoft-standard-WSL2 | 4.2.1 | dump 源 + 同内核基线 | ✅ |
| Docker Desktop / kind 节点 | 同上(共享 WSL2 内核) | — | 说明容器与宿主同内核 | ✅ |
| QEMU/KVM 第二内核 VM | 6.1.0-50-amd64(Debian) | 3.16.1 + 匹配 modules | 跨内核恢复目标 | ⚠️ **环境未验证**(见 §4.2) |

第二内核用 Debian netboot `vmlinuz` + 以 Ubuntu 22.04 rootfs(含 CRIU 3.16)
构建的 initramfs 启动;CRIU 依赖的 `crc32c_generic`/`libcrc32c`/`nf_tables`/
`ip_tables` 模块来自与内核严格匹配的 `linux-image-6.1.0-50` 包。

## 3. 实验一:同内核基线(PASS,有效)

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

## 4. 实验二:跨内核恢复(结果不可归因,方法学无效)

拓扑:WSL 6.18 上 dump 静态计数器 → 把镜像放进第二内核 VM 的 initramfs →
在 6.1 VM 内 `criu restore`。该实验**不能用于判断内核兼容性**,原因:

### 4.1 混杂变量 A:CRIU 版本混用

源端 CRIU 4.2.1,目标端 3.16.1。CRIU 的镜像格式不保证跨大版本交换兼容,
4.2.1 的镜像交给 3.16.1 restore 本身就可能失败。失败不可归因于内核差异。

### 4.2 混杂变量 B:目标环境未经 control 验证

目标 VM 自身**本地 dump 也未产出可用镜像**(`Unexpected EOF on (empty-image)`,
原报告已记录)。没有「目标环境同内核 dump/restore PASS」这个 control,
该环境上的任何失败都更可能来自 initramfs/CRIU 安装/运行时假设,而非内核不兼容。

### 4.3 观察到的失败模式(待受控实验区分归因)

1. `kerndat_has_nftables_concat` 阶段失败(缺 netfilter 模块时)——
   补 `crc32c_generic`/`libcrc32c`/`nf_tables`/`ip_tables` 后特性探测通过;
   属**环境/模块集**问题,与内核版本无直接关系。
2. `tty: No task found with sid 277` —— 镜像携带源环境 tty/session 引用;
   可能由版本差异、dump 侧 session 处理、或目标环境缺同 sid 任务共同导致,
   当前无法区分。
3. 目标 VM 本地 dump EOF —— 环境问题(见 4.2)。

**结论:撤回原报告「跨内核透明迁移不可行」。**

### 4.4 受控重跑(2026-10-20,按 §5 协议)

按 §5 协议完整重跑:源端 WSL 6.18 + CRIU 4.2.1 dump(①);目标 VM 为
Arch rootfs + **CRIU 4.2.1(与源端完全一致)** + Debian 6.1.0-47-amd64
内核(与源端不同的唯一内核变量),先跑目标端本地 control(②),PASS 后
再执行跨内核 restore(③),两轮重复:

- **② target-control:PASS**(两轮)——目标环境本地 dump → kill → restore
  → 计数继续,环境有效性成立;
- **③ 跨内核 restore:FAIL**(两轮稳定)。失败点随环境混杂逐个清除而
  移动,每一层都属**环境类**,不是内核类:
  1. initramfs `/dev` 未挂 devtmpfs → init stdio 丢失(init 顺序修复);
  2. Debian 内核包不带 `modules.dep` → 特性探测失败(构建侧 depmod 修复);
  3. 被测进程 stdio 重定向文件在目标端不存在 → dump 侧改为 `/dev/null`,
     进程不携带外部文件 fd;
  4. 计数器文件大小校验(CRIU 要求与源端字节数一致)→ restore 脚本用
     源端 `counter_after_dump` 值预建同尺寸文件;
  5. 进程 root 目录 mode 不一致(源 0555 / 目标 0755)→ 目标 root 对齐;
  6. 进程 cwd 指向源机专用路径(WSL `/mnt/...`)→ dump 侧固定 cwd 为 `/`。
  最终稳定失败于 cwd/挂载命名空间引用源机路径(`Can't open cwd`):
  **根因是两端 rootfs/挂载命名空间不同(WSL ext4 vs initramfs)**,属
  环境类;本次拓扑无法把变量收敛到「内核」一个。

按 §5 判定:**证据不足**——不能据此得出「跨内核不可行」,也不得出
「可行」。roadmap 保持未勾选。

**下一轮严谨设计**(要真正单变量):两端使用**同一 rootfs 镜像**、仅内核
不同——例如同一 initramfs 分别以两个不同版本的 vmlinuz 启动两个 VM,
源端 dump 后把镜像放入同一 rootfs 的第二个 VM 恢复。

## 5. 受控实验协议(重跑标准)

有效实验只允许「内核」一个变量。协议由脚本强制(见 §8):

1. **同版本 CRIU**(两端 `criu --version` 完全一致;脚本断言,不一致即中止);
2. **目标环境 control 先行**:在目标 VM 上先跑
   `target-control.sh`(同内核本地 dump → kill → restore → 计数继续),
   失败则目标环境判定无效,实验终止,不得进入跨内核步骤;
3. **同 arch**;记录两端 kernel release、CRIU 版本、内核关键 config
   (CHECKPOINT_RESTORE / NETFILTER / NF_TABLES / SYSVIPC / CGROUP_*)与
   模块列表;
4. **只恢复、不改环境**:跨内核步骤除内核外,rootfs/CRIU/modules 与 control
   轮保持一致;
5. **失败分类**:每条失败先归入〔环境/版本/内核〕三类,只有排除前两类后才
   可归因于内核;
6. **判定**:
   - 目标 control PASS + 跨内核实测 PASS(计数继续) → 跨内核可行;
   - 目标 control PASS + 跨内核稳定失败且分类排除环境/版本 → 跨内核不可行;
   - 其余 → 证据不足,不得写入路线图结论。

## 6. 与现有实现的关系

- `mixum-snapshot/src/engine.rs`:dump 时记录 `kernel`/`arch`,restore 时
  精确比较并返回 `KernelMismatch`/`ArchMismatch`。该闸门是**保守策略**,
  与 §5 的可行性结论解耦,无论重跑结果如何都保留。
- `mixum-snapshot/src/criu.rs`:`--shell-job` 的用法正确;同内核路径可用。
- `docs/snapshots.md` §6 的表述已同步为「跨内核未验证 + 精确指纹门禁」。

## 7. 工程对策(不依赖可行性结论)

1. **保留精确指纹**:kernel release + arch 必须完全相同才允许 memory restore;
   错误信息明确提示「需要同内核节点」,不做静默降级。
2. **同内核节点池**:节点打 `mixumos.io/kernel` 标签(如 operator 的
   `arch` nodeSelector 同款机制),memory 快照的 restore 只调度到同内核节点;
   disk 快照不受此限。
3. **降级路径**:目标内核不同 → memory 快照不可用,改用 disk 快照 +
   任务重启(记录 RTO);用户可在控制台看到降级原因。
4. **同内核热迁移**:笔记本 → 云端使用同一发行版内核镜像(`dist` 固定
   内核版本)时,CRIU 迁移可行;这是近期可落地的「合盖上云」形态。
5. **长期方案**:需要真正跨内核时,走 VM 级 live migration
   (Firecracker/Cloud Hypervisor)或在快照中携带可引导内核,
   而不是放宽 CRIU 指纹。
6. **测试策略**:同内核 dump/restore 可进 privileged nightly e2e;
   跨内核只保留实验脚本,不作为 CI 门槛。

## 8. 复现(按受控协议)

```bash
# ① 源内核:dump 静态计数器,并写入 meta.txt(CRIU 版本/内核/arch)
sudo tests/integration/criu/dump-for-cross-kernel.sh /tmp/criu-imgs-cross

# ② 目标 VM:control 必须先 PASS(同内核本地 dump/restore)
sudo tests/integration/criu/target-control.sh /tmp/criu-imgs-cross

# ③ 目标 VM:断言版本一致 + control-ok 存在后,执行跨内核 restore
sudo tests/integration/criu/restore-for-cross-kernel.sh /tmp/criu-imgs-cross
```

第二内核 VM 的组装(initramfs、模块、loopback、QEMU 启动)见
`tests/integration/criu/README.md`。脚本只包含可移植的测试逻辑;
内核、rootfs、CRIU 包不入库。
