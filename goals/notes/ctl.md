# mixum-ctl 交付说明

更新：2026-10-05，接力完成全部验收。接力前的状态与遗留事项见 ctl-handoff.md。

## 分支与提交

- 分支 `module/ctl`，worktree `../mixumos-ctl`；
- `ac2bc7b` 实现 mixum-ctl 主机侧工具链（初版）；
- `1caae44` 修复任务事件与窗口帧订阅；
- `a9a0a77` 修复多节点场景的指纹解析优先级。

## 本次接力完成的修复

1. 任务事件订阅移到 `task.submit` 之前，极快任务不再丢事件；服务端可见配套效果：task submit 后快照树出现「任务前置无感快照」节点（默认 disk 前置策略生效）。
2. 视频帧通道改为 `tokio::broadcast`，多个订阅者互不抢帧；视图断开时发送 `display.detach`。
3. `power --watch` / `--simulate-sleep` 支持从 `config.remote` 回退远端地址。
4. 指纹解析优先级修复：显式 `--fingerprint` > known-hosts 中该地址的记录 > config 指纹（仅对 `config.remote` 生效）。修复前 `connect` 到 B 节点会把 B 的指纹写进 config 全局字段，之后连 A 地址会拿 B 的指纹校验直接失败，多节点场景不可用。补了三个解析顺序单测。

## 验证结果（2026-10-05 实测）

- `cargo test -p mixum-ctl`：7 passed；`cargo clippy -p mixum-ctl --no-deps -- -D warnings` 干净；`cargo check --target aarch64-apple-darwin` 干净；`x86_64-unknown-linux-gnu` 在原生 Arch 容器内 test + check 干净（方法见环境备忘）。
- 对 mixum-e2e（127.0.0.1:17601，reference mixumd + fakecomp）：
  - 证书轮换后用 `connect --fingerprint <当前指纹>` 显式重配对成功，known-hosts 0600；
  - `task submit`（shell 驱动）：实时收到 state/log/progress，终态 completed，退出码 0；
  - `snap tree / create / restore`：/var/lib/mixum/work/verify.txt 走 v1 → 快照 → v2 → 恢复 → v1；
  - `display list / record`：3 帧 300x160 JPEG 落盘；
  - `serve`：浏览器（IAB）窗口列表、canvas 实时画面、按帧头光标元数据叠加（状态行「窗口 1 300x160 光标 100,50」），截图验证；
  - 跨节点推送（源 17601 → mixumos-e2e-dist，docker 网 172.17.0.6:7601）：首轮 65539 字节 / 2 块，第二轮 0 字节 / 0 块，远端树出现推送节点。

## BLOCKER：hybrid 快照在 reference 侧被阻塞（非本模块）

在带 CRIU 的节点上实测（Debian bookworm + criu 3.17.1 + 特权容器 + 静态 mixumd 二进制），两种失败模式都复现：

1. 无运行任务：`snap.create hybrid` → `criu dump --tree 0` → `invalid argument for --tree: 0`。引擎 taskPid 为 nil，解引用后是 0，engine.go 里「pid=0 退化为 disk」的注释与实现不符，没有退化分支。
2. 有运行任务（external 驱动 agent）：criu 报 `The root process is not a session leader. Consider using --shell-job option`。沙箱用 `Setpgid`（进程组）而非 setsid（会话），进程不可能是会话首，而 reference 的 Dump 参数里没有 `--shell-job`。
3. 附带发现：shell 驱动不调用 SetTaskPid（只有 external 驱动调用），shell 任务永远以 tree 0 失败。

修复建议（reference，越界不改）：`memory.go` 的 Dump 参数加 `--shell-job`，或沙箱改用 setsid；engine.go 对 pid=0 显式报错或退化 disk；shell 驱动注册 pid。修好后 ctl 侧无需改动——本模块已验证 hybrid 请求正确到达服务端引擎并如实回传错误，磁盘快照加推送的链路完整可用。

## 环境与复现备忘

- 宿主 mac 缺 x86_64 交叉 gcc，`ring` 编译不过；在 jiuwanli-test VM（原生 x86_64）用容器验证：

  ```bash
  docker --context colima-jiuwanli-test run --rm -v mixum-x86-cargo:/root/.cargo \
    -v <worktree>:/src -w /src/rust archlinux:latest \
    sh -c 'pacman -Sy --noconfirm rust && cargo test -p mixum-ctl --target x86_64-unknown-linux-gnu'
  ```

- e2e 容器重启后证书轮换，known-hosts 会正确拒绝；指纹用 `openssl s_client -connect <addr> -tls1_3` 现算，再用 `connect --fingerprint` 重配。
- Alpine 3.21 没有 criu 包（aarch64 与 x86_64 均无）；验证 hybrid 需 Debian/Arch 容器 + 特权 + 搬静态 mixumd。
- `snap push --remote` 是源容器发起拨号的地址，必须用 docker 网内容器 IP:7601，宿主映射端口（127.0.0.1:17603）在容器内指向自己。
- config.json 已重置为 remote=17601 + 当前指纹；原 token_path 字段存的是令牌值而非路径（读取必然失败），已移除。

## 验收清单对照（任务书）

- [x] `cargo test -p mixum-ctl` 全绿，clippy `-D warnings` 干净；
- [x] e2e 实测：task submit 跟踪到 completed；snap restore 后容器内文件状态变化可验证；
- [x] 跨节点演示：第二轮推送零传输；
- [x] serve 浏览器显示窗口列表与实时画面（真窗口帧，非模拟帧）；
- [x] 电力钩子 `--simulate-sleep`：客户端链路（连接、snap.create、snap.push、配置回退）全部到达服务端；hybrid 快照全链路被 reference 缺口阻塞，证据与建议已详录 BLOCKER 节；
- [x] `docs/host-integration.md`（含本次指纹语义补充）；本交付说明。
