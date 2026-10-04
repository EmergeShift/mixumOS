# 目标:mixcomp 运行时调通与窗口流验证

> 你负责 mixumOS 的 C/wlroots 合成器(mixcomp)运行时调通。
> 这是一份自包含任务书:先读本文档,再动手。

## 必读(动手前按序读完)

1. `/Users/ashreo/agent-project/mixumos/docs/architecture.md` —— 全局架构与语言分工
2. `/Users/ashreo/agent-project/mixumos/docs/wayland-magic.md` —— mixcomp 设计与魔改点(你的主规格)
3. `/Users/ashreo/agent-project/mixumos/docs/protocol.md` §5、§4.4 —— 帧格式与接管规则(你必须逐字节对齐)
4. `/Users/ashreo/agent-project/mixumos/mixcomp/README.md`

## 项目背景(30 秒版)

mixumOS 是基于 Arch Linux 的 Agent OS:agent 在容器里跑,操作的 GUI 以
独立原生窗口流到用户设备。mixcomp 是容器内的 Wayland 合成器(魔改点:
不渲染输出、每窗口直接读 client buffer、virtual-pointer/keyboard 协议注入),
它与容器内守护进程 mixumd 经 Unix 套接字通信(5 字节记录头 + JSON/帧)。

## 方向铁律(不得违背)

- mixcomp 保持 **C + wlroots 0.18**,这是既定决策,不要提议改 Rust/Smithay;
- 帧格式与控制通道协议**逐字节对齐** reference 实现,不允许私改协议;
- 中文注释与文档用**全角标点**,文风平实,不写营销腔;
- 只修运行时问题与验收缺口,不做架构重构,不扩新功能(v1 边界见
  architecture.md「非目标」与 wayland-magic.md §7)。

## 你的边界(只准动这些)

- 允许:`mixcomp/`、`tests/integration/`
- 只读:`reference/`(Go 参考实现,协议对齐的事实来源,禁止修改)、`docs/`
- 若协议文档与 reference 实现有出入:以 reference 行为为准,并在
  `goals/notes/mixcomp.md` 记录差异,不要自行改文档或协议。

## 现状(2026-10-04,重要)

- mixcomp **编译已通过**(aarch64,Alpine 3.21 容器内 make);
- **运行时段错误待修**:gdb 已定位——崩溃在注册表事件回调创建虚拟
  pointer 时,栈在 `wl_proxy_marshal_flags`(`input.c` 的
  `maybe_create_devices`);已加调试日志(`创建虚拟输入设备: seat=%p
  vp_mgr=%p kb_mgr=%p`)。怀疑方向:①seat 全局晚于两个 manager 到达的
  时序(guard 已有,需验证是否真的生效);②`wl_proxy_marshal_flags`
  对 new_id 的 version 参数用法;③自定义 wl_interface 表字段有误
  (对照上游 XML 逐字段核对 zwlr_virtual_pointer / zwp_virtual_keyboard);
- 容器 e2e 环境已存在并可用(见下);mixumd(Go 参考实现)+ foot 已能在
  容器里跑,foot 曾报「no compositor」(已修,缺 wlr_compositor_create)
  与「no monitors」(已修,加了 anchor 输出)。

## 任务

1. 修掉段错误,让 mixcomp 在 e2e 容器里稳定运行(≥10 分钟无崩溃,
   期间有窗口注册与心跳)。
2. 打通窗口流:e2eclient 对容器内 mixumd 收到 foot 窗口 ≥3 帧 JPEG。
3. 打通输入注入:通过 mixumd 下发 pointer_motion → 帧头光标坐标变化可观测;
   display.takeover 状态机(agent 拒注入 → force claim → 注入成功 →
   release)全链路走通。
4. 把修复过程与根因写进 `docs/wayland-magic.md` 追加一节「运行时调通记录」,
   并更新 `goals/notes/mixcomp.md` 交付说明。

## 验收标准

- [ ] `tests/integration/Dockerfile.e2e` 镜像构建成功,容器跑满 10 分钟无重启;
- [ ] e2eclient 输出包含「窗口流正常(≥3 帧)」与光标坐标随注入变化;
- [ ] 接管流程按 protocol.md §4.4 语义全部正确(错误码含 conflict);
- [ ] `mixcomp/src` 内全部编译警告清零(-Wall -Wextra);
- [ ] 根因与修复写入文档,后人不再踩。

## 环境备忘

- 宿主 mac(arm64),Docker 走专用 VM:`colima list` 应有 `mixumos`
  (aarch64,已配好代理与 DNS);`docker context use colima-mixumos`;
  拉镜像失败先查 VM 代理(192.168.5.2:7897)。
- 编译容器:`docker run --rm -v "$PWD/mixcomp:/src" -w /src alpine:3.21 sh -c
  "apk add --no-cache make build-base pkgconf wlroots-dev json-c-dev
  libxkbcommon-dev libjpeg-turbo-dev wayland-protocols && make"`;
  注意:`xdg-shell-protocol.h` 需 wayland-scanner 现场生成
  (Dockerfile.e2e 里有现成写法);链接需 `-lwayland-client` 与 `-ljpeg`。
- mixumd 二进制:`cd reference && GOOS=linux GOARCH=arm64 CGO_ENABLED=0
  go build -o ../build/mixumd-arm64 ./cmd/mixumd`(build/ 已 gitignore)。
- e2e:`docker build -f tests/integration/Dockerfile.e2e -t mixumos-e2e .`
  → `docker run -d --name mixum-e2e -p 17601:7601 mixumos-e2e` →
  令牌取日志 READY 行 → `cd reference && go build -o /tmp/e2eclient
  ./cmd/e2eclient` → `/tmp/e2eclient -addr wss://127.0.0.1:17601
  -token <令牌> -insecure`。

## 协作规程

- 开工先:`cd /Users/ashreo/agent-project/mixumos && git worktree add
  ../mixumos-mixcomp -b module/mixcomp`,在 `../mixumos-mixcomp` 里工作,
  避免与其他 agent 冲突;
- 只提交你边界内的路径;提交信息为中文一句话,说清「做了什么、为什么」;
- 完成后:分支上提交齐全 + `goals/notes/mixcomp.md` 写清交付内容、
  根因、遗留问题;不要合并到 main(由用户统一合并);
- 遇到超出边界的阻塞:记录到 notes 并标注 BLOCKER,不要绕开边界硬修。
