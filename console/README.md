# mixumOS 控制台(PWA)

用户入口:发任务、看 agent 实时操作窗口、管快照树。Vite + TypeScript +
React,无 UI 组件库,样式手写。协议客户端逐字对齐 `docs/protocol.md` v1。

## 快速开始

```sh
cd console
npm install
npm run dev          # http://localhost:5173
```

打开页面 → 「设置」→ 粘贴服务地址与能力令牌(容器日志 READY 行,或
`/var/lib/mixum/admin-token`)→ 保存并连接。令牌与地址存在 localStorage。

## 对接 e2e 容器(自签证书)

服务端是 `wss://` 自签证书,浏览器不允许跳过校验,两条路:

### 方式一:vite 开发中继(默认,推荐日常开发)

`npm run dev` 时 vite(Node 进程)把浏览器的 `ws://localhost:5173/ws`
转发到 `wss://127.0.0.1:17601`,`secure: false` 只作用于本机 Node 进程。

- 设置里地址填 `/ws` 即可走中继;
- 对接其他容器:`MIXUM_DEV_TARGET=wss://127.0.0.1:17602 npm run dev`;
- **风险:本机开发专用**。中继不校验服务端证书,生产构建不经此路径。

### 方式二:信任证书后直连(接近生产形态)

```sh
console/scripts/trust-cert.sh mixum-e2e 17601
```

脚本用 openssl 取出容器证书打印 SHA-256 指纹,加入登录钥匙串信任
(系统可能弹一次确认框)。之后设置里直填 `wss://127.0.0.1:17601`。
**风险:信任的是开发容器的自签证书,仅限本机。**

## 演示容器(mixcomp 未就绪时)

e2e 容器里的 mixcomp 若在修复中(窗口数为 0),可用 fakecomp 模拟窗口:

```sh
console/demo/run-demo.sh    # 起独立容器 mixum-demo,端口 17602
```

脚本会打印地址与令牌。模拟窗口按 10fps 推 JPEG 帧(帧头光标移动),
接管后按键会点亮画面格子(输入回传可视化)。

## 冒烟脚本

```sh
node scripts/e2e-check.mjs wss://127.0.0.1:17601 <令牌>
```

零依赖(Node ≥ 22),跑通 握手 → 系统信息 → 窗口流 → 任务 → 快照回退/fork,
结束时打印 `E2E PASS`。脚本内关闭 TLS 校验,仅限本机开发。

## 构建与部署

```sh
npm run build       # dist/ 为纯静态文件,全部相对路径
```

产物可由 mixum-ctl serve 或任意静态服务器托管(含子路径)。静态部署后
浏览器直连 `wss://` 地址,需先信任服务端证书(方式二);协议数据全走
WebSocket,不经 HTTP,Service Worker 只缓存应用壳。

## 目录

```
src/protocol/   协议客户端(WS、envelope 关联、二进制帧头、重连)
src/state/      zustand store(高频帧不进 store,画布直接订阅)
src/components/ 三视图 + 设置 + 画布
src/util/       keysym 映射、快照树分层布局、格式化
demo/           fakecomp 模拟合成器 + 演示容器脚本
scripts/        图标生成、e2e 冒烟、证书信任
public/         manifest.json、sw.js、图标
```
