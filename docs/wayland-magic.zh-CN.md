# mixcomp:魔改 Wayland 合成器设计

> 状态:C 实现编译通过并在 e2e 容器内运行时调通(wlroots 0.18,aarch64);
> 窗口流、输入注入、接管语义验收通过。本文档记录「魔改了什么、为什么、
> 边界在哪」,末节为运行时调通记录。

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
- 下行:`stream.start {id,fps}` / `stream.stop` / `input {…}` / `input.mode` /
  `snapshot.freeze` / `snapshot.thaw`(快照期间暂停/恢复捕获节拍);
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

## 8. 运行时调通记录(2026-10-04)

mixcomp 在 e2e 容器(Alpine 3.21 + wlroots 0.18.1,aarch64)内稳定运行:
e2eclient 输出「窗口流正常(≥3 帧)」;tests/integration/displaytest 十二项
全过——帧头光标坐标随 pointer_motion 注入可观测变化((20,10)→(100,50)),
接管 claim/release/未知动作 conflict/不存在窗口错误码全部符合预期。
编译警告 -Wall -Wextra 清零。以下为逐项根因,后人勿再踩。

### 8.1 注入客户端段错误(手搓协议表签名错序)

gdb 定位崩溃在注册表回调创建虚拟 pointer 的 `wl_proxy_marshal_flags`。
根因:input.c 手搓的 `zwlr_virtual_pointer_manager_v1` 协议表把
create_virtual_pointer 签名写成 `"no"`,上游 XML 是 `"on"`(seat 在前,
new_id 在后)。libwayland 的 `wl_argument_from_va_list` 对 `'n'`(new_id)
也消费一个变参(生成桩传 NULL 占位,随后由 create_outgoing_proxy 覆写),
错序后 `'o'` 槽位越界读到寄存器残留垃圾;序列化解引用垃圾指针,垃圾内容
随寄存器状态而变——恰好为 NULL 时走 err_null 干净失败,非 NULL 可解引用
时直接段错误,这解释了「时崩时不崩」。同表还有:管理器 destroy 的 opcode
错位(上游在 1 而非 2)、frame/axis_source 的 opcode 对调、keymap 写成
"ihu" 且调用传 (fd,size,format)——libwayland 按 "ihu" 把 size 当 fd 去
复制,EBADF,keymap 请求被丢弃。

修复:弃用手搓协议表,Makefile 用 wayland-scanner 从
`mixcomp/protocol/*.xml` 生成客户端头文件与私有码,注入调用全部改走
生成桩函数,new_id 变参占位与 opcode 顺序由生成器保证。XML 与
wlroots 0.18.2 的 protocol/ 目录同源——上游 wayland-protocols 已移除
virtual-keyboard,Alpine 3.21 亦无任何包携带该 XML,vendor 是唯一稳定
来源。keymap 的 fd 在封送时已被 libwayland 复制,close 放在请求发出后
即可(注释里的「sendmsg 时 dup」说法有误,已订正)。

### 8.2 wl_seat name 事件触发 wl_abort

注入客户端绑定 wl_seat v5+,服务端必发 name 事件;libwayland 1.22 起
`wl_closure_invoke` 对 NULL 监听器槽位直接 abort(旧版仅告警跳过)。
此前监听器 `.name = NULL`,注入客户端在第一次 dispatch 即整体崩溃。
修复:补 no-op name 处理函数。

### 8.3 foot「no monitors available」(wl_output 全局不再自动创建)

wlroots 0.18 起输出使能后不再自动创建 wl_output 全局,合成器必须显式
调用 `wlr_output_create_global`。anchor 输出建了但客户端看不见。
修复:commit 成功后显式创建全局。

### 8.4 客户端贴帧即被断连(「Unknown buffer type」)

wlroots 0.18 移除了 `wl_display_init_shm`,但该符号仍由 libwayland-server
以兼容名义导出(遗留实现)。mixcomp 误调它,创建出的 wl_shm 全局发出的
缓冲实现指针落在 libwayland 内,wlroots 的 `wlr_buffer_try_from_resource`
只认自家实现,报 Unknown buffer type 并断开客户端连接。最小 xdg 客户端
能把链路走到 configure,故该问题只在与 wlroots 有深度交互的真实客户端
(foot)上暴露。修复:改用 `wlr_shm_create_with_renderer`。

### 8.5 窗口永不注册、configure 永不出现

两处 0.17→0.18 语义变化叠加:

1. `xdg_shell.events.new_surface` 现在只在 get_xdg_surface 时发射一次,
   且此时 role 必为 NONE;带 role 的入口是 `new_toplevel` 信号(tinywl
   0.18 同款)。此前按 `role == TOPLEVEL` 过滤 new_surface,所有窗口
   被静默丢弃;
2. initial commit 后合成器必须主动回一个空 configure,客户端 ack 并
   贴上首帧缓冲后才算映射完成(0.18 不再代发首次 configure)。

修复:改挂 new_toplevel,窗口 commit 回调对 `initial_commit` 调
`wlr_xdg_toplevel_set_size(xdg_toplevel, 0, 0)`(尺寸由客户端自选,
符合 mixcomp 不做摆放的定位)。

### 8.6 注入事件无人接收、坐标恒为 0

1. 0.18 的虚拟 pointer/keyboard 设备不经 backend 的 new_input,须分别
   监听两个 manager 的 `new_virtual_pointer` / `new_virtual_keyboard`
   信号。此前误挂 backend new_input,路由监听从未挂上;
2. `wlr_pointer_motion_absolute_event` 的坐标从布局绝对值改为 [0,1]
   归一化值(事件不再携带 extent),路由时按窗口尺寸换算回窗口内坐标。

### 8.7 e2e 测试链路的客户端侧约束

(2026-10-04 记录;原 Go e2eclient 已随参考实现移除,约束演化见
tests/integration/。)原 e2eclient(gorilla/websocket)默认读缓冲 4096,
单帧超过即触发宿主侧 bufio 越界 panic,且其帧循环的读截止会让连接失效;
帧循环与应答循环并发读同一 ws 亦不可靠。约束规避:entrypoint 以
`initial-window-size-pixels=300x200` 起小窗口 foot,单帧约 1.5KiB;
注入/接管验收改用 tests/integration/displaytest(单读循环、1MiB 读缓冲、
wire 层按 protocol.md 手写,兼作协议交叉验证)。displaytest 顺带发现的
两个 agentd 侧问题:Inject 对不存在窗口的错误码——Rust 实现已按 §4
语义改为 not_found(2026-10-17;此前所有注入错误一律映射 conflict)。
窗口 agent 模式切换在 Rust 实现中已全链接线(2026-10-17 核对):
task.rs 活跃计数 0↔1 → bin 回调 → broker.set_tasks_active(free↔agent)
→ inject 在 agent 模式硬拒绝、claim 需 force;mixcomp 收 input.mode
仅记录(仲裁在 mixumd,v1 决策)。

