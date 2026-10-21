# 剪贴板同步（A 路线）设计草案

状态：草案，待上游评审。本文只描述协议与实现边界，不包含代码改动。
参考实现位于 fork（`1179e72`，sync 后恢复于 `475da2c`），e2e 见
`tests/integration/clipboard-e2e.mjs`。

## 1. 背景

上游目前有一个只读的 `display.clipboard` 请求：agentd 以 2s 超时执行
`wl-paste`，固定连接 B compositor（`XDG_RUNTIME_DIR=/run/mixum`、
`WAYLAND_DISPLAY=mixum-art-0`），返回当前 selection。它解决了 MixumART
（B 路线）的"拉取一次"需求，但作为通用剪贴板能力有四个缺口：

- 只覆盖 B compositor；mixcomp（A 路线）的窗口无法使用；
- 只有拉取，没有写入，console 无法把文本粘贴进容器；
- 没有变化推送，每次复制都要轮询；
- 不绑定窗口，无法按 owner 路由，也无法套用窗口 ACL 与 agent 模式门控。

本文定义 **A 路线**：由窗口所属 comp 原生管理 seat selection，agentd 只做
协议路由与权限判定。B 路线继续保留其拉取请求，作为兼容路径。

## 2. 目标与非目标

目标：

1. console ↔ 容器 GUI 双向文本剪贴板，按窗口路由；
2. 权限、窗口 ACL、agent 模式门控与输入注入同规；
3. 多 comp（mixcomp 与 B compositor 并存）下不串窗口、不串 comp；
4. 纯增量协议，旧客户端与旧 comp 不受影响；
5. 可在 CI 的容器 e2e 中验证（`wl-copy`/`wl-paste` 作为真实客户端）。

非目标（v1）：

- 富 MIME（图片、文件、`text/html`）；
- 剪贴板历史、持久化与跨节点同步（gateway 只是透传，同步域是节点内）；
- 多 seat / 多用户并发策略；
- 浏览器剪贴板权限失败时的降级 UI（由 console 处理）。

## 3. 路线比较

| | A：comp 原生（推荐） | B：agentd 代理（现状） |
|---|---|---|
| 选区归属 | 各 comp 的 seat，窗口语义天然正确 | agentd 作为独立 Wayland 客户端 |
| 覆盖范围 | 每个 comp 可各自实现 | 固定 B compositor socket |
| 写入 | `wlr_data_source` / Smithay selection | 需再拉一个 `wl-copy` 进程 |
| 变化通知 | comp 直接上报 | 只能轮询 |
| 路由/权限 | 按窗口 owner 与 ACL | 无窗口上下文 |
| 依赖 | comp 内实现 | agentd 依赖 `wl-clipboard` |

决策：新增 A 路线；B 的 `display.clipboard` 拉取语义保持不变，不扩展。

## 4. 协议增量

### 4.1 下行写入

```json
{"op":"display.clipboard.set","payload":{"window":3,"text":"hello"}}
```

应答 `{ "ok": true }`。规则：

- 需要 `display.input` 能力，且令牌的窗口 ACL 覆盖 `window`；
- 窗口处于 `agent` 模式时拒绝（`conflict`），与 `display.input` 同规；
- `text` 必须是 UTF-8，长度 1..=4 MiB；空串与超限返回 `bad_request`；
- 窗口不存在 `not_found`；comp 不支持 A 路线时 `conflict`；
- 成功仅表示文本已送入 owner comp 的控制通道，不保证应用已读取。

### 4.2 上行变化事件

```json
{"op":"display.clipboard.changed","payload":{"window":3,"text":"hello"}}
```

推送给已订阅 `display.windows` 的会话（该订阅已要求 `display.view`）。
`window` 用于客户端做窗口级同步与回声抑制；不携带 comp id（客户端不感知）。

命名说明：上游已有同名的 `display.clipboard` 请求，若复用它作为推送事件，
同一 op 会同时表示"拉取请求"和"异步事件"，客户端只能靠 `id`/`replyTo`
区分，容易误判。本草案建议事件使用 `display.clipboard.changed`。
（fork 参考实现目前复用了 `display.clipboard`，合并前需二选一。）

### 4.3 兼容路径

`display.clipboard`（无参数，返回 `{text}`）保留，语义明确为"B compositor
只读拉取"，不做窗口路由。A 路线可用时客户端应优先使用窗口级操作；两者
互不影响。

## 5. 数据流

下行（console → 容器应用）：

1. console 在 focus/paste 时读取浏览器剪贴板；
2. 发 `display.clipboard.set`，gateway 按 `envelope.node` 透传到目标节点；
3. agentd 校验能力/ACL/mode，按窗口 owner 找到 comp 控制通道；
4. 向 comp 发送私有消息 `{"type":"clipboard.set","id":3,"text":"..."}`；
5. comp 创建内建 data source，设为 seat selection，声明
   `text/plain;charset=utf-8` 与 `text/plain`；容器应用直接粘贴。

上行（容器应用 → console）：

1. 应用复制触发 `request_set_selection`，comp 的 data-control 路径接受；
2. comp 读取数据管道直到 EOF（上限 4 MiB，建议加 2s 读超时）；
3. comp 上报 `{"type":"clipboard","id":3,"text":"..."}`；
4. broker 校验该窗口 owner 与消息来源 comp 一致，广播事件；
5. 订阅 `display.windows` 的会话收到 `display.clipboard.changed`；
6. console 写入浏览器剪贴板；用"最近一轮文本"做回声抑制，避免
   console→容器→console 的无限往返。

## 6. 多 comp owner 路由

- 窗口表以 `window.id` 为键，记录 `owner`（comp 连接 id），沿用现有
  `stream.start`/`display.input`/视频帧的 owner 规则；
- 下行只发 owner；owner 断连时窗口被清理，无需保留选区；
- 上行 `clipboard` 消息必须校验 owner，非 owner 上报丢弃并计入
  `protocol_errors`（**参考实现当前缺少这一校验，落地时必须补**）；
- 前提是 `window.id` 在 broker 内唯一；首占者胜，另一 comp 的同 id 更新
  被忽略。该假设已存在于现有窗口表，本文不改变；
- comp 能力：建议 comp 连接后上报
  `{"type":"capabilities","clipboard":true}`（agentd↔comp 私有协议，不进 MAP）。
  broker 记录该标记；未声明能力的 comp 收到 `clipboard.set` 时回
  `conflict`，避免"应答成功但无效果"。该握手是新增项，需在评审中确认是否
  接受；若不接受，替代方案是乐观投递 + 文档声明。

## 7. 实现约束（wlroots 0.18 / mixcomp）

以下为参考实现踩过的坑，B compositor 实现同语义时同样适用：

1. `wlr_seat_set_selection` 内部会销毁旧 data source（发 destroy 信号并释放
   mime）；comp 不能再手动销毁，只能在 `seat.events.set_selection` 回调里
   同步指针；
2. `wlr_data_source_init` 会整体重置 `base`（含 `mime_types`），MIME 必须
   在 init 之后添加；
3. 下行 serial 必须使用 `wl_display_next_serial(display)`。自增小计数超过
   display serial 后，客户端后续 `set_selection` 会因无符号下溢被 wlroots
   当作过期请求静默丢弃，表现为"第一次成功之后永远失败"；
4. 必须创建 `wlr_data_control_manager_v1`，否则 `wl-copy`/`wl-paste`
   以及应用间复制没有挂接点；
5. 上行读取走 event loop 非阻塞 fd；EOF 才算完成，4 MiB 截断并丢弃；
   建议加 2s 读超时，防止 source 持有者不写导致 fd 泄漏；
6. 单选区假设：一个 comp 一个 seat；多 seat 留待后续。

## 8. 安全与隐私

- `set` 复用 `display.input`，不新增能力位；事件仅发给已通过
  `display.view` 的 `display.windows` 订阅者；
- 窗口 ACL 与 agent 模式门控先于投递；被拒绝的请求不触发 comp 副作用；
- 双向 4 MiB 上限，按字节计（UTF-8），超限在协议层拒绝/截断；
- 服务端与 comp 日志只记录长度与结果，不记录内容；
- gateway 不解析 payload，仅按 `node` 路由；设备令牌默认无 `snap.manage`/
  `admin`，剪贴板不扩大其权限。

## 9. 测试计划

单元（agentd）：

- 下行按 owner 路由；owner 断连返回 `conflict`；
- 非 owner 的 `clipboard` 上报被丢弃；
- agent 模式拒绝、ACL 拒绝、缺失能力拒绝；
- 4 MiB 边界与空串语义；
- 无 `capabilities.clipboard` 的 comp 返回 `conflict`。

e2e（容器内真实 Wayland 客户端）：

- 下行：console 侧发送后，容器内 `wl-paste` 读回同一文本；
- 上行：容器内 `wl-copy` 后，事件在 1s 内到达且文本一致；
- 归属：用户接管（user 模式）后仍可写入；agent 持有窗口时写入被拒；
- 多 comp：A/B 各开窗口，交叉写入不串窗（需要 B compositor 侧实现）；
- 回归：damage 门控、H.264 协商、`display.windows` 订阅不受影响。

浏览器：console 焦点/paste 反向同步的端到端用例，验证回声抑制只回传一次。

## 10. 落地拆分

建议单个上游 PR 内按 commit 拆分：

1. `mixum-protocol`：新增 op 常量与文档（本文件 + `protocol.md` §4.4）；
2. `mixum-agentd`：broker owner 路由、上行业主校验、server 权限与事件推送、
   单测；
3. `mixcomp`：`clipboard.c`、`stream.c` 分发、data-control manager、
   Makefile 依赖（`wlroots` 已提供）；
4. `console`：`clipboardSet`、事件监听与回声抑制；
5. `tests/integration/clipboard-e2e.mjs` 与文档回写。

参考实现：fork `1179e72`（原始）、`475da2c`（sync 后恢复 A 路线）、
`tests/integration/clipboard-e2e.mjs`。

## 11. 未决问题

1. 上行事件 op 名：`display.clipboard.changed`（本文建议）还是复用
   `display.clipboard`（fork 现状）？
2. comp 能力握手是否接受？若不接受，缺失 A 能力时如何避免"假成功"？
3. 空串语义：拒绝（本文建议）还是清空 selection？
4. B compositor 的 A 路线实现排期；在此之前其窗口的 `set` 应明确返回
   `conflict`。
5. 是否在 v2 扩展 `text/html`/图片，以及是否需要内容类型协商。
