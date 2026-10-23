# mixumOS 总体架构

> 状态:设计定稿(2026-10-04)。
> 本文档回答:mixumOS 是什么、由什么组成、每个组件用什么语言、为什么。

## 一句话定义

mixumOS 是一个基于 Arch Linux 的 **Agent OS 发行版**:它把容器变成 agent 的
运行环境,把 agent 的每一次操作变成可回退、可分叉、可迁移的系统事件,
把 agent 操作的 GUI 以原生窗口的形式流到用户的手机或电脑上。

## 两条铁律

1. **发行版,不是一堆二进制。** 所有组件以 PKGBUILD 打包进自有 pacman 仓库,
   镜像(组装的 rootfs / OCI)是发行版的产物而不是发行版本身。
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

**Go 参考实现的定位(已于 2026-10-16 移除)。** wire 协议、快照树语义
(回退/fork/分叉)、接管状态机、跨节点同步协议最初由 Go 参考实现以单测
和容器 e2e 锁定行为,并充当 Rust 重写的可执行规格;Rust 实现落地后参考
实现完成使命移除,其锁定的行为由 Rust 单测与 `tests/cross-protocol/`
回归向量(令牌/快照,预生成)承接。

## 关键数据流

### 任务流(用户发指令 → agent 跑)

1. 用户在控制台/手机发出指令,选择 runner(本地容器或云端);
2. mixum-agentd 收到 `task.submit`,先打任务前置无感快照(冻结所有任务进程,
   打包工作区,毫秒级),再交给 AgentDriver;
3. AgentDriver(嵌入的 agent crate 或 external 桥)通过 Sandbox 执行命令、
   打开 GUI 应用;
4. GUI 应用的窗口经 mixcomp 隔离捕获,帧流推到用户端,光标位置随帧头映射;
   display broker 支持多个 comp 连接(mixcomp 与 B compositor 可并存),窗口
   记录 owner,`stream.start/stop`、`display.input` 和视频帧都按 owner 路由;
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
├── dist/                  发行版构建(见 distro.md)
│   ├── pkgs/              PKGBUILD(mixum-agentd / mixcomp / mixum-ctl
│   │                      / mixum-base / wlroots0.18 自建)
│   ├── repo/              pacman 仓库工具
│   └── os/                rootfs 与镜像构建(pacstrap + OCI 打包脚本;
│                          mkosi 以等价脚本替代,可启动磁盘镜像后置)
├── console/               PWA 控制台(后置)
├── cloud/                 k8s 清单(后置)
├── tests/
│   ├── integration/       容器 e2e(Rust 版)
│   └── cross-protocol/    交叉回归向量(令牌/快照,JSON)
└── proto/v1/              协议的机器可读形态(JSON Schema,后置)
```

## 非目标(v1 明确不做)

- 多用户/多租户隔离(v1 单用户多设备;租户模型在 gateway 层后置);
- 图形协议的 H.264/AV1 硬编(v1 用 JPEG,WebCodecs 升级路径已留);
- 非容器形态(裸机安装器);Windows/macOS 作为容器宿主(只作为客户端)。

## Rust 实现记录

`rust/crates/mixum-agentd` 已按本文件与 `docs/protocol.md` 落地守护进程：
WSS（TLS 1.3）、能力令牌握手、请求路由、50/s（突发 100）限速、任务引擎、
external 行分隔 JSON 桥、窗口 broker,均使用协议 crate 的类型。
快照后端已接入 `rust/crates/mixum-snapshot`（目录 CAS + 树,经
`real-snapshot` 特性装配,缺省内存桩保证分支可独立构建）；跨节点同步
（协议 §6）已实现:push.import 对账、0x03 块流哈希校验落 CAS、commit
落树与远端根标记,推送端为 wss 客户端（自签证书跳过校验,同
原 Go client.go 行为）。

Go 参考实现已移除(2026-10-16),下表保留为行为溯源对照:

| 行为 | 原 Go 参考实现 | Rust 实现 | 差异 |
|------|-------------|-----------|------|
| envelope、令牌、视频帧头 | `reference/internal/wire`、`auth` | `mixum-protocol` | |
| 任务状态与事件 | `reference/internal/task` | `mixum-agentd::task` | |
| 快照树 | `reference/internal/snap` | `mixum-snapshot` 适配（`RealSnapshotEngine`） | |
| 跨节点同步 | `handlers_snap.go`、`client.go` | `mixum-agentd::sync` | `snap.pull` 同原参考实现不设处理器（拉取=反向调用对方 push） |
| 显示控制通道 | `reference/internal/display` | `mixum-agentd::display` | |
| 管理 REST／指标 | 后置能力 | 未实现 | 任务书明确排除 |

装配语义:`--config` 支持空格与等号两种形式,
空配置文件回退默认值;快照引擎 dataDir=`dataDir/snapshots`、
root=`workRoot`、skip=`.mixum`;bootstrap 管理令牌 10 年;
快照冻结回调冻结全部任务进程组。

验证记录（2026-10-05）:

- e2e:`tests/integration/Dockerfile.e2e-rust`（mixum-agentd + mixcomp +
  foot,musl arm64）,容器内 Go e2eclient 全链路通过——握手/系统信息
  （backend=directory）/窗口流 ≥3 帧/任务/快照树/回退/fork。原版
  e2eclient 在有窗口时 task.submit 超时为其自身并发读缺陷（已在
  wayland-magic.md §8.7 记录,Go 服务端同样复现）,修复版客户端
  （单读循环）对 Rust 与 Go 服务端均 `E2E PASS`;
- 真实文件回退:容器内落文件 → snap.create（chunks=1,size=19）→
  回退 genesis → 文件消失;
- 跨实现推送:Rust 推送端把 Rust 引擎创建的节点推入 Go 参考容器,
  节点出现在 Go 树上（id/分支保持,根节点无远端根标记）;双节点
  回环见 `rust/crates/mixum-agentd/tests/sync_loopback.rs`;
- 单元测试 42 项（含真实引擎）＋协议 crate 18 项全绿;clippy
  `-D warnings` 双配置干净。
