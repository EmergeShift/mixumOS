# mixumOS 0.1.4 — aarch64

状态:pre-release。发布日期:2026-10-20。

## 亮点

- **B 路线接入 XWayland**：X11 toplevel、override-redirect 菜单、指针/键盘、
  X11↔Wayland 剪贴板、Unicode 文本和 XIM 输入均可工作。
- **X11 环境自动注入**：agentd 任务、external driver 和登录 shell 自动从
  `/run/mixum/xwayland.display` 设置 `DISPLAY` 与 `XMODIFIERS=@im=mixum`，
  X11 应用不再需要开发者手带参数。
- **文件传输**：新增 `file.put` / `file.put.commit` / `file.get` 和 0x05
  文件二进制通道；`mixum-ctl file put/get` 可直接上传/下载，上传成功返回
  agent 可读的绝对路径。
- **Host 侧体验**：MixumART 支持把 host 文件拖入 surface，上传后把远端路径
  注入目标窗口；X11 XIM 使用内置 `@im=mixum` server。
- **Gateway M3**：k8s Lease 节点注册表、副本间双向路由、跨副本掉线清理。

## 新增

- `file.put` / `file.put.commit` / `file.get`，文件块通道 `0x05`，支持分块
  上传、大小/可选 sha256 校验、`workRoot/transfers` 落盘。
- `mixum-ctl file put <local> [--name NAME]`：上传并输出远端绝对路径。
- `mixum-ctl file get <remote> [--out LOCAL]`：下载并校验大小。
- `xorg-xwayland` 进入 `mixum-base`，镜像内提供 `/usr/sbin/Xwayland`。
- B compositor XWayland/X11Wm、override-redirect 菜单合成/输入、X11 剪贴板桥接、
  X11 Unicode 文本注入、XIM server。
- X11 自动环境：agentd LocalSandbox/external driver 与
  `/etc/profile.d/mixum-x11.sh`。
- MixumART canvas 文件 drop：上传文件并把远端路径作为文本发送到 surface。

## 变更

- `display.input` 转发 Wayland `text` 字段。
- `display.clipboard` 拉取远端 selection。
- 镜像新增 `xorg-xwayland` 依赖（镜像体积因此增加，0.1.4 aarch64 OCI 约
  500 MiB）。
- Gateway M3 节点注册/路由能力合入。

## 文件传输用法

```sh
# host → node；stdout 是远端路径，适合 agent 脚本
path=$(mixum-ctl file put ./input.bin --name input.bin)

# node → host
mixum-ctl file get "$path" --out ./result.bin

# 结构化输出
mixum-ctl file put ./input.bin --json
```

上传默认落到 `workRoot/transfers/<name>`。文件操作需要 `admin` 能力；
`file.get` 只允许 `workRoot` 内的路径。

## 验证

- 在 `map-node-017` / 0.1.4 构建机上验证 `file put` / `file get` 3 MiB
  随机文件，sha256 一致。
- X11：XClock 出窗、Xev 指针/键盘、override 窗口点击、剪贴板、
  `xterm` `read` 收到 `XIM中文`。
- 镜像内 `Xwayland 24.1.14`，登录 shell 实测
  `DISPLAY=:0`、`XMODIFIERS=@im=mixum`。
- 0.1.4 镜像 `mixum-ctl` 与 agentd 均来自本版本包。

## 已知限制

- 文件 drop 是 host→surface 的文本/文件内容通道；**完整跨 VM XDND / Wayland
  data-source 文件拖拽仍未实现**，拖入文件后注入的是远端路径文本。
- `file.*` 目前要求 `admin` 能力；尚未拆分细粒度 file 能力。
- 非登录 `docker exec` 不会 source profile；请用 `sh -lc` 或由 agentd 启动。
- CRIU 跨内核验证 PR 未合入本版本；跨内核仍按 `arch/kernelRelease` 精确拒绝。
- 本次发布为 aarch64；未同时产出 x86_64 镜像。

## 升级提示

- 从 0.1.3 升级后，X11 应用可直接通过 agentd/登录 shell 启动；旧脚本若显式
  设置 `DISPLAY`/`XMODIFIERS` 仍兼容。
- 镜像变大是 `xorg-xwayland` 依赖树导致；如只用 A 路线可继续不启动 B
  compositor。
