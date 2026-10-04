# mixum-ctl 交接记录

> **接力完成（2026-10-05）**：本文档所列未提交修复与验证缺口已全部处理完毕，
> 交付状态以 [ctl.md](ctl.md) 为准。要点：四个源文件已提交（1caae44）；
> x86_64 目标改在原生 Arch 容器验证通过；证书重配对后全部回归通过；
> 新增指纹解析优先级修复（a9a0a77）；hybrid 在 reference 侧确认被阻塞
> （与下文预判一致，两种失败模式证据已详录 ctl.md）。

更新时间：2026-10-05（Asia/Shanghai）

## 当前分支

- Worktree：`/Users/ashreo/agent-project/mixumos-ctl`
- 分支：`module/ctl`
- 最近提交：`ac2bc7b 实现 mixum-ctl 主机侧工具链`
- 当前有未提交的修复，见“未提交改动”。不要直接把工作树当成干净提交使用。

## 已完成

### 代码与命令

- 新建 `rust/crates/mixum-ctl`，模块为 `transport`、`cmd`、`view`、`power`。
- WSS/TLS 1.3，客户端请求 `mixum.v1`，证书 SHA-256 指纹 TOFU。
- 首次连接打印指纹并写入 `~/.mixum/known-hosts`，Unix 权限 0600；后续连接强校验。
- 配置写入 `~/.mixum/config.json`，Unix 权限 0600；支持 `remote`、`token_path`、`fingerprint`、策略字段。
- CLI 已覆盖：
  - `connect --addr ... [--token ...] [--token-path ...] [--fingerprint ...]`
  - `task submit/list/get/cancel`
  - `snap tree/create/restore/fork/push`
  - `display list/record`
  - `serve`
  - `power --simulate-sleep`、`power --watch`
- `snap push` 的源端令牌使用全局 `--token`，远端令牌使用 `--remote-token`。
- `task submit` 默认携带 `{snapshotPolicy:{pre:true,kind:"disk"}}`，并在提交前订阅 `task.events`。
- 视频帧通道使用 `broadcast`，多个窗口/录制订阅者不会互相抢帧；视图断开时发送 `display.detach`。
- `serve` 默认绑定 `127.0.0.1:7602`，内嵌单文件 HTML/JS，canvas 绘制 JPEG 和帧头光标元数据。
- macOS `power --watch` 已有 `IORegisterForSystemPower` + CoreFoundation run loop 入口；非 macOS 返回 systemd/模拟路径提示。

### 已验证行为

以下验证曾在 reference mixumd/fakecomp 容器上完成：

- TLS 握手、指纹固定、hello。
- `task submit` 完成，并输出 `log`、`progress`、`state` 事件。
- `snap tree`、`snap create`、`snap restore`。
- `display list` 返回 fakecomp 窗口（640x400）；`display record` 成功保存 3 张 JPEG。
- `serve` HTTP 页面、`/api/windows`、浏览器 canvas 画面和光标坐标。
- 跨节点 disk 快照推送：首轮传输 13 字节/1 块，第二轮 `bytesSent=0`、`chunksSent=0`。
- 提交版本曾在 x86_64 Linux Arch 容器内执行过：

  ```text
  cargo check -p mixum-ctl --target x86_64-unknown-linux-gnu
  Finished `dev` profile
  ```

## 未完成或未完全验证

### 1. 当前未提交修复

当前工作树修改了以下文件，尚未提交：

- `rust/crates/mixum-ctl/src/cmd.rs`
  - 默认任务提交增加 disk 前置快照策略。
- `rust/crates/mixum-ctl/src/main.rs`
  - 任务事件订阅移到 `task.submit` 之前。
  - `power --watch` / `--simulate-sleep` 使用配置中的远端地址回退。
- `rust/crates/mixum-ctl/src/transport.rs`
  - 视频帧接收改为 `tokio::broadcast`，新增独立订阅测试。
- `rust/crates/mixum-ctl/src/view.rs`
  - 使用独立帧订阅，并在连接结束时 detach。

这些改动已通过本地：

```text
cargo check -p mixum-ctl
cargo test -p mixum-ctl       # 4 tests passed
cargo clippy -p mixum-ctl --no-deps -- -D warnings
```

当前未提交修复尚未在 x86_64 目标上重跑；提交前应补一次目标检查。

### 2. 当前环境限制

- `hybrid` 合盖模拟依赖服务端 CRIU；现有 e2e 容器报告 `criu=false`，所以服务端会拒绝 memory/hybrid 快照。disk 推送已验证。
- macOS IOKit 代码可在 `aarch64-apple-darwin` check 下编译，但未进行真实合盖事件验证。
- 当前 arm fakecomp 容器重启后更换了 TLS 证书。旧 `known-hosts` 会正确拒绝新证书；必须先用当前证书重新配对，再做录制回归。
- 一个当前 fakecomp 实例曾返回 HTTP 101 但没有 `Sec-WebSocket-Protocol` 响应，客户端按协议严格拒绝。这是服务器实例/容器状态问题，需确认实际运行的 mixumd 与 `reference/internal/server/server.go` 是否一致。
- 完整 workspace clippy 可能被既有 `rust/crates/mixum-protocol/src/frames.rs` 的 `1 - 1` 表达式触发 `clippy::eq_op`；不要为本模块绕过该警告。

## 下一步建议

1. 清理构建产物和 protocol 格式化噪声：

   ```bash
   cd /Users/ashreo/agent-project/mixumos-ctl
   git checkout -- rust/crates/mixum-protocol rust/target
   git clean -fd rust/target
   ```

2. 运行本地检查并提交当前修复：

   ```bash
   cd /Users/ashreo/agent-project/mixumos-ctl/rust
   cargo fmt --all
   cargo test -p mixum-ctl
   cargo clippy -p mixum-ctl --no-deps -- -D warnings
   cargo check -p mixum-ctl --target aarch64-apple-darwin
   git -C .. add rust/crates/mixum-ctl/src/cmd.rs rust/crates/mixum-ctl/src/main.rs rust/crates/mixum-ctl/src/transport.rs rust/crates/mixum-ctl/src/view.rs
   git -C .. commit -m '修复任务事件与窗口帧订阅'
   ```

3. 重新配对当前容器证书后做视图回归。不要猜指纹，使用容器实际证书计算值：

   ```bash
   openssl s_client -connect 127.0.0.1:17602 -tls1_3 -showcerts </dev/null 2>/dev/null \
     | openssl x509 -outform der \
     | openssl dgst -sha256 -c
   ```

4. 确认服务器返回 `Sec-WebSocket-Protocol: mixum.v1` 后再运行：

   ```bash
   mixum-ctl connect --addr wss://127.0.0.1:17602 --token <token> --fingerprint <fp>
   mixum-ctl --addr wss://127.0.0.1:17602 --token <token> --fingerprint <fp> display record 9 --out /tmp/mixum-record --frames 3
   ```

5. 若要完成 hybrid 验收，需要启动带 CRIU 能力的服务端容器，执行：

   ```bash
   mixum-ctl power --simulate-sleep --remote <remote> --remote-token <remote-token>
   ```

## 重要协议/实现注意

- 远端 snapshot push 的两个令牌不能混用：源连接令牌与远端目标令牌可能由不同 identity.key 签发。
- TOFU 指纹变化必须报错，不能自动覆盖 known-hosts；证书轮换应显式重新配对。
- `display record --out` 当前把 out 当目录，输出 `frame-0000.jpg`、`frame-0001.jpg` 等序列。
- 本地视图只应绑定回环地址；不要改成公网监听而不增加认证。
- 不要修改 `reference/`、`mixum-agentd`、`mixum-snapshot`；本任务允许边界是 `mixum-ctl`、workspace 增量依赖、host integration 文档和 notes。
