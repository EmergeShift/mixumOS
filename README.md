# mixumOS

**Agent OS 发行版:给 AI agent 一个可回退、可分叉、可迁移的操作系统。**

基于 Arch Linux:容器即 agent 运行环境,操作可回退、可分叉、可在设备间
迁移;agent 操作的 GUI 以独立原生窗口流到手机或电脑,不抢宿主的鼠标键盘。

- 设计文档入口:[docs/architecture.md](docs/architecture.md)
- 路线图:[docs/roadmap.md](docs/roadmap.md)(ddl 以月计,文档先行)
- 协议规范:[docs/protocol.md](docs/protocol.md)

## 核心能力

**快照即时间线(类 Git)**
- 内容寻址块存储(CAS + zstd + 可选 AES-256-GCM),快照树是 DAG:
  原地回退自然分叉、显式 fork、回到任一未来快照,节点永不丢失;
- **无感前置快照**:任务启动前自动打点,进程冻结窗口内打包,
  未变更工作区热快照实测 **8.8 ms**(300 文件),大文件吞吐约 640 MB/s;
  容器内 mixumd 启动到可连接中位 **102–107 ms**,端到端 `docker run` 中位
  **226 ms**(2026-10-21 基准,见 [snapshots.md §9](docs/snapshots.md);
  复现:`cargo run --release -p mixum-snapshot --example bench`、
  `node tests/integration/startup-bench.mjs`);
- 跨节点流转幂等去重:重推已同步快照零字节传输;
  盒盖→云端全链(快照→TLS 推送→远端 restore 物化→head 迁移续跑)
  由双节点 e2e 锁定([tests/integration/sync-e2e.mjs](tests/integration/sync-e2e.mjs))。

**容器内 GUI 以原生窗口流到宿主桌面**
- mixcomp(C/wlroots 0.18)按窗口隔离捕获,HEADLESS 多后端合成器;
- JPEG 帧流 30fps 实测稳定(30 分钟浸泡 54,361 帧零异常零断连),
  光标来自帧头,鼠标键盘经能力令牌鉴权后注入接管;
- 不抢宿主输入:接管是显式动作,agent 模式下注入硬拒绝。

**发行版形态**
- `make dist` 一条命令产出双架构(x86_64 原生 + aarch64)pacman 仓库
  与 OCI 镜像,单组件 `pacman -S mixum-agentd` 独立升级;
- 动态链接优先:Rust 组件动态链接 glibc,mixcomp 链系统 wlroots 0.18
  (依赖库安全更新升一个 so 即可)。

## macOS / Apple Container 内核

Apple Container 的内核是**宿主级全局设置**，不支持按容器指定内核。mixumOS 在 macOS 上
使用 `linux-mixum-micro`（基于 kata 6.18.35，开启 CRIU/Landlock/diag）。把它设为默认后，
**新建容器会自动使用这个内核**；已经运行的容器保持启动时的内核，重启后才切换。因此
“自动切换”只作用于之后启动的容器，不存在给单个容器换内核的开关。

安装/升级内核（会提示确认，影响本机所有 Apple Container）：

```bash
container system kernel set --binary /path/to/Image   # 或 ./kernel/scripts/install-apple-container.sh <Image>
container run --rm alpine:3.20 uname -r               # 期望 6.18.35-mixum-micro
```

### 切回原内核

`container system kernel set` 拒绝目标目录同名文件覆盖，先备份并复制成唯一文件名：

```bash
K="$HOME/Library/Application Support/com.apple.container/kernels"
# 安装 mixumOS 内核前先备份当前默认内核（如果还没备份，原 kata 内核通常是
# vmlinux-6.18.35-197-debug）
cp "$K/vmlinux-6.18.35-197-debug" /tmp/vmlinux-original
container system kernel set --binary /tmp/vmlinux-original
container system status
container run --rm alpine:3.20 uname -r
```

> 切换后已运行容器不会自动换内核；需要旧内核的容器要 stop/start 一次。
> Apple Container 服务若在切换后卡住，先清理卡住的 VM 再重启 `container system`。

### 切回原内核会放弃什么

| 能力 | 原 kata 内核（无 CRIU） | `linux-mixum-micro` |
|---|---|---|
| 磁盘快照 / 快照树 / fork / 回退 | ✅ | ✅ |
| `.mart` 跨设备迁移（文件层） | ✅ | ✅ |
| mixcomp GUI 流 / 窗口重放 | ✅ | ✅ |
| **memory / hybrid 快照（CRIU dump）** | ❌ | ✅ |
| **同机进程级回退（CRIU restore）** | ❌ | ✅ |
| CRIU 的 UNIX/INET socket 收集（diag） | ❌ | ✅ |
| Landlock / FUSE / cgroup freezer | 以原内核配置为准（kata 3.32 仍有 Landlock/FUSE） | ✅ |
| 自定义内核补丁与 `-mixum-micro` 指纹 | ❌ | ✅ |

**不依赖内核的能力不会丢**：文件快照、`.mart` 迁移、GUI 画面流、窗口重放仍可用；
丢的是“进程内存/打开的文件与 socket”这一类 CRIU 能力。需要这些能力时再切回
`linux-mixum-micro` 并重启容器即可。

## 架构总览

```
 console(PWA)──┐                          手机/浏览器
               │ WSS(TLS 1.3 + ed25519 能力令牌)
               ▼
        mixum-agentd ──── 任务引擎(shell/external 驱动,进程组冻结)
               │  │  ──── 快照引擎(CAS + 快照树 DAG + CRIU)
               │  │  ──── 显示 broker(窗口注册/帧分发/接管状态机)
               │  └──── snap.push → 远端 agentd(跨节点块同步,幂等)
               ▼ AF_UNIX
            mixcomp ──── wlroots 0.18:每窗口捕获 + 输入注入
               ▼
          Arch 容器(agent 工作区;镜像由 dist/ 从 pacman 仓库组装)
```

## 快速体验

```sh
# 1. 单元测试(协议/快照引擎,含交叉回归向量;Linux 或容器内)
cd rust && cargo test -p mixum-protocol -p mixum-snapshot

# 2. 容器 e2e(mixum-agentd + mixcomp + foot 窗口流 + 任务 + 快照)
#    需先构建 Linux 二进制放 build/mixum-agentd-arm64(方法见
#    tests/integration/Dockerfile.e2e-rust 头注)
docker build -f tests/integration/Dockerfile.e2e-rust -t mixumos-e2e-rust .
docker run -d --name mixum-e2e -p 17601:7601 mixumos-e2e-rust
# 令牌在容器日志 READY 行;控制台开发对接见 console/README.md

# 3. 发行版双架构全链(pacman 仓库 + OCI 镜像;需 docker,耗时较长)
make dist ARCHS="aarch64 x86_64"
# 产物:dist/out/repo-{aarch64,x86_64}、mixumos-<ver>-<arch>.oci.tar、
#       mixumos-<ver>-multi.oci.tar(双架构索引)
# 验收:docker import --platform <arch> dist/out/rootfs-<arch>.tar mixumos:dev
#       docker run -p 17601:7601 mixumos:dev /entrypoint.sh   # 看 READY 行
```

## 文档地图

| 文档 | 内容 |
|------|------|
| [architecture.md](docs/architecture.md) | 组件、目录结构、数据流 |
| [protocol.md](docs/protocol.md) | 线协议 v1:envelope、ops、能力令牌、二进制帧 |
| [snapshots.md](docs/snapshots.md) | 快照引擎设计 + 基准数字(§9) |
| [tests/poc/fuse-lazy](tests/poc/fuse-lazy) | FUSE 惰性挂载 PoC 脚本与判定标准 |
| [tests/poc/qemu-full](tests/poc/qemu-full) | QEMU virt 启动 linux-mixum full |
| [poc-criu-wayland.md](docs/poc-criu-wayland.md) | CRIU + Wayland 会话快照 PoC 与结论 |
| [console.md](docs/console.md) | Web 控制台(PWA)架构与对接 |
| [wayland-magic.md](docs/wayland-magic.md) | mixcomp 合成器与窗口流实现 |
| [backends.md](docs/backends.md) | 宿主后端形态:Docker/WSL2/nvx/k8s |
| [distro.md](docs/distro.md) | 发行版构建工具链与踩坑记录 |
| [security.md](docs/security.md) | 威胁模型、令牌、传输、快照加密 |
| [agent-bridge.md](docs/agent-bridge.md) | agent 进程接入协议(external 驱动) |
| [migration.md](docs/migration.md) | 跨设备 `.mart` 迁移包与流程 |
| [gateway.md](docs/gateway.md) | 云端枢纽设计:多容器路由、设备配对、令牌分发 |
| [criu-feasibility.md](docs/criu-feasibility.md) | CRIU 跨内核迁移可行性验证与工程对策 |
| [roadmap.md](docs/roadmap.md) | 路线图与阶段完成标准 |

## 当前状态(2026-10-17)

| 部分 | 状态 |
|------|------|
| rust/(核心实现) | mixum-protocol/mixum-snapshot/mixum-agentd/mixum-ctl/operator 全链落地,Rust e2e PASS |
| mixcomp(C/wlroots 0.18) | 每窗口捕获 + 协议注入;30fps 稳定流,30 分钟浸泡零异常 |
| 跨节点同步 | 推送/导入/幂等/云端恢复续跑,双节点 e2e 全链 PASS |
| dist/(发行版) | 双架构(x86_64 原生 + aarch64 qemu)五包/仓库/OCI 全链重打实测 PASS,multi 索引合并 |
| Go 参考实现 | 已移除:历史行为由 tests/cross-protocol 回归向量锁定 |
| docs/ | 架构/协议/快照/合成器/发行版/安全/agent 桥 已定稿 |

## 许可证

本仓库以 [Apache-2.0](LICENSE) 发布。mixcomp 的"魔改 Wayland 合成器"
指基于 wlroots 自研,仓库不包含任何上游组件的修改版源码:

- 仓库内源码(rust/mixcomp/console/dist 脚本)为原创;
  构建期拉取的上游 wlroots 0.18.2 tarball 未打补丁(MIT,见其 PKGBUILD);
- 运行时动态链接的第三方库按各自许可:wayland/wayland-protocols(MIT)、
  libxkbcommon(MIT)、json-c(MIT)、libjpeg-turbo(IJG/BSD-3)、zstd(BSD);
- mixumOS 镜像内含 Arch/ALARM 基础系统(glibc/systemd/coreutils 等
  GPL/LGPL 组件),其源码合规由上游发行仓库满足。
