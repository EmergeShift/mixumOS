# macOS 实机验证清单

本机（Windows）无法覆盖的验收项：IOKit 电力钩子、本地视图的真实浏览器体验、
MixumART 的 Metal 原生窗口壳。本文给出在一台真实 Mac 上逐项执行的步骤与判据。

前置：

- macOS 13+，Xcode Command Line Tools（`xcode-select --install`）
- Rust（aarch64-apple-darwin）、Node 20+
- Docker Desktop for Mac 或 Apple Container（二选一）
- 已有一个可连的 mixumOS 节点（本机容器或局域网机器），拿到 `wss://<host>:7601`
  的用户令牌与证书指纹

证据统一收集：命令输出（复制文本）、关键页面截图、`pmset -g log` 片段。

## 1. IOKit 电力钩子（合盖迁移）

```sh
cargo build --release -p mixum-ctl
./target/release/mixum-ctl connect --addr wss://<host>:7601 --token <token> --fingerprint <fp>
./target/release/mixum-ctl --token <token> \
  power --watch --remote wss://<host>:7601 --deadline-secs 30
```

判据：

1. 合盖或 `pmset sleepnow` 触发后，日志出现 **CanSystemSleep → 仅放行**，不应触发迁移；
2. 随后 **WillSleep** 事件才触发快照推送/迁移，系统保持唤醒直到完成或 30s 超时；
3. 迁移完成后 `IOAllowPowerChange` 放行（`pmset -g log` 中 Sleep 时间戳晚于迁移完成）；
4. 失败注入：拔网线/停远端后合盖，超时后仍能正常睡眠（不挂死），日志记录超时；
5. 远端 `mixum-ctl snap tree`（或 console 快照树）出现新 head，时间与合盖时刻一致。

`power --simulate-sleep` 可在不真实合盖的情况下走同一链路，先做冒烟。

## 2. 本地窗口视图（浏览器）

```sh
./target/release/mixum-ctl serve --bind 127.0.0.1:7602 \
  --addr wss://<host>:7601 --token <token> --fingerprint <fp>
```

用 Safari 与 Chrome 各验证一次：

1. 管理页列出窗口，点击打开独立视图页；标题栏显示窗口标题与 free/agent 徽标；
2. 画面为 PNG（或 JPEG 回退）渲染，静态时无流量（损伤门控），移动指针/键盘输入立即出新帧；
3. 接管按钮：free → user 后可注入鼠标/滚轮/键盘，F5/F11/F12 仍归浏览器；
4. 断线（停节点 3s 再启动）后视图页 1s 内自动重连并恢复画面；
5. 关闭视图页后页面 WS 断开、远端 attach 释放（静态窗口恢复零帧流量，节点日志可见 detach）。

## 3. MixumART 原生窗口壳（Metal）

```sh
cd MixumART/apps/desktop
npm install
npm run tauri dev
```

1. 连接 agentd，扫描窗口并打开独立窗口；macOS 默认走 **MTKView + Metal** 原生叠加层；
   设置 `MIXUM_NATIVE_METAL=0` 后应回退 WebView canvas（对照两组画面一致性）；
2. 输入注入、IME（text-input）、host→remote 粘贴、remote→host 剪贴板（Cmd/Ctrl+Alt+V）；
3. XWayland 应用（如 xterm）窗口显示与输入；X11 文本走内置 XIM；
4. 多 comp 源：A（mixcomp）与 B 合成器窗口应同时出现，标签区分正确；
5. 合盖场景：合盖前打开的窗口在云端迁移后，重新连接能看到同一窗口（快照/续跑链路）。

## 4. 结果记录模板

| 项 | 结果 | 关键证据 | 备注 |
|---|---|---|---|
| 合盖 CanSleep 仅放行 | ✅/❌ | 日志片段 | |
| 合盖迁移完成/超时放行 | ✅/❌ | pmset 时间戳 | |
| 本地视图 PNG + 输入注入 | ✅/❌ | 截图 + 帧计数 | Safari/Chrome |
| 视图断线重连 | ✅/❌ | 日志 | |
| Metal 原生壳渲染 | ✅/❌ | 截图 | 与 canvas 对照 |
| 剪贴板/IME/XWayland | ✅/❌ | 操作录屏/截图 | |
| 迁移后续跑窗口 | ✅/❌ | 窗口截图 | |

已知限制：IOKit 实机路径与 Metal 壳在 CI 中不可测；Apple Container 后端目前只实现
探测/列举，创建与启停仍为契约预留。验证通过后把本表结果回写 roadmap 对应条目。
