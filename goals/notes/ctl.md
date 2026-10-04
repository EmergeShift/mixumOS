# mixum-ctl 交付说明

已完成：

- 新建 `rust/crates/mixum-ctl`，包含 `transport`、`cmd`、`view`、`power` 模块。
- WSS TLS 1.3、`mixum.v1` 子协议、证书 SHA-256 TOFU、`~/.mixum/known-hosts` 0600 存取。
- `connect`、`task submit/list/get/cancel`、`snap tree/create/restore/fork/push`、`display list/record`、`serve`、`power --simulate-sleep` 命令。
- 任务提交同时订阅 `task.events`，输出事件 JSON 与状态轮询结果；终态失败返回非零退出码。
- 本地视图内嵌 HTML/JS，`/api/windows` 列窗口，`/api/stream/<window>` 转发 JPEG 帧并绘制光标。
- `cargo test -p mixum-ctl` 通过；`cargo check -p mixum-ctl` 通过。
- 已用运行中的 reference mixumd 实测 TLS 握手、任务完成、事件跟踪、快照树、创建和恢复；跨容器推送链路已定位到双端令牌参数歧义并改为 `--remote-token`。

验证限制：

- 已在运行中的 reference mixumd/fakecomp 节点实测：任务完成与 `log/progress/state` 事件、快照树/创建/恢复、窗口列表、3 帧 JPEG 录制、本地视图页面，以及跨节点推送首轮 13 字节/1 块、第二轮 0 字节/0 块去重。
- `cargo clippy -p mixum-ctl --no-deps -D warnings` 通过；完整 workspace clippy 仍可能被既有 `mixum-protocol/src/frames.rs` 的 `1 - 1` 索引表达式阻塞，该文件不在本模块边界。
- Darwin 已提供 `power --watch` 的 IOKit `IORegisterForSystemPower`/CoreFoundation run loop 入口；Linux 继续返回 systemd sleep.target 说明并建议 `--simulate-sleep`。
- 当前 e2e 容器未启用 CRIU，因此 `hybrid` 模拟合盖会由服务端拒绝“需要 CRIU”；disk 快照推送链路已验证。
- `aarch64-apple-darwin` check 通过；`x86_64-unknown-linux-gnu` 在本机无 Linux sysroot/交叉 C 头文件（`assert.h`、`sys/types.h`）时无法完成依赖的 `ring/aws-lc-sys` 编译。
- `rustls` 已关闭默认 `aws-lc-rs` 后端，仅使用 `ring`，减少 Linux 交叉构建依赖；当前仍需 Linux sysroot 才能编译 `ring`。
