# mixum-agentd 交付说明

更新时间：2026-10-05（二期交付,接续 agentd-handoff.md 的遗留清单）

## 已交付

### 一期（前次交付,见 agentd-handoff.md）

- `rust/crates/mixum-agentd` crate 与 `mixum-agentd` 二进制入口：WSS（TLS 1.3）、
  能力令牌 hello、驼峰 JSON envelope、错误码、50/s（突发 100）限速。
- 任务引擎（`AgentDriver`/`Sandbox`/`EventEmitter`,shell 与 external 驱动,
  行分隔 JSON 桥）、显示 broker（帧订阅满载丢旧保新、接管状态机）。
- `SnapshotEngine` trait 与内存桩、单元测试与文档追加。

### 二期（本次）

- **真实快照后端接入**:`RealSnapshotEngine` 适配器封装 `rust/crates/mixum-snapshot`
  （目录 CAS + 树）,装配同 reference/cmd/mixumd（dataDir/snapshots、root=workRoot、
  skip=`.mixum`、encryptSnapshots）。经 `real-snapshot` 特性装配,缺省内存桩
  保留,保证分支独立可构建;sys.info 的 snapshotBackend/criuAvailable 改由引擎上报。
- **跨节点同步（协议 §6）收发两端**:
  - 接收端:push.import 逐哈希对账回 missing、0x03 块帧哈希校验落 CAS、
    commit 校验缺失块到齐后落树,父不在本地树时成新分支根并追加「（远端根）」;
  - 推送端:`sync::push_node` 拨号远端（wss,自签证书跳过校验同 reference
    client.go;显式 `ws://` 走明文供测试）、hello（mixumd-sync/0.1）、import、
    只传缺失块、commit,回 `{node,chunksSent,bytesSent,dedupRatio}`;
  - `snap.pull` 与 reference 一致不设处理器（拉取=反向调用对方 push）。
- **装配修正**:`--config <path>` 空格形式（entrypoint 用法）;空配置文件回退
  默认值;主机名缺省取 gethostname;快照冻结回调冻结全部任务进程组
  （`LocalSandbox::freeze_all`,对齐 FreezeAll）;bootstrap 管理令牌 10 年有效期。
- **错误码对齐 Go**（差异表清空）:snap.create 失败 `internal`、snap.fork 失败
  `not_found`、snap.delete 失败 `bad_request`、未知操作 `unsupported`
  （消息含 op 名）;二进制帧限速与「未知二进制通道」语义同 Go。
- **e2e 资产**:`tests/integration/Dockerfile.e2e-rust`（Rust 守护进程变体,
  二进制以 `build/mixum-agentd-arm64` 名义 COPY 为 `mixumd`）与
  `entrypoint-rust.sh`（沿用 mixcomp 分支的质量 30 + 300x200 小窗口约束）;
  `examples/push_node.rs` 推送工具。

## 验证证据

```text
# rust/ 下:
cargo test --workspace                                        # 36 项全绿（stub）
cargo test --workspace --features mixum-agentd/real-snapshot  # 42 项全绿
cargo clippy -p mixum-agentd --all-targets --no-deps -- -D warnings            # 干净
cargo clippy -p mixum-agentd --features real-snapshot --all-targets --no-deps -- -D warnings  # 干净
```

workspace clippy 全绿需 `mixum-protocol` 的既有 `frames.rs` `1 - 1` 告警修复,
该文件禁改,故 clippy 以 `-p mixum-agentd` 为准（前次交付同一结论）。

容器与跨实现验证（musl arm64 二进制在 colima-mixumos 的 rust:alpine 内构建）:

- **e2e 全链路**（mixumos-e2e-rust 镜像 = mixum-agentd + mixcomp + foot）:
  原版 Go e2eclient 验证握手、sys.info（backend=directory、compositor=true、
  windows=1）、窗口流 ≥3 帧;task.submit 超时为其自身并发读缺陷
  （wayland-magic.md §8.7,Go 服务端同样复现,非服务端问题）。修复版客户端
  （单读循环,复用 reference wire 包）对 Rust 容器与 Go 参考容器均输出
  `E2E PASS`:窗口流/任务/快照树/回退/fork 全部通过。
- **真实文件回退**:容器内任务落文件 → snap.create 得 chunks=1 size=19 →
  回退 genesis → 宿主侧确认文件消失（CAS 打包与物化回滚均真实生效）。
- **跨实现推送**:Rust `examples/push_node` 把 Rust 引擎创建的节点推入
  **Go 参考容器**,节点以原 id/分支落 Go 树（Manifest JSON 两实现逐字段一致,
  根节点按语义不加远端根标记）。
- **双节点回环**:`rust/crates/mixum-agentd/tests/sync_loopback.rs`——
  两个 agentd 实例间推送,验证全量块传输、dedup 统计、幂等重推（零发送）、
  远端根标记、不存在的节点报 `not_found`。

## 与 Go 的行为差异

目标空表已达成（详见 docs/architecture.md「Rust 实现记录」）。备忘一条
**两实现共有**的语义（非差异）:任务进程不 chdir,shell 任务文件落在守护
进程 cwd 而非 workRoot;快照引擎打包 workRoot,真实 agent 驱动落文件时
应使用绝对路径（e2e 已按此验证）。

## 遗留问题

- 无 BLOCKER。
- `real-snapshot` 缺省关闭,因 `rust/crates/mixum-snapshot` 与
  `tests/cross-protocol/snapshot-vector.json` 属快照模块边界、尚未提交
  （本工作树内有本地未跟踪副本用于构建测试,不入本分支提交）。快照模块
  合并 main 后,建议把 `real-snapshot` 改为默认特性,一行改动。
- e2eclient 的并发读缺陷建议按 mixcomp 分支的 displaytest 思路重写
  （主机 CLI 模块范围）。
- 交叉编译仍走 colima VM 内 native 构建（musl,aws-lc-sys 需 apk 装
  cmake/perl）;宿主直交叉的 aarch64-linux-gnu-gcc 问题与前次一致。
