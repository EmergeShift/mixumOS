# 目标:mixum-ctl crate(主机侧 CLI 与本地视图)

> 你负责 mixumOS 主机侧工具链的 Rust 实现:连接管理、任务/快照操作、
> 电力钩子、本地窗口视图。这行让用户的 mac/手机能真正用起来。
> 这是一份自包含任务书:先读本文档,再动手。

## 必读(动手前按序读完)

1. `/Users/ashreo/agent-project/mixumos/docs/protocol.md` —— 你是协议客户端(逐字对齐)
2. `/Users/ashreo/agent-project/mixumos/docs/architecture.md` —— 组件总览
3. `/Users/ashreo/agent-project/mixumos/docs/snapshots.md` §7 —— 跨设备流转语义(盒盖迁移链路)
4. Go 参考实现(只读):`reference/cmd/e2eclient/main.go`(WS 客户端参考:
   握手/请求-应答关联/帧接收)、`reference/internal/server/client.go`(远端同步客户端)

## 项目背景(30 秒版)

mixumOS 是基于 Arch Linux 的 Agent OS,容器内是 mixum-agentd(Rust,并行开发中)
或其 Go 参考实现 mixumd(已可用)。mixum-ctl 是用户设备上的 CLI:
连上容器提交指令、看任务事件、操作快照树、把快照推到另一台设备、
在本地浏览器里看 agent 操作的实时窗口。最终形态还包括 macOS 盒盖前
自动打快照上传(「mac 睡了,云端继续跑」的核心体验)。

## 方向铁律(不得违背)

- **Rust + 动态链接**,不做 musl 静态宇宙;协议复用 `rust/crates/mixum-protocol`
  (线上字段名驼峰、能力字符串带点号、帧头 27 字节小端——已锁定);
- `reference/` 只读;
- 中文注释与文档全角标点,平实文风;
- 不扩范围:TUI、多容器管理面板、凭据库(令牌文件按 reference 惯例明文 0600,
  钥匙串集成后置)。

## 你的边界(只准动这些)

- 允许:`rust/crates/mixum-ctl/`(新建)、`rust/Cargo.toml`(增量)、
  `docs/`(只允许追加「host-integration.md」新文件与「Rust 实现记录」节)
- 只读/禁改:`reference/`、`mixcomp/`、`rust/crates/mixum-agentd/`、
  `rust/crates/mixum-snapshot/`

## 现状(2026-10-04)

- `rust/crates/mixum-protocol` 就绪(18 测试全绿,含交叉验证);
- 服务端可用形态:Go 参考实现 mixumd(容器 e2e 通过);
  Rust agentd 由另一 agent 并行开发——**你的验收对象是 reference mixumd**,
  协议兼容所以两者通吃;
- e2e 容器:`tests/integration/Dockerfile.e2e`(mixumd + mixcomp + foot)。

## 任务

1. crate 结构:`transport`(WS + TLS 1.3,证书指纹 TOFU:首次连接打印
   指纹确认后存 ~/.mixum/known-hosts,后续强校验,格式参考
   `rust/crates/mixum-protocol` 的 token 模块与 docs/security.md §3)、
   `cmd`(子命令)、`view`(本地视图)、`power`(电力钩子)。
2. 子命令:
   - `mixum-ctl connect --addr <wss://...> [--fingerprint <…>]`:保存设备配置;
   - `mixum-ctl task submit "指令" [--agent x] [--param k=v]` 并跟踪事件
     (state/log/progress 实时打印,退出码 = 任务结果);
   - `mixum-ctl task list / get <id> / cancel <id>`;
   - `mixum-ctl snap tree / create / restore <node> [--strategy fork] / fork
     / push <node> --remote <addr>`;
   - `mixum-ctl display list / record <window> --out <file>`
     (收 JPEG 帧存序列);
   - `mixum-ctl serve` —— 本地视图服务(见 4)。
3. **电力钩子**(darwin 优先):
   - macOS:IOKit 监听合盖前事件(IOPMSleepInterestCallback 或
     io_power_message 驱动方案,选可无权限实现的,写明依据);
     触发:`snap create hybrid` → `snap push --remote <云端>`;
   - 配置:`~/.mixum/config.json`(远端地址、令牌路径、策略);
   - linux:systemd sleep.target hook 说明文档即可(不做实现);
   - 本地无法真合盖验证:提供 `--simulate-sleep` 手动触发路径,
     用两个容器节点(本地 + 第二容器当"云端")做全链路演示。
4. **本地视图服务**(`serve`):起本地 HTTP(默认 127.0.0.1:7602),
   浏览器打开即看到窗口列表;点开窗口后服务端经 WS 收帧,
   页面用 canvas 绘制 JPEG + 按帧头光标元数据叠加光标
   (协议 §5);页面为单文件内嵌 HTML/JS,不依赖 npm 构建。
5. 双平台编译检查:`cargo check --target aarch64-apple-darwin` 与
   `--target x86_64-unknown-linux-gnu`(linux 目标只需 check 通过,
   电力钩子 darwin 特性用 cfg 隔离)。

## 验收标准

- [ ] `cargo test -p mixum-ctl` 全绿(transport 的握手/应答关联/令牌校验、
      帧解析、TOFU 存取),clippy `-D warnings` 干净;
- [ ] 对 e2e 容器(reference mixumd)实测:`task submit` 提交并跟踪到
      completed;`snap tree` 输出与 e2eclient 一致;
      `snap restore` 后容器内文件状态变化可验证;
- [ ] 跨节点演示:两个容器互为远近端,`snap push` 第二次零传输(去重生效);
- [ ] `serve` 在浏览器显示窗口列表与实时画面(若 mixcomp 未就绪,
      用 broker 的模拟窗口路径演示帧管道:自行造 2-3 帧静态图;
      mixcomp 就绪后补真窗口,在 notes 记录状态);
- [ ] 电力钩子 `--simulate-sleep` 全链路演示(快照 + 推送);
- [ ] `docs/host-integration.md`:合盖迁移链路、平台差异、安全注意
      (全角标点);`goals/notes/ctl.md` 交付说明。

## 环境备忘

- 宿主 mac(arm64),Rust 1.97;cargo 直跑即可;
- e2e 容器:`colima list` 确认 `mixumos` VM 在跑;`docker build -f
  tests/integration/Dockerfile.e2e -t mixumos-e2e .`;令牌取 READY 行;
  第二"云端"节点:`docker run -d --name mixum-cloud -p 17602:7601 mixumos-e2e`;
- Docker 代理已配;拉镜像失败查 192.168.5.2:7897。

## 协作规程

- 开工先:`cd /Users/ashreo/agent-project/mixumos && git worktree add
  ../mixumos-ctl -b module/ctl`,在 `../mixumos-ctl` 里工作;
- 只提交你边界内的路径;提交信息中文一句话;
- 完成后:分支提交齐全 + notes 交付说明;不要合并 main;
- 遇阻:记录 notes 标注 BLOCKER。
