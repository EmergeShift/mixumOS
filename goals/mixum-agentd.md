# 目标:mixum-agentd crate(Rust 容器内守护进程)

> 你负责 mixumOS 核心守护进程的 Rust 实现(roadmap 阶段 2)。
> 这是一份自包含任务书:先读本文档,再动手。

## 必读(动手前按序读完)

1. `/Users/ashreo/agent-project/mixumos/docs/protocol.md` —— 线上协议规范(你的对外契约,逐字对齐)
2. `/Users/ashreo/agent-project/mixumos/docs/architecture.md` —— 组件总览与语言分工
3. `/Users/ashreo/agent-project/mixumos/docs/wayland-magic.md` §5 —— mixcomp 控制通道协议
4. `/Users/ashreo/agent-project/mixumos/docs/agent-bridge.md` —— AgentDriver 与 external 桥
5. Go 参考实现(只读,行为规格):`reference/internal/server/`、`reference/internal/task/`、
   `reference/internal/display/`、`reference/internal/auth/`,及 `reference/cmd/mixumd/main.go`(装配顺序)

## 项目背景(30 秒版)

mixumOS 是基于 Arch Linux 的 Agent OS。mixum-agentd 是容器内的核心守护进程:
WSS 服务端(TLS 1.3 + 能力令牌)、任务引擎(类 Manus 的指令运行器)、
显示 broker(窗口流分发与接管状态机)、快照引擎的宿主。
已有 Go 参考实现全栈测试通过;你的任务是用 Rust 重写它,
**协议兼容即验收**——Go 侧现成的 e2e 客户端将直接打你实现的端口。

## 方向铁律(不得违背)

- **Rust(tokio 生态)+ 动态链接**,不做 musl 静态宇宙;
- 协议以 `docs/protocol.md` 与 reference 行为为准;**线上字段名是驼峰**
  (replyTo/taskId),能力字符串带点号(task.run),帧头 27 字节小端——
  这些已被 mixum-protocol 锁定,直接复用该 crate,不要重造;
- `reference/` 只读;
- 中文注释与文档全角标点,平实文风;
- 不扩范围:metrics、admin REST、令牌吊销列表等在 roadmap 后置阶段。

## 你的边界(只准动这些)

- 允许:`rust/crates/mixum-agentd/`(新建)、`rust/Cargo.toml`(增量)、
  `tests/cross-protocol/`(如需 agentd 相关向量)、`docs/`(只允许追加
  「Rust 实现记录」节)
- 只读/禁改:`reference/`、`mixcomp/`、`rust/crates/mixum-protocol/`、
  `rust/crates/mixum-snapshot/`(别的 agent 在写)

## 现状(2026-10-04)

- `rust/crates/mixum-protocol`:envelope/ops/帧头/令牌,18 测试全绿,
  含 Go↔Rust 令牌交叉验证。你的协议地基,直接依赖;
- `rust/crates/mixum-snapshot`:另一 agent 并行开发中——**因此快照引擎
  一律走 trait 桩**:定义 `trait SnapshotEngine`(create/restore/fork/tree/
  head),先用内存/临时目录桩实现跑通全部测试;crate 就绪后再接真实现
  (那是后续任务,不在本次范围);
- Go 参考实现全栈 e2e 通过;e2e 客户端在 `reference/cmd/e2eclient`
  (用法见验收)。

## 任务

1. crate 结构(建议 `wire 复用 protocol`、`auth`、`session`、`task`、
   `display`、`snapshott`、`main` 模块),异步运行时 tokio;
   WSS 用 tokio-tungstenite + rustls(**强制 TLS 1.3**;自签证书生成、
   指纹暴露)。
2. 协议层:hello 握手与能力令牌校验(mixum-protocol::token)、
   请求-应答路由、错误码、限速(50/s 突发 100)、二进制帧分发。
3. 任务引擎:`AgentDriver` trait(docs/agent-bridge.md 方式 A 的签名
   即规格,async 适配)、shell 与 external 两个内建驱动(external 桥
   行分隔 JSON 逐字对齐 reference/internal/task/drivers.go)、
   任务状态机与事件订阅、任务前置无感快照调用(trait 桩)。
4. 显示 broker:窗口注册表、帧订阅分发(帧通道满丢旧保新)、
   接管状态机(agent 模式硬拒绝注入 → force claim → user → free)、
   mixcomp 控制通道(unix 套接字,5 字节记录头 + 类型 0x01/0x02,
   200ms 轮询重连)。
5. 进程管理:任务进程组(SIGSTOP/SIGCONT/SIGTERM,供快照冻结与取消)、
   nix crate 即可。
6. main 装配与配置(JSON 配置文件,字段与 reference/cmd/mixumd 一致)。

## 验收标准

- [ ] `cargo test -p mixum-agentd` 全绿;clippy `-D warnings` 干净;
- [ ] **协议兼容验收(最重要)**:编译出 linux/arm64 二进制,替换
      e2e 容器里的 mixumd(复用 `tests/integration/Dockerfile.e2e`,
      改 COPY 路径或写个变体 Dockerfile 放 `tests/integration/`),
      `reference/cmd/e2eclient` 全链路 PASS(握手/系统信息/任务/快照树/
      回退/fork;窗口流部分若 mixcomp 未就绪可跳过并注明);
- [ ] 单元测试覆盖:令牌鉴权、能力检查、接管状态机、external 桥
      请求-应答、错误码路径;
- [ ] docs 追加「Rust 实现记录」:与 Go 的行为差异表(目标:空表);
- [ ] `goals/notes/agentd.md`:交付清单、遗留问题。

## 环境备忘

- 宿主 mac(arm64),Rust 1.97;交叉编译:
  `cargo build --release --target aarch64-unknown-linux-gnu` 需要装
  target 与链接器;**更省事的路线**(推荐):在专用 colima VM 里构建——
  `colima list` 应有 `mixumos`(aarch64),VM 内装 rustup 后
  `cargo build --release`,产物直接给 e2e 镜像 COPY;
- e2e 容器与客户端用法:`docker build -f tests/integration/Dockerfile.e2e
  -t mixumos-e2e .` → `docker run -d -p 17601:7601` → 日志 READY 行取令牌 →
  `go build` + 运行 e2eclient(参数 -addr/-token/-insecure);
- Docker 走 VM(代理已配好);拉镜像失败查 192.168.5.2:7897。

## 协作规程

- 开工先:`cd /Users/ashreo/agent-project/mixumos && git worktree add
  ../mixumos-agentd -b module/agentd`,在 `../mixumos-agentd` 里工作;
- 只提交你边界内的路径;提交信息中文一句话;
- 完成后:分支提交齐全 + notes 交付说明;不要合并 main;
- 遇阻:记录 notes 标注 BLOCKER(尤其 mixum-snapshot trait 桩与真实现的
  接缝,如需调整 trait 签名,先在 notes 里写提案,保持最小改动)。
