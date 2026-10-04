# 目标:控制台 PWA(任务 / 窗口画布 / 快照树)

> 你负责 mixumOS 的 Web 控制台(用户发指令、看 agent 实时操作、
> 管快照树的界面;iOS/Android 直接用浏览器/添加到主屏幕)。
> 这是一份自包含任务书:先读本文档,再动手。

## 必读(动手前按序读完)

1. `/Users/ashreo/agent-project/mixumos/docs/protocol.md` —— 你是协议客户端
   (envelope、全部 op、帧格式、接管规则,逐字对齐)
2. `/Users/ashreo/agent-project/mixumos/docs/architecture.md` —— 组件总览与 v1 非目标
3. `/Users/ashreo/agent-project/mixumos/docs/snapshots.md` §4 —— 快照树语义
   (UI 必须表达「回退后旧时间线仍在、fork 是树状分出」)
4. Go 参考实现(只读,协议事实来源):`reference/cmd/e2eclient/main.go`
   (WS 客户端的最小参考实现)

## 项目背景(30 秒版)

mixumOS 是基于 Arch Linux 的 Agent OS:用户发指令 → 容器内 agent 跑任务,
agent 打开的 GUI 应用(如 Chromium)以独立窗口实时流到控制台,用户可随时
接管输入;每次任务前自动打「无感快照」,快照构成 git 式的树,可回退、
可 fork、可在设备间流转。控制台是这一切的用户入口,做成 PWA。

## 方向铁律(不得违背)

- 技术栈:**Vite + TypeScript + React**;依赖克制(状态管理用 zustand 或
  原生 context,路由可不要,单页三栏;**不引入 UI 组件库**,样式手写
  CSS,平实风格,参考「工整的工具界面」,不写营销页);
- 帧播放:v1 JPEG 全帧 + 帧头光标元数据(协议 §5),canvas 绘制,
  **光标位置来自帧头,不是鼠标推算**;H.264/WebCodecs 后置,不要做;
- 协议字段名是驼峰(replyTo/taskId),能力字符串带点号(task.run),
  envelope 结构见 protocol.md §4——照抄,不要「改良」;
- 中文 UI 文案用**全角标点**,语言平实、功能优先(按钮叫什么说什么,
  不写「智能/极速/强大」);代码注释英文中文皆可,文档全角;
- 不扩范围:登录/租户、多容器管理、通知推送后置。

## 你的边界(只准动这些)

- 允许:`console/`(新建整个应用)、`docs/`(只允许新增
  `docs/console.md` 开发与部署说明)
- 只读/禁改:`reference/`、`mixcomp/`、`rust/`、`dist/`、`tests/`

## 现状(2026-10-04)

- 服务端可用:Go 参考实现 mixumd(容器 e2e 通过,WSS 端口 7601,
  自签证书,握手令牌在容器日志 READY 行);Rust agentd 并行开发中,
  协议兼容所以你不需要关心差异;
- e2e 环境:`tests/integration/Dockerfile.e2e`,容器端口映射
  `docker run -d -p 17601:7601 mixumos-e2e`;
- 没有任何前端代码,console/ 是空目录,从零开始;
- 仓库根 README 有快速体验命令。

## 任务

1. **协议客户端**(`src/protocol/`):WebSocket + 请求-应答关联
   (envelope id/replyTo,超时与错误码处理)、任务事件订阅、
   窗口列表订阅、二进制帧解析(ChanVideo 27 字节小端帧头)、
   令牌从 localStorage 读取,设置页可粘贴令牌与地址。
2. **三个主视图**:
   - 任务:提交表单(指令/agent 选择/参数)、任务列表(状态机着色)、
     选中任务看实时事件流(log/step/progress),`window` 事件引导跳转窗口画布;
   - 窗口画布:窗口列表(来自 display.list + display.windows 推送)、
     点开即 display.attach,canvas 播帧 + 光标叠加 + 窗口标题/尺寸、
     **接管按钮**(takeover claim/release,force 确认框)、
     接管后画布上鼠标/键盘事件 → display.input(坐标换算物理像素);
     静默丢帧策略(只画最新帧);
   - 快照树:从 snap.tree 的节点数组自建 DAG 渲染(分层布局 SVG,
     不用图形库);节点信息(分支/标签/时间/kind/auto)、
     回退(inplace,fork 弹窗输入分支名)、恢复到任意历史节点、
     当前 head 高亮。
3. **PWA**:`manifest.json`(名称 mixumOS、独立窗口、主题色克制)、
   Service Worker(仅缓存应用壳,不缓存数据)、iOS Safari 添加到
   主屏幕的 meta 标签。
4. **开发对接**:提供 `console/README.md`:如何起 e2e 容器、如何处理
   自签证书(推荐:生成本地 CA 并信任,给出脚本;或 dev 模式
   `vite --mode insecure` 下用带 rejectUnauthorized:false 的 Node 中继——
   选一条实现并写清风险:仅限本机开发);
   `npm run build` 产物为纯静态文件,可由 mixum-ctl serve 或任意静态
   服务器托管(相对路径)。

## 验收标准

- [ ] `npm run build` 零错误零警告;`npm run dev` 可用;
- [ ] 对 e2e 容器实测全链路:粘贴令牌 → 提交任务(shell 驱动,写文件)
      → 事件流实时滚动 → 任务 completed;
- [ ] 快照树正确渲染 e2e 数据(genesis → 前置快照 → fork 分支),
      回退/fork 操作生效(容器内树状态可核对);
- [ ] 窗口画布:mixcomp 就绪则对真窗口播帧并接管打字
      (foot 终端里敲出字来);未就绪则用服务端模拟窗口帧演示,
      在 console/README 与 notes 注明;
- [ ] iOS Safari(或 iPhone 模拟视口)可用:布局不破、主屏添加后独立窗口打开;
- [ ] `docs/console.md`:架构、协议层说明、开发对接指南(全角标点);
- [ ] `goals/notes/console.md`:交付清单、遗留问题。

## 环境备忘

- 宿主 mac,node 24 / npm 11 已装;`npm create vite` 起手(react-ts);
- e2e 容器:`colima list` 确认 `mixumos` VM;`docker build -f
  tests/integration/Dockerfile.e2e -t mixumos-e2e .`;
  `docker run -d --name mixum-e2e -p 17601:7601 mixumos-e2e`;
  令牌:日志 READY 行;Docker 代理已配,失败查 192.168.5.2:7897;
- 控制台从 mac 浏览器直连 `wss://127.0.0.1:17601/ws`。

## 协作规程

- 开工先:`cd /Users/ashreo/agent-project/mixumos && git worktree add
  ../mixumos-console -b module/console`,在 `../mixumos-console` 里工作;
- 只提交你边界内的路径(`console/node_modules` 永不入库);
  提交信息中文一句话;
- 完成后:分支提交齐全 + notes 交付说明;不要合并 main;
- 遇阻:记录 notes 标注 BLOCKER。
