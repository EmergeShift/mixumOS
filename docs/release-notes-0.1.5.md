# mixumOS 0.1.5 — aarch64

状态：pre-release。发布日期：2026-10-22。构建 commit：`7fbbe0e`
（发布 tag 另含本说明提交）。

## 亮点

- **macOS / Apple Container 内核打通**：`linux-mixum-micro`（kata 6.18.35 基底）
  在 Apple Container 上启动、CRIU + Landlock/diag 全开；README 增加“切回原内核”
  与能力损失说明。
- **CRIU + Wayland PoC**：Sway + foot 整个会话树 dump/restore 成功；引擎支持
  多根 dump（任务 + mixcomp），修复 Wayland 半连接与同机回退进程树清理。
- **`.mart` 迁移闭环**：mixumOS `snap.export/import` + MixumART 导出/导入/新环境
  迁移 + `migration.json`（镜像 digest、容器规格、guiSessions）；两个真实
  agentd 之间 336B `.mart` 迁移并 restore 文件验证通过。
- **GUI 会话清单**：快照记录 `guiSessions`，restore 后按清单重放窗口。
- **音频 / 输入法 / 拖拽上传**：`audio.attach/detach` + s16le 帧、text-input-v3
  宿主输入法文本、画布拖入 `file.put`；镜像入口在无 systemd 环境自动拉起
  PulseAudio null sink。
- **Landlock + cgroup v2 freezer**：沙箱写白名单与冻结优先级落地。
- **剪贴板 A 路线设计**：事件 ACL、能力握手、空串语义与 op 命名的评审结论。

## 新增 / 修复

### 内核

- `linux-mixum-micro`：48-bit VA/PA、`CHECKPOINT_RESTORE`、`SECURITY_LANDLOCK`、
  `INET/UNIX/PACKET/NETLINK_DIAG`、FUSE/vsock/9p；Apple Container 实测
  `uname -r = 6.18.35-mixum-micro`。
- `linux-mixum` full：62 MB `Image` + modules，QEMU virt + 最小 initramfs
  实测 `FULL_KERNEL_BOOT_OK`。
- 构建脚本：容器内大小写敏感 `/build`、参数透传、kata config 基底、
  host 解压规避 Docker symlink EPERM。

### 快照 / CRIU

- `Engine` 支持 `restore_prepare`、`set_dump_roots`、多 `--tree`；
- agentd 扫描 `/proc/*/comm` 把 mixcomp 一并纳入 dump 根；
- CRIU 参数带 `--ext-unix-sk --file-locks --ghost-limit`；
- 修复并行快照共用 `/tmp/mixum-criu-` 的偶发 NotFound。

### 迁移

- `mixum-mart` vendored `map-snapshot`（Apache-2.0），CI 不再依赖私有仓库；
- `migration.json` 记录镜像 tag + `@sha256` digest、env、ports、mounts、command、
  来源节点/分支、`guiSessions`；
- MixumART「新环境迁移」：按源规格重映射端口、创建/启动容器、读 admin-token、
  写连接档案、导入 `.mart`；
- MapClient 改发 `hello` 并兼容 `hello.ok` / `session.hello.ok`，修复旧镜像
  `unauthorized: 请先 hello`。

### 音频 / 输入 / 文件

- `audio.attach/detach`、`0x02` s16le 帧（48 kHz 双声道 20 ms）；
- AudioHub 用进程组回收 `sh+parec`，start/stop 代数隔离旧 reader；
- mixcomp `text_input.c` 校验同一 Wayland client，避免非法 enter；
- 画布拖入文件经 `file.put` 分块上传，≤64 MiB 计算 sha256；
- 镜像入口自动启动 PulseAudio null sink（`/etc/mixum/pulse.pa`）。

## 验证

- 镜像内包版本：

```text
mixum-base 0.1.5-1
mixum-agentd 0.1.5-1
mixcomp 0.1.5-1
mixum-ctl 0.1.5-1
```

- Apple Container 启动：`uname -r = 6.18.35-mixum-micro`。
- 迁移 e2e（0.1.5 镜像，两个 agentd）：

```text
mart 336 bytes → target import → snap.restore
目标文件内容：data
```

- 音频：`pactl` 看到 `mixum` sink；`parec --device=mixum.monitor` 2 秒采集
  65,668 字节 s16le。
- CRIU + Wayland PoC：Sway+foot dump→kill→restore，`swaymsg -t get_tree`
  仍有 `app_id: foot`。
- QEMU full 内核：串口 `FULL_KERNEL_BOOT_OK`、`uname -a` 为 6.18.35-mixum。
- Rust：`mixum-agentd` 49、`mixum-protocol` 23、`mixum-ctl` 14、`mixum-snapshot` 19；
  clippy `-D warnings`；console `npm run build`；mixcomp wlroots 0.18 编译通过。
- CI（main）：`rust` / `rust-windows` / `console` / `mixcomp` 全绿。

## 已知限制

- **CRIU 二进制未随镜像打包**：内核已开 `CHECKPOINT_RESTORE`，但 `criu` 需在容器内
  自行安装；Arch 的 CRIU 4.2.1 在本内核上 parasite SIGSEGV，PoC 使用 Debian
  CRIU 3.17.1。memory/hybrid 快照暂不是开箱即用。
- `linux-mixum` full 不能直接在 Apple Container 启动（需要自己的 initramfs/rootfs）；
  QEMU/裸机可用。
- 本次仅产出 aarch64；x86_64 未构建。full arm64 使用 defconfig + 片段，
  x86_64 可用 `--base arch` 拉 Arch 官方 config。
- 跨设备自动建容器仍需目标端 MixumART 执行；当前支持“已连接目标环境”直传。
- 音频 D-Bus 警告来自容器内无 system bus，不影响 null sink 采集。
- 剪贴板 A 路线仍是设计稿；B 的只读 `display.clipboard` 不变。

## 升级提示

- macOS 升级内核后，新建容器自动使用 `linux-mixum-micro`；已运行容器需重启才切换。
  切回原内核与能力损失见 README「macOS / Apple Container 内核」。
- 旧镜像只有 `hello` op；新 host 客户端已兼容，无需改容器。
- 需要内存快照时，先确认容器内 `criu --version`，并按内核指纹/特权要求使用。

## 发布资产

| 文件 | SHA256 |
|---|---|
| `mixumos-0.1.5-aarch64.oci.tar` | `acf7c5512c9c22eb312ceecf67bca02fbfd7bb5cf83fd01f4afa28aaf43909af` |
| `mixumos-0.1.5-aarch64-rootfs.tar.zst` | `e849a52aa70ddce251f7ef8233b3adde3e8b65201d5072885f0265a7c39cf7cb` |
| `mixumos-0.1.5-aarch64-repo.tar.zst` | `16b0d2ed9518f56da8c1f22da43b5b0dc8b91c9c129152adf05fe5a0bfe7d567` |
| `BUILDINFO-0.1.5` | `12723a81f747b82caf3c8d745f0fe7272f3b6bf5d956000449926d5f66bccecb` |
| `linux-mixum-micro-6.18.35-arm64.Image` | `859426725efd63099ce4106a84ad221f32b47ea0d9a2b738c2cc1f4635f9ca3b` |
| `linux-mixum-6.18.35-arm64.Image` | `c95413d257db24b6c75d8fd0009cedafa4d669c21ccb929dc73a32819d35ed10` |

registry digest：以本地 registry 导入后的 `mixumos:arm64` 为准（OCI tar 的
manifest digest 构建日志：`85699307...`）。
