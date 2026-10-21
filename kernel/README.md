# 内核：`linux-mixum-micro` 与 `linux-mixum`

两个内核共享同一份配置源，按用途分构建：

| 名称 | 基底 | 用途 | `uname -r` |
|------|------|------|------------|
| `linux-mixum-micro` | kata 6.18.35 | Apple Container / WSL2 / QEMU microVM 的 guest | `6.18.35-mixum-micro` |
| `linux-mixum` | Arch 内核 config | 完整磁盘镜像 / 通用 Linux VM / 裸机 | `6.18.x-mixum` |

> 能力边界见 `MixumART/docs/support-matrix.md`。内存快照要求节点内核指纹完全一致。

## 目录

```
kernel/
├── configs/
│   ├── common.fragment     # 两端共有：CRIU/Landlock/FUSE/BPF/KSM/ZRAM/namespaces
│   ├── micro.fragment      # guest 快路径：virtio / Hyper-V / 9p / vsock
│   └── full.fragment       # 完整镜像：LOCALVERSION 与 QEMU virtio
├── scripts/
│   ├── build.sh            # Docker 内构建（下载源码 → merge config → make）
│   ├── install-apple-container.sh
│   └── install-wsl2.ps1
├── patches/                # 少量、可上游的补丁；默认空
└── out/                    # 构建产物（gitignore）
```

## 构建

前置：Docker（Apple Silicon 上原生构建 arm64；x86_64 需要交叉工具链）。

```sh
# guest 快路径（默认 arm64，defconfig 基底）
./kernel/scripts/build.sh --variant micro --arch arm64

# 完整镜像（Arch config 基底）
./kernel/scripts/build.sh --variant full --arch x86_64 --base arch

# 用已有 config 作为基底（推荐：从目标 runtime 导出）
./kernel/scripts/build.sh --variant micro --arch arm64 --base file:/path/to/kata.config
```

产物在 `kernel/out/linux-<variant>-<arch>/`：`Image`（arm64）或 `bzImage`（x86_64）、
`vmlinux`、`.config`、`System.map`。

> 发布用的 `linux-mixum-micro` 应以 kata 的实际 config 为基底（从 Apple Container
> 导出的 `/proc/config.gz`，或 kata-containers 对应 tag 的 fragments）。`defconfig`
> 只用于本地快速验证，不用于发版。

## 安装

### Apple Container

```sh
./kernel/scripts/install-apple-container.sh kernel/out/linux-mixum-micro-arm64/Image
```

脚本会调用 `container system kernel set --binary`；这是**系统级**设置，影响本机所有
Apple Container。请先阅读脚本里的确认提示。

### WSL 2

在 Windows PowerShell 中：

```powershell
.\kernel\scripts\install-wsl2.ps1 -Kernel C:\path\to\linux-mixum-micro
```

脚本会备份 `%USERPROFILE%\.wslconfig`，写入 `[wsl2] kernel=...`，然后 `wsl --shutdown`。

## 验收

```sh
uname -r                       # 6.18.35-mixum-micro / 6.18.x-mixum
criu check                     # 需要 criu + 特权
cat /sys/kernel/security/lsm   # 含 landlock
zcat /proc/config.gz | grep -E "CHECKPOINT_RESTORE|LANDLOCK|FUSE_FS|USERFAULTFD"
```

最少跑通：

1. `criu dump/restore` 一个普通进程；
2. Landlock 拒绝工作区外写入（Task 4 的测试）；
3. FUSE 挂载一个只读目录；
4. Apple Container / WSL2 正常启动并跑 `mixumd`。

## 配置策略

- **common.fragment** 只放“两个内核都需要”的能力；平台驱动放各自 fragment。
- 补丁控制在 0–3 个，必须有“为什么不能等上游”的理由，并尽量上游化。
- `CONFIG_LOCALVERSION` 是快照指纹的一部分；升级内核会让旧 memory/hybrid 节点失效
  （disk 节点已放开跨内核恢复）。
- 不追求的：把 micro 做成通用发行版内核。它只服务 guest 快路径；完整镜像用
  `linux-mixum`。

## 许可与冠名

- Linux 内核是 **GPL-2.0**：分发内核二进制必须提供对应源码（或书面要约），保留版权声明；
  你们的修改可以加自己的版权，但不能改掉别人的。
- kata 的构建脚本/配置是 **Apache-2.0**：保留 NOTICE，描述性使用 “based on kata”。
- `linux-mixum` / `linux-mixum-micro` 是**构建与配置的品牌**；不要宣称内核由 mixum 从零编写，
  也不要用 “Linux” 暗示官方认证。
- 发布时同时归档：源码 tarball、`.config`、补丁、构建脚本、校验和。
