# mixum-agentd 交接说明

> **状态:已由后续窗口完成交付**,本文「尚未完成」清单全部处理完毕。
> 最终交付记录与验证证据见同目录 [agentd.md](agentd.md)。

更新时间：2026-10-05

## 当前状态

- 工作树：/Users/ashreo/agent-project/mixumos-agentd
- 分支：module/agentd
- 当前提交：ef5d881 实现 Rust mixum-agentd 守护进程
- 主仓库 /Users/ashreo/agent-project/mixumos 仍在 main，本分支未合并。
- reference/、mixcomp/、rust/crates/mixum-protocol/ 未被本任务修改。

## 已完成

### crate、协议和安全

- 新增 rust/crates/mixum-agentd/，包含 auth、wire、task、snapshot、display、server 模块和二进制入口。
- 使用 tokio、tokio-tungstenite、rustls、rcgen、nix、async-trait。
- 复用 mixum-protocol 的 envelope、能力令牌、错误码和视频帧格式。
- 实现 hello 握手、能力检查、窗口范围检查、请求应答、错误码和每连接 50/s（突发 100）限速。
- 强制 TLS 1.3；dataDir 持久化 identity.key、tls-cert.pem、tls-key.pem、admin-token，并输出证书 SHA-256 指纹。
- 控制消息限制 32 MiB，二进制消息限制 16 MiB。

### 任务引擎

- 定义异步 AgentDriver、Sandbox、EventEmitter 接口。
- 内建 shell 和 external 驱动。
- external 桥支持行分隔 JSON 的 hello、log、progress、exec、exec.done、done、fail。
- 任务状态覆盖 queued、running、completed、failed、canceled。
- 本地 Sandbox 使用进程组，支持 SIGSTOP、SIGCONT、SIGTERM、SIGKILL；取消会终止任务进程组。
- 支持任务事件订阅、taskId 限定令牌、任务前置快照策略和失败恢复调用。
- 支持配置中的命名 external agent。

### 快照与显示

- 定义 SnapshotEngine trait 和 MemorySnapshotEngine，支持 create、restore、fork、tree、head、delete、gc、HEAD 和分支。
- 实现 mixcomp Unix 控制通道：5 字节记录头、window.open/update/close、视频帧分发。
- 帧队列满时丢旧保新。
- 实现 agent、user、free 接管状态机，以及 display.list/attach/detach/input/takeover/windows。
- display.windows 会推送窗口变化。

### 文档和测试

- docs/architecture.md 已追加「Rust 实现记录」。
- goals/notes/agentd.md 已记录交付、验证和遗留问题。
- 单元测试覆盖令牌鉴权、身份持久化、能力/窗口权限、接管状态机、shell、external 桥、任务取消和快照树。

## 验证证据

在 rust/ 下通过：

    cargo test --workspace
    cargo clippy -p mixum-agentd --all-targets --no-deps -- -D warnings

结果：agentd 8 项测试通过，mixum-protocol 18 项测试通过，agentd clippy 通过。

Go 客户端曾对本地 Rust 服务实跑并输出 E2E PASS，覆盖握手、sys.info、shell 任务、前置快照、snap.tree、snap.restore；当时没有窗口，所以客户端跳过窗口帧检查，节点数量不足时也没有执行 fork 分支。

Linux/arm64 native build 已在 colima-mixumos 中通过：

    docker --context colima-mixumos run --rm \
      -v /Users/ashreo/agent-project/mixumos-agentd:/workspace \
      -w /workspace/rust rust:latest \
      cargo build --locked -p mixum-agentd --release

宿主机直接交叉编译会因缺少 aarch64-linux-gnu-gcc 在 aws-lc-sys 阶段失败；Colima native build 已绕过该问题。

## 尚未完成

### 1．Rust 二进制尚未放进 Docker e2e 镜像

tests/integration/Dockerfile.e2e 仍写死：

    COPY build/mixumd-arm64 /usr/local/bin/mixumd

尚未提交 Rust e2e Dockerfile 变体，也没有用 Rust 二进制启动该镜像。下一窗口应创建变体或构建前复制 release 二进制，再运行 Docker 镜像和 Go e2eclient。入口脚本还需要确认 Rust 的 /var/lib/mixum/admin-token 和日志约定。

### 2．真实快照后端尚未接入

二进制入口目前固定使用：

    let snapshots = Arc::new(MemorySnapshotEngine::new());

需要等待另一 agent 的 rust/crates/mixum-snapshot API，写适配器替换内存桩。当前内存桩不会保存工作区文件，因此 e2e 的 marker 文件只验证协议流程，不验证真实文件回退。

### 3．跨节点快照同步尚未实现

snap.push、snap.pull、snap.push.import、snap.push.commit 已路由，但当前明确返回 unsupported。需要真实 CAS 后实现 manifest 对账、0x03 chunk 校验、commit 落树、远端根分支和 snap.sync 权限。

### 4．真实窗口帧 e2e 尚未完成

broker 和视频帧解析已实现，但尚未用 mixcomp-runtime 启动实际 Wayland 窗口跑通 display.attach 后至少 3 帧的客户端检查。下一窗口应先确认 /Users/ashreo/agent-project/mixumos-mixcomp 的当前状态，再组合镜像测试。

### 5．workspace 全量 clippy 的已知限制

cargo clippy --workspace --all-targets -- -D warnings 会触发 rust/crates/mixum-protocol/src/frames.rs 原有的 1 - 1 等价表达式告警。任务书禁止修改 mixum-protocol，因此当前只保证 agentd 自身 clippy。

## 下一窗口建议顺序

1. 阅读本文件、goals/mixum-agentd.md 和 docs/protocol.md。
2. 检查另一 agent 的 mixum-snapshot API，写 SnapshotEngine 适配器。
3. 创建 Rust e2e Dockerfile 变体，在 colima-mixumos native arm64 中构建并启动。
4. 先跑无窗口 Go e2e，再启动 mixcomp/foot，补窗口 attach、帧和接管验证。
5. 最后实现 snap.push/pull，补跨节点协议测试，再更新交付 notes。

## 约束

- 不要提交 /tmp/mixum-agentd-data*、rust/target、marker.txt。
- 不要合并 main。
- 不要修改 reference/、mixcomp/、rust/crates/mixum-protocol/、rust/crates/mixum-snapshot/。
