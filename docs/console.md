# Web 控制台(console)

> 状态:v1 已交付(2026-10-04),对 e2e 容器全链路实测通过。
> 本文档说明控制台的架构、协议层实现要点与开发对接方式。

## 定位

控制台是 mixumOS 的用户入口:发任务、看 agent 实时操作的 GUI 窗口、
管理快照树。PWA 形态,iOS/Android 浏览器或添加到主屏幕直接使用;
桌面浏览器同样可用(单页三栏)。

技术栈:Vite + TypeScript + React + zustand。依赖刻意克制:无路由、
无 UI 组件库,样式全部手写(`src/styles.css`),构建产物为纯静态文件
(相对路径,可托管在任意路径下)。

## 布局

桌面三栏:任务(提交表单 + 列表 + 事件流)| 窗口画布 | 快照树。
窄屏(≤980px,手机)折叠为底部三页签,同一套组件切换显示。

## 协议层(src/protocol/)

对齐 `docs/protocol.md` v1 与 mixum-agentd 实现,要点:

- **envelope**:文本帧 JSON,`v/id/op/seq/ts/payload`,请求-应答用
  `id`/`replyTo` 关联;`op.err` 负载 `{code, message}` 映射为
  `ProtocolError` 供界面程序化处理;
- **握手**:子协议 `mixum.v1`(浏览器端必须声明,服务端只认这个);
  `hello.ok` 的 `heartbeat` 驱动 `sys.ping` 心跳,连续两次失败即断开重连;
- **二进制帧**:首字节通道标签,`0x01` video 按 27 字节小端帧头解析
  (协议 §5),`cursorX/Y/flags` 直接交给画布叠加;
- **重连**:指数退避(1s 起,10s 封顶);重连成功后自动补订阅
  `task.events`、`display.windows`,并重新 `display.attach` 附流中的窗口;
- **窗口增删**:`display.windows` 推送单条 Window;`title === "__closed__"`
  视为窗口关闭(mixumd 关闭窗口时以该标题推送,见 agentd display.rs)。

高频视频帧不经过 zustand:`WindowCanvas` 直接订阅 `client.onFrame`,
store 只放任务、窗口列表、快照树等低频状态。

## 窗口画布

- 附流 `display.attach`(15fps),收帧只留最新一帧,绘制用
  `requestAnimationFrame` 合帧——绘制期间到达的帧静默丢弃(只画最新);
  另有 120ms 定时器兜底,内嵌/后台等 rAF 受限环境也能播帧;
- canvas 尺寸 = 帧头 width/height(物理像素),CSS 缩放适配容器;
- **光标来自帧头**(flags bit1),画白色描边箭头叠加,不用本机指针推算;
- 接管:`display.takeover claim/release`;`mode=agent` 时提供「强制接管」
  (确认框,`force: true`);接管后画布聚焦,鼠标/键盘事件回传:
  - 坐标换算:事件位置 × (物理尺寸 / 画布 CSS 尺寸),夹到窗口范围;
  - `pointer_motion` 按 rAF 合帧发送;滚轮映射 `pointer_axis`;
  - 键盘映射 XKB keysym 名(`Enter→Return`、`ArrowUp→Up` 等,见
    `src/util/keysym.ts`),修饰键 `mods: ctrl/alt/shift/logo`;
- 已知平台限制:浏览器拿不到对端证书,协议 §1 的 TOFU 指纹固定无法在
  纯浏览器端实现;开发期由 vite 中继兜底,生产由托管侧(mixum-ctl)
  解决,已在 console/README 注明。

## 快照树

`snap.tree` 的节点数组自建 DAG:层号 = 到根的最长路径,层内按
(分支首节点时间,创建时间)排序,三次贝塞尔连线,SVG 渲染
(`src/util/treelayout.ts`,不用图形库)。节点显示 label/分支/kind/
自动标记/时间/体积;当前 head 蓝框高亮 + head 角标;分支着色取自
低饱和调色板。

树语义的表达(snapshots.md §4):回退(inplace)不删节点——回退后
新快照挂在旧节点下,与旧时间线并排渲染成两个分支;fork 按协议不产生
新节点(head 是指针),节点盒子的 branch 属性与当前分支名如实显示。

操作:回退(inplace)、fork(弹窗输入分支名,`snap.restore strategy=fork`,
冲突由服务端自动加 -2、-3);连接时与手动刷新时拉取,协议没有快照推送,
产生新快照后需刷新。

## 任务视图

提交表单(指令、agent、键值参数、快照策略 pre/kind/onFailure);
`task.list` + `task.event` 维护列表,状态机着色;选中任务看实时事件流
(log 等宽字体自动滚动,step/progress/error 分类着色);`type=window`
事件提供跳转窗口画布的按钮。

## PWA

- `manifest.json`:standalone、克制主题色(`#f4f4f2`)、图标三份
  (192/512/maskable,由 `scripts/gen-icons.mjs` 零依赖生成);
- `sw.js`:只缓存应用壳——导航请求网络优先回退缓存,带哈希的静态
  资源缓存优先;协议数据全走 WebSocket 不经 HTTP,不缓存任何数据;
- iOS meta:`apple-mobile-web-app-capable`、状态栏样式、标题、
  apple-touch-icon;添加到主屏幕后以独立窗口打开。

## 开发对接

见 `console/README.md`:vite 开发中继(默认)或信任证书直连两条路、
fakecomp 演示容器、`scripts/e2e-check.mjs` 冒烟脚本。

## 边界与遗留

边界:本模块只新增 `console/` 与本文档;`mixcomp/`、
`rust/`、`dist/`、`tests/` 只读。

遗留:

- 纯浏览器端无法做 TOFU 指纹固定(平台限制),待 mixum-ctl 托管方案;
- 快照树无推送,新快照需手动刷新;
- H.264/WebCodecs 帧播放为阶段 4 内容,v1 未涉及。
