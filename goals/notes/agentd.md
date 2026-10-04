# mixum-agentd 交付说明

## 已交付

- `rust/crates/mixum-agentd` crate 与 `mixum-agentd` 二进制入口。
- TLS 1.3 自签配置、能力令牌 hello、驼峰 JSON envelope、错误码和 50/s（突发 100）限速。
- `AgentDriver`、`Sandbox`、`EventEmitter` 公共接口；shell 与 external 驱动；本地进程执行和任务取消。
- external 桥请求/应答、持久身份指纹和任务前置快照策略已有单元覆盖。
- `SnapshotEngine` trait 与内存 DAG 桩，支持 create、tree、restore、fork、delete、gc。
- Unix 控制套接字 broker、窗口注册、视频帧订阅、满载丢旧保新的通道设计、agent/user/free 接管状态机。
- 单元测试覆盖令牌鉴权、能力与窗口范围、接管冲突、shell 任务、快照树和驱动注册。

## 验证

在 `rust/` 目录执行：

```text
cargo test -p mixum-agentd
cargo clippy -p mixum-agentd --all-targets --no-deps -- -D warnings
```

两项均通过。workspace 的 `mixum-protocol` 全量 clippy 仍有其原有 `frames.rs` 的 `1 - 1` 等价表达式告警；该文件属于任务书禁止修改的边界，未在本任务中改动。

Go `reference/cmd/e2eclient` 已对本地 Rust 服务实跑并输出 `E2E PASS`，覆盖握手、系统信息、任务、前置快照、快照树、回退和 fork 路径；无窗口时按客户端约定跳过窗口帧检查。

## 遗留问题

- BLOCKER：本机虽已安装 `aarch64-unknown-linux-gnu` Rust target，但缺少 `aarch64-linux-gnu-gcc`，因此本地交叉编译在 `aws-lc-sys` 构建阶段停止；应在 `mixumos` Colima VM 内安装交叉工具链后重试。e2e 容器还需要接入 `mixum-snapshot` 真实实现。
- Linux/arm64 native Docker build 已在 `colima-mixumos` context 中通过：`rust:latest cargo build -p mixum-agentd --release`。
- TLS 证书、签名种子、指纹和 bootstrap token 已接入 `dataDir` 持久化；客户端 TOFU 校验仍由主机侧实现。
- mixcomp 上行帧解析和窗口事件已实现，端到端窗口流仍依赖 mixcomp-runtime 的运行时修复。
