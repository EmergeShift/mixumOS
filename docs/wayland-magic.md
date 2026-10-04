# mixcomp:魔改 Wayland 合成器设计

> 状态:C 实现编译通过(wlroots 0.18,aarch64);运行时调通进行中。
> 本文档记录「魔改了什么、为什么、边界在哪」。

## 1. 要解决的问题

agent 在容器里操作 Chromium,用户在 mac/手机上要看到**单独一个
Chromium 窗口**,实时、带光标、可接管,感觉像原生应用;而不是
VNC 一整块 Linux 桌面。这是 WSLg 的窗口体验 + Parallels 融合模式的容器版。

## 2. 魔改点总览(与常规合成器的差异)

| 维度 | 常规合成器 | mixcomp |
|------|-----------|---------|
| 输出 | 一个物理显示,多窗口叠放 | **不渲染输出**;每窗口直接读 client buffer |
| 窗口管理 | 摆放/焦点/装饰 | 无摆放;窗口即捕获单元 |
| 光标 | wlr_cursor 渲染到场景 | 不渲染;坐标随帧头元数据下发,主机端叠加 |
| 输入 | 真实输入设备 | virtual-pointer/keyboard 协议注入 + 按窗口路由 |
| 生命周期 | 随会话 | 随容器;每窗口独立帧定时器 |

## 3. 捕获管线(v1 定稿)

```
xdg_toplevel map
  → 每窗口 wl_event_loop timer(33ms ≈ 30fps 上限)
      → wlr_surface_send_frame_done()   ← 永远发,客户端靠它节流续帧
      → 若有订阅者:
          surface->buffer (wlr_client_buffer)
            → texture(buffer 自带,缺则 wlr_texture_from_buffer)
            → wlr_texture_read_pixels(preferred_read_format)
            → JPEG(libjpeg-turbo,按 DRM 格式选 BGRA/RGBA 扩展色域)
            → 控制通道推帧(§5)
```

设计取舍:

- **为什么不建每窗口 headless 输出?** v1 早期方案。被否:
  headless 输出 + 场景合成引入 swapchain 与像素读回的复杂度
  (0.18 已移除 `wlr_output_read_pixels`),而单窗口捕获只需要
  client buffer 的纹理——少一层合成,少一处平台相关;
- **代价**:子表面/弹窗(下拉菜单)不在主 buffer 里,v1 丢失;
  阶段 4 如实需,再评估「每窗口场景树 + 自有 swapchain 读回」方案;
- **为什么客户端会持续画帧?** 常见误区:没人订阅时窗口就停帧了。
  frame_done 是无条件的,任务在跑,GUI 应用就活着,快照里的画面
  也是活的;
- 分辨率上限 8192²,防御性读回缓冲按需增长。

## 4. 输入注入(v1 定稿)

mixcomp 连接**自己的** wayland 套接字,成为自己的客户端:

```
mixumd 控制消息 {type:"input", kind, …}
  → input.c 翻译为协议请求(zwlr_virtual_pointer_v1 / zwp_virtual_keyboard_v1)
  → wlroots 生成标准输入事件(与真实设备同路径)
  → main.c 事件路由:focus_window 的 seat 通知
      (enter → motion → button/axis/key,跨窗口注入自动换 focus)
```

- 选择协议而非内部 API:`wlr_pointer_init` 等设备构造函数在 0.18
  不是公开 API;协议路径行为与 ydotool/wtype 一致,零耦合上游;
- 修饰键:客户端侧 xkb_state 维护,按住 → 敲键 → 松开的完整序列;
  keymap 经 memfd 上传(默认 evdev/pc105/us);
- 键名解析:keysym 名(如 `Return`)在客户端 keymap 反查 evdev 码;
- 光标坐标:注入即知(x,y 即窗口内坐标),写入帧头,不渲染真实光标;
- 接管仲裁不在 mixcomp:mixumd 的 broker 决定「谁能注入」
  (见 wayland-magic 末节与 protocol.md §4.4),合成器只认已放行的消息。

## 5. 与 mixumd 的控制通道

mixcomp 主动连 `/run/mixum/display.sock`(mixumd 监听)。
记录格式:`4 字节小端长度 + 1 字节类型`,类型 `0x01` 行内 JSON 控制、
`0x02` 视频帧(负载即 protocol.md §5 的帧格式)。

- 上行:`window.open` / `window.update`(标题、app_id、尺寸)/ `window.close`;
- 下行:`stream.start {id,fps}` / `stream.stop` / `input {…}` / `input.mode`;
- 断线重连:200ms 轮询定时器驱动;帧发送拥塞时丢帧保实时。

## 6. 构建与发行版集成

- 依赖:wlroots0.18、libjpeg-turbo、json-c、libxkbcommon(全部走
  pacman 系统依赖,动态链接,见 distro.md);
- 已知构建坑(供打包参考):
  - pc 名差异:Alpine `wlroots-0.18` / Arch `wlroots0.18`,Makefile 自动探测;
  - `xdg-shell-protocol.h` 需 wayland-scanner 现场生成(打包时放 build());
  - 必须加 `-DWLR_USE_UNSTABLE`;
- 多后端结构:multi + headless 子后端(virtual-pointer/keyboard 设备
  事件经 multi 转发,与 sway 同构);anchor 输出建在 headless 子后端,
  为要求至少一个显示器的客户端(如终端)提供存在感。

## 7. 边界与后续

- 剪贴板:wlr_data_device_manager 已挂载,转发逻辑在阶段 4;
- 多窗口并发流:每窗口独立定时器天然支持;帧率由 broker 的
  stream.start 按订阅者动态调节(阶段 4 做自适应);
- X11 应用:经 XWayland 后置(v1 只承诺 Wayland 原生应用,Chromium 合格);
- wlroots 0.19 升级:捕获管线在 client buffer 层,受 API 变动影响小。
