# mixcomp —— mixumOS 的魔改 Wayland 合成器

mixcomp 是基于 wlroots 的合成器，为「agent 在容器内操作 GUI、用户在主机上
看到原生窗口」这一核心体验而生。与常规合成器的差异（即魔改点）：

1. **每窗口一个独立场景图 + 独立 headless 虚拟输出**。
   常规合成器是「一个输出、多个窗口」；mixcomp 反过来——每个 xdg_toplevel
   映射时获得自己的 `wlr_scene` 与自己的虚拟输出，窗口之间零渲染串扰。
   这使得「把 Chromium 的窗口单独流给主机、而不暴露整个 Linux 桌面」
   成为合成器的原生能力，而非屏幕截图裁剪。

2. **WSLg/Coherence 式输出**：mixcomp 不连接真实显示。帧从每个虚拟输出
   读回像素，JPEG 编码后经 Unix 域套接字推给 mixumd（协议见下），
   mixumd 再按窗口 id 分发给主机端订阅者。主机上每个窗口就是一个
   独立的原生窗口，光标位置随帧头实时映射。

3. **输入注入内建**：mixcomp 自持一个 wlr_pointer + wlr_keyboard 设备，
   mixumd 下发的输入事件直接路由到目标窗口的 wlr_seat——agent 用自己的
   输入源操作，用户接管时同样经此路径，二者由 mixumd 的接管状态机仲裁。

4. **无桌面环境**：没有面板、没有壁纸、没有窗口装饰管理器。
   mixcomp 只做一件事：让容器内的 GUI 应用被单独地、实时地、可接管地看到。

## 与 mixumd 的控制通道

mixcomp 作为客户端连接 `/run/mixum/display.sock`（mixumd 监听）。
记录格式：`4 字节小端长度 + 1 字节类型 + 负载`，类型 `0x01` 为行内 JSON 控制，
`0x02` 为视频帧（负载即 wire.VideoHeader + JPEG 数据，与 Go 端
`internal/wire` 完全对齐）。

控制消息（mixcomp → mixumd）：`window.open` / `window.update` /
`window.close`，字段 id、appId、title、width、height、focused。
（mixumd → mixcomp）：`stream.start {id,fps}`、`stream.stop {id}`、
`input {...}`、`input.mode {id,mode}`。

## 构建

见 os/ 构建脚本；依赖：wlroots0.18、libxkbcommon、libjpeg-turbo、pixman。

```sh
make            # 需要 pkg-config 找到 wlroots0.18 与 libturbojpeg
make run        # 本地试跑（headless）
```
