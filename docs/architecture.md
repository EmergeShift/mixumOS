# mixumOS 总体架构

> 状态:设计定稿(2026-10-04)。
> 本文档回答:mixumOS 是什么、由什么组成、每个组件用什么语言、为什么。

## 一句话定义

mixumOS 是一个基于 Arch Linux 的 **Agent OS 发行版**:它把容器变成 agent 的
运行环境,把 agent 的每一次操作变成可回退、可分叉、可迁移的系统事件,
把 agent 操作的 GUI 以原生窗口的形式流到用户的手机或电脑上。

## 两条铁律

1. **发行版,不是一堆二进制。** 所有组件以 PKGBUILD 打包进自有 pacman 仓库,
   镜像(mkosi 组装的 rootfs / OCI)是发行版的产物而不是发行版本身。
   依赖库出安全更新时升一个 so,而不是重编整个世界。
2. **agent 是一等公民。** 我们的 agent 用 Rust 写,runner 的核心也是 Rust,
   agent 以 crate 形式嵌入 runner,共享类型与生命周期;
   外部第三方 agent 走 external 桥协议(见 agent-bridge.md)。

## 组件总览

```
┌─────────────────────────── 用户侧 ───────────────────────────┐
│  iOS / Android(PWA 控制台)   macOS / 桌面(mixum-ctl + 控制台)  │
└──────────────┬────────────────────────────┬─────────────────┘
               │ WSS(TLS 1.3 + 能力令牌)      │
┌──────────────▼────────────────────────────▼─────────────────┐
│                      mixum-gateway(云端)                      │
│            k8s Ingress / operator(MixumTask CRD)              │
└──────────────┬──────────────────────────────────────────────┘
               │ 调度(arm64 / x86_64 节点池)
┌──────────────▼──────────────────────────────────────────────┐
│                     容器:mixumOS rootfs                       │
│                                                              │
│  mixum-agentd(Rust)         mixcomp(C,wlroots 0.18)          │
│  ├─ 任务引擎(AgentDriver)     ├─ 每窗口隔离捕获(client buffer)  │
│  ├─ 快照引擎(块+树+CRIU)     ├─ virtual-pointer/keyboard 注入  │
│  ├─ 显示 broker(接管状态机)  └─ Unix 控制通道 ↕ 上行帧/下行输入  │
│  └─ WSS 服务端(协议见 protocol.md)                             │
│                                                              │
│  pacman 仓库:pkgs/mixum-agentd  pkgs/mixcomp  pkgs/mixum-ctl   │
└──────────────────────────────────────────────────────────────┘
```

## 语言分工(及理由)

| 组件 | 语言 | 理由 |
|------|------|------|
| mixum-agentd(核心守护进程) | **Rust** | 与 agent 同生态,agent 以 crate 嵌入;tokio 承载 ws/进程管理/流;发行版组件正统(动态链接、走 pacman) |
| mixcomp(Wayland 合成器) | **C** | 必须贴 wlroots C API;wlroots 生态最成熟;魔改点在 C 层收敛为一个可审计的目录 |
| mixum-ctl(主机 CLI) | **Rust** | 与 agentd 同 workspace,共享 protocol crate |
| 控制台(PWA) | **TypeScript** | 浏览器端唯一正解;iOS/Android 直接用 |
| 云端 operator | **Rust** | 与核心同 workspace;k8s 交互用 kube-rs |
| 协议/SDK | **TS + Rust crate** | 客户端两端的形态 |

**reference/(Go)的定位:参考实现。** wire 协议、快照树语义(回退/fork/分叉)、
接管状态机、跨节点同步协议已经在 Go 实现中被单测和容器 e2e 锁住行为。
Rust 重写时它充当可执行规格;两侧实现对跑可做协议一致性回归
(`tests/cross-protocol/`,规划中)。它不进入发行版产物。

## 关键数据流

### 任务流(用户发指令 → agent 跑)

1. 用户在控制台/手机发出指令,选择 runner(本地容器或云端);
2. mixum-agentd 收到 `task.submit`,先打任务前置无感快照(冻结所有任务进程,
   打包工作区,毫秒级),再交给 AgentDriver;
3. AgentDriver(嵌入的 agent crate 或 external 桥)通过 Sandbox 执行命令、
   打开 GUI 应用;
4. GUI 应用的窗口经 mixcomp 隔离捕获,帧流推到用户端,光标位置随帧头映射;
5. 过程事件(task.event)实时推送,用户随时接管(display.takeover);
6. 任务结束:状态与产物落盘,失败可按策略自动回退到前置快照。

### 快照流(跨设备迁移)

1. mac 盒盖前,mixum-ctl 的电力钩子触发 hybrid 快照(disk + CRIU 内存);
2. 快照以内容寻址块存储,只推远端缺失的块(zstd 压缩 + 去重);
3. 云端容器导入节点,挂进自己的快照树(origin 标记来源),任务继续跑;
4. 用户下次打开 mac,反向 pull 即可回到本地。

## 目录结构

```
mixumos/
├── docs/                  设计文档(本目录)
├── rust/                  Rust workspace(核心,重写主战场)
│   └── crates/
│       ├── mixum-protocol 协议层:envelope/ops/能力令牌/帧头
│       ├── mixum-snapshot 快照引擎:块存储/树 DAG/CRIU/同步
│       ├── mixum-agentd   容器内守护进程(任务引擎/显示 broker/WSS)
│       ├── mixum-ctl      主机侧 CLI(电力钩子/快照推送)
│       └── mixum-operator 云端 k8s operator(后置)
├── mixcomp/               C/wlroots 合成器(见 wayland-magic.md)
├── reference/             Go 参考实现(可执行规格,不进发行版)
├── dist/                  发行版构建(见 distro.md)
│   ├── pkgs/              PKGBUILD(mixum-agentd / mixcomp / mixum-ctl)
│   ├── repo/              pacman 仓库工具
│   ├── os/                rootfs 与镜像构建(mkosi)
│   └── mkosi/
├── console/               PWA 控制台(后置)
├── cloud/                 k8s 清单(后置)
├── tests/
│   └── integration/       容器 e2e(reference 版已通过,Rust 版复用)
└── proto/v1/              协议的机器可读形态(JSON Schema,后置)
```

## 非目标(v1 明确不做)

- 多用户/多租户隔离(v1 单用户多设备;租户模型在 gateway 层后置);
- 图形协议的 H.264/AV1 硬编(v1 用 JPEG,WebCodecs 升级路径已留);
- 非容器形态(裸机安装器);Windows/macOS 作为容器宿主(只作为客户端)。

## Rust 实现记录

`rust/crates/mixum-agentd` 已按本文件与 `docs/protocol.md` 落地守护进程骨架：
WSS（TLS 1.3）、能力令牌握手、请求路由、50/s（突发 100）限速、任务引擎、
external 行分隔 JSON 桥、窗口 broker 和快照 trait 桩均使用协议 crate 的类型。

| 行为 | Go 参考实现 | Rust 实现 | 差异 |
|------|-------------|-----------|------|
| envelope、令牌、视频帧头 | `reference/internal/wire`、`auth` | `mixum-protocol` | |
| 任务状态与事件 | `reference/internal/task` | `mixum-agentd::task` | |
| 快照树 | `reference/internal/snap` | `SnapshotEngine` 内存桩 | 真实块存储待 `mixum-snapshot` 接口稳定后接入 |
| 显示控制通道 | `reference/internal/display` | `mixum-agentd::display` | |
| 管理 REST／指标 | 后置能力 | 未实现 | 任务书明确排除 |

Rust 服务已用 Go `reference/cmd/e2eclient` 实跑协议链路并通过 `E2E PASS`；该结果使用内存快照桩，窗口帧部分在无 mixcomp 窗口时按客户端约定跳过。
