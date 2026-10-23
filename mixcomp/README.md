# mixcomp —— mixumOS 的每窗口隔离合成器

mixcomp 基于 wlroots 0.18，运行在 mixumOS 容器内，只做一件事：
把每个 GUI 窗口的像素单独编码成帧推给 `mixumd`，并把宿主输入注入回目标窗口。
没有面板、壁纸、窗口装饰，也不暴露整个 Linux 桌面。

## 当前架构（以此为准）

1. **单个 headless 锚点输出，不做每窗口场景/输出。**
   进程用 `wlr_multi_backend` + `wlr_headless_backend` 起一个 1280×800 的锚点
   输出，仅为满足「必须有显示器」的客户端（终端、浏览器等）并创建
   `wl_output` 全局。捕获不经过这个输出。

2. **直接读 client buffer。**
   每个 `xdg_toplevel` 映射后拿到自己的 `wlr_surface->buffer`，按窗口定时器
   （默认 30fps，`stream.start` 可调）把 `wlr_texture` 读回 CPU 内存。
   窗口之间没有场景图、没有重绘合成，互不串扰。

3. **`frame_done` 无条件发送。**
   即使没有订阅者、没有新帧，也要给客户端回 `frame_done`，否则客户端会
   节流停帧。捕获失败只记日志，不影响这条节拍。

4. **输入注入是自连接。**
   mixcomp 同时作为 Wayland 客户端连接自己（`WAYLAND_DISPLAY` 由启动时创建），
   绑定 `zwp_virtual_keyboard_manager_v1` 与 `zwlr_virtual_pointer_manager_v1`
   创建虚拟设备；`mixumd` 下发的 `input` JSON 在这里被翻译成 seat 事件。
   键盘焦点需要显式 `wlr_seat_keyboard_notify_enter`——pointer enter 不会
   自动设置键盘焦点。

## 帧编码

| 编码 | 触发 | 说明 |
|------|------|------|
| JPEG | 未协商时的基线 | libjpeg-turbo；按 DRM 格式选 `JCS_EXT_BGRA` / `JCS_EXT_RGBA`；每帧都是关键帧 |
| PNG + damage | `stream.start {codec:"png"}` | 逐行 `memcmp` 找脏区包围盒，只发脏区 PNG 补丁；画面未变但光标移动时发 `format=0` 的空补丁；重新订阅强制首帧全量 |
| H.264 | `stream.start {codec:"h264"}` 且编译期检测到 openh264 | `SCREEN_CONTENT_REAL_TIME`，2 秒 IDR；编码器缺失/失败时回告 `jpeg` |

记录格式：`4 字节小端长度 + 1 字节类型 + 负载`，类型 `0x01` JSON 控制、
`0x02` JPEG、`0x03` H.264、`0x04` PNG。视频负载前是 27 字节帧头：
window id、宽高、长度、flags（关键帧/有光标/damage 补丁）、光标坐标与尺寸。
PNG 补丁在帧头后再跟 20 字节 patch 头（x/y/w/h/format）。

## 与 mixumd 的控制通道

mixcomp 作为客户端连接 `/run/mixum/display.sock`，200ms 轮询一次，
每轮最多处理 16 条消息，避免饿死捕获。断线自动重连。

- mixcomp → mixumd：`window.open` / `window.update` / `window.close` / `stream.info`
- mixumd → mixcomp：`stream.start {id,fps,codec,quality}`、`stream.stop {id}`、
  `input {...}`、`input.mode {id,mode}`

发送是**非阻塞**的：零字节写出时整帧丢弃（实时性优先）；部分写出后
`poll` 200ms 排空，排不空就断线重连——记录流一旦半途而废，后续字节会全部错位。

## 快照与恢复

mixcomp **不参与快照**，也没有窗口状态的持久化接口。`mixumd` 打快照时
冻结的是任务进程组（`SIGSTOP`），mixcomp 本身继续运行：客户端不再提交新
buffer，它会把最后一帧重复编码推送；PNG 模式下画面无变化，通常只产生光标补丁。
`frame_done` 会在此期间向被冻结客户端的 socket 排队。

跨设备恢复后是新容器、新 mixcomp，窗口表为空；旧客户端进程的 Wayland
连接无法重绑，窗口只能由应用重新启动后重建。

## 构建

依赖：wlroots 0.18、libxkbcommon、libjpeg-turbo、libpng、json-c；
H.264 可选（pkg-config 找到 openh264 才会编入）。

```sh
make            # 需要 pkg-config 找到 wlroots0.18
make run        # 本地 headless 试跑，监听 /tmp/mixum-display.sock
```

Wayland 协议头在构建时由 `wayland-scanner` 现场生成；`protocol/` 下的两个
XML 与 wlroots 0.18.2 保持一致（上游 wayland-protocols 已移除 virtual-keyboard）。
