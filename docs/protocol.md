# mixumOS 协议规范 v1

> 状态:与 mixum-agentd(Rust)实现一致;Go 参考实现已移除(2026-10-16),原为规格来源。
> 任何不兼容变更必须递增主版本(envelope.v)并在本文档记录。

## 1. 传输与安全

- 唯一传输:WebSocket over TLS 1.3(`wss://host:port/ws`)。
  服务器强制 TLS 1.3;明文 WebSocket 仅存在于测试代码。
- 证书:自签(ECDSA P-256,10 年)。客户端首次连接记录证书
  SHA-256 指纹(TOFU),此后强校验;指纹不匹配即拒绝(疑似中间人)。
- 认证:握手时携带能力令牌(§3)。未握手只允许 `session.hello`(别名 `hello`)、
  `session.resume` 与 `sys.ping`;`sys.info` 在握手前只回 `profiles`/`server`,
  其余操作一律 `unauthorized`。
- 子协议:客户端应请求 `map.v1`;过渡期服务端同时接受 `mixum.v1`(两者同时
  出现时回 `map.v1`),应答头回显选中的子协议。
- 消息大小:控制消息上限 32 MiB,二进制记录上限 16 MiB。
- 限速:每连接令牌桶,50 条/秒,突发 100;超限回 `session.err/rate_limited`。

## 2. 帧

- **文本帧** = 控制消息(envelope JSON)。
- **二进制帧** = 流数据,首字节为通道标签:

| 值 | 通道 | 负载 |
|----|------|------|
| 0x01 | video | 视频帧(§5) |
| 0x02 | audio | 预留 |
| 0x03 | chunk | 快照块(§6) |
| 0x04 | stdio | 任务 stdio 字节流(预留) |

## 3. 能力令牌

格式:`map1.<base64url(payload)>.<baseurl64(sig)>`,ed25519 签名,
密钥即服务端身份密钥(首次启动生成,落 `dataDir/identity.key`)。
旧前缀 `mixum1.` 在过渡期继续被验证端接受,但签发端只发 `map1.`。

```json
{
  "sub": "iphone-15",              // 主体
  "caps": ["task.run", "display.view"],
  "iat": 1696400000, "exp": 1696403600,
  "jti": "01A…",                    // 预留给吊销列表;v1 不做单次使用
  "taskId": "01J…",                 // 可选:限定可操作任务(ULID)
  "windows": [7, 9]                 // 可选:限定可观看/接管窗口;缺省全量
}
```

能力集合(令牌只授最小集):

| 能力 | 含义 |
|------|------|
| task.run | 提交/取消任务 |
| task.observe | 订阅任务事件、查询任务/快照树/系统信息、答复人机输入 |
| snap.manage | 快照创建/恢复/fork/删除/GC |
| snap.sync | 作为推送对端收块/落树 |
| display.view | 列窗口、附流 |
| display.input | 注入输入、接管 |
| mem.read | 记忆 providers/search/get/list/export |
| mem.write | 记忆 put/delete/import |
| vm.manage | 保留(节点尚未实现 vm.* 时回 `unsupported`) |
| admin | 全部能力 |

规则:`admin` 覆盖一切;令牌绑定了 `taskId` 时禁止提交新任务;
绑定了 `windows` 时窗口操作逐一校验。长期令牌允许跨连接复用
(传输重放由 TLS 1.3 防护;jti 留给将来的吊销列表)。

## 4. 控制消息(envelope)

```json
{
  "v": 1,                      // 协议主版本
  "id": "01A…",                // ULID(26 字符 Crockford base32)
  "op": "task.submit",
  "seq": 42,                   // 推送方向每会话单调;回复可省略
  "replyTo": "01A…",           // 回复时指向请求 id
  "ts": 1696400000123,         // unix 毫秒
  "payload": { … }
}
```

回复 op = 请求 op + `.ok` / `.err`。`.err` 负载:

```json
{ "code": "forbidden", "message": "令牌缺少能力: display.input" }
```

错误码(稳定,客户端程序化处理):`bad_request` `unauthorized` `forbidden`
`not_found` `conflict` `already_exists` `replay_detected` `busy` `internal`
`unsupported` `rate_limited` `timeout` `unavailable` `snapshot_invalid`。

### 4.1 握手与会话恢复

`session.hello` 请求(旧名 `hello` 为过渡别名,应答随之回 `hello.ok`):
`{ "token": "map1.…", "client": "mixumctl/0.1", "device": "iphone-15",
"profiles": ["core","display","memory"] }`

`session.hello.ok`:`{ "sessionId", "server": "mixum-agentd/0.1.0", "arch",
"kernel", "caps": [...], "profiles": ["core","display","memory"],
"versions": [1], "heartbeat": 30 }`

`session.resume`:`{ "sessionId", "lastSeq" }` →
`{ "resumed": true, "replayed": N, sessionId, server, arch, kernel, caps,
profiles, versions, heartbeat }`。服务端每会话保留 ≥256 条推送 envelope
的环形缓冲,重放 `seq > lastSeq` 的消息后才发新推送;订阅状态与 claims
随会话恢复,客户端无需重新 hello/`task.events`。未知或过期会话回
`not_found`,客户端应退回完整握手。

### 4.2 任务

| op | 请求负载 | 应答负载 |
|----|----------|----------|
| task.submit | `{ instruction, agent?, runner?, params?, snapshotPolicy?{pre,kind,onFailure} }` | Task 对象(含 preSnapNode) |
| task.list | — | `{ tasks: [Task] }` |
| task.get | `{ id }` | Task |
| task.cancel | `{ id }` | `{ id }` |
| task.events | — | `{ subscribed: true }`(此后服务端推送 task.event) |
| task.reply | `{ taskId, replyId, value }` | `{ delivered: true }`(回答 agent 的 input.request) |

Task:`{ id, instruction, agent, runner, state, preSnapNode?, params?, error?, createdAt, startedAt?, endedAt? }`
`id` 为 ULID;`params` 是任意 JSON 对象。
状态机:`queued → running → suspended → completed | failed | canceled`。

`task.submit` 的应答在 pre-task 快照完成后才发出(MAP 语义);
快照策略 `{ pre: always|never, kind: disk|memory|hybrid, onFailure: rollback|keep }`,
过渡期仍接受旧布尔 `pre`。

task.event 负载:`{ taskId, type, step?, data, ts }`,`data` 恒为对象:

| type | data |
|------|------|
| state | `{ state }` |
| log | `{ line }` |
| step | `{ name }` |
| progress | `{ pct, step? }` |
| error | `{ code?, message }` |
| window | Window 对象(客户端可据此自动附流) |
| input | `{ replyId, prompt, choices?, sensitive? }` |

人机输入闭环:agent 经桥发 `input.request` → 客户端收到 `task.event/input`
→ `task.reply { taskId, replyId, value }` → runner 向 agent 写
`input.response`。同一 `replyId` 只能答复一次(第二次 `conflict`),
超过 `timeoutMs`(默认 120 s)未答时回 `input.response { value:"", error:"timeout" }`。

### 4.3 快照

| op | 请求 | 应答 |
|----|------|------|
| snap.create | `{ kind: disk\|memory\|hybrid, label?, parent?, branch?, auto? }` | SnapNode |
| snap.tree | — | `{ head, branch, nodes: [SnapNode] }` |
| snap.restore | `{ node, strategy: inplace\|fork, label? }` | `{ restoredTo, newBranch?, newNode? }` |
| snap.fork | `{ node, branch }` | `{ branch }` |
| snap.delete | `{ node }` | `{ deleted }`(仅叶子可删) |
| snap.gc | — | `{ removed }` |
| snap.push | `{ node, remote, token? }` | `{ node, chunksSent, bytesSent, dedupRatio }` |

SnapNode:`{ id, parent?, branch, label?, kind, auto?, createdAt, origin?,
sizeBytes, chunks, kernel?, arch? }`。`id` 为 ULID;树语义见 snapshots.md §4。

### 4.4 显示

| op | 请求 | 应答 |
|----|------|------|
| display.list | — | `{ windows: [Window] }` |
| display.attach | `{ window, fps?, codec?: "jpeg"\|"h264"\|"png", quality?: 1-100 }` | `{ window, codec, width, height, fps }`(codec = 实际编码,此后二进制 video 帧) |
| display.detach | `{ window }` | `{ window }` |
| display.input | `{ window, type, … }` | `{ ok: true }` |
| display.takeover | `{ window, action: claim\|release, force? }` | `{ mode }` |
| display.windows | — | `{ subscribed }`(此后推送窗口增删/改名) |

Window:`{ id, appId, title, width, height, focused, mode: agent|user|free,
codec? }`。codec = 窗口流的实际编码,由 mixcomp 能力回告(如请求 h264
但编码器缺失时回 jpeg),broker 经 window 事件广播更新。

输入事件(type 字段):

| type | 字段 | 说明 |
|------|------|------|
| pointer_motion | x, y | 窗口内坐标(物理像素) |
| pointer_button | button: left\|right\|middle, action: press\|release\|click | |
| pointer_axis | action: scroll_up\|scroll_down | |
| key | key(evdev 码)或 keysym(XKB 名), action, mods?: [ctrl\|alt\|shift\|logo] | keysym 名如 `"a"`、`"Return"` |

接管规则:窗口 `mode=agent` 时注入一律拒绝(`conflict`);
`claim` 成功进入 `user`,agent 侧收到让出事件;`force=true` 抢占;
`release` 回到 `free`。agent 模式由任务引擎在任务开始/结束时切换。

### 4.5 系统

`sys.ping` → `{ pong: <请求 id> }`;`sys.info` → `{ server, arch, kernel, os,
hostname, caps, profiles, snapshotBackend, criuAvailable, compositor, windows,
memoryProviders }`。`profiles` 当前为 `core`/`display`/`memory`。

### 4.6 记忆(MAP-Memory)

节点本地记忆库位于 `dataDir/memory`,只实现 `builtin` provider;
记录 schema 与 MAP `memory-record.schema.json` 一致(`opaque` 与未知字段
原样保留,保证导入无损)。

| op | 请求 | 应答 | 能力 |
|----|------|------|------|
| mem.providers | — | `{ providers: [{ id, kind, status, capabilities }] }` | mem.read |
| mem.search | `{ query, kinds?, providers?, scope?, limit? }` | `{ hits: [{ record, score }] }` | mem.read |
| mem.get | `{ id }` | Record | mem.read |
| mem.list | `{ provider?, kind?, cursor?, limit? }` | `{ records, nextCursor? }` | mem.read |
| mem.export | `{ provider?, format: "map-json" }` | `{ records }` | mem.read |
| mem.put | `{ record }` | Record(服务端补 id/createdAt/updatedAt) | mem.write |
| mem.delete | `{ id }` | `{ deleted: true }` | mem.write |
| mem.import | `{ provider, format: "map-json", records }` | `{ imported, skipped }` | mem.write |

搜索 v1 为子串匹配,`score` 归一化到 0–1;排序实现自定义(规范允许)。

## 5. video 帧格式(小端)

```
[0]      0x01          通道标签(已由二进制帧前缀携带,重复为校验)
[1:5]    windowID u32
[5:9]    width u32
[9:13]   height u32
[13:17]  size u32      编码数据长度
[17:19]  flags u16     bit0 关键帧, bit1 携带光标
[19:21]  cursorX i16   相对窗口左上角,物理像素
[21:23]  cursorY i16
[23:25]  cursorW u16   光标绘制区尺寸
[25:27]  cursorH u16
[27:]    编码帧(v1: JPEG,quality 由 mixcomp --quality 控制)
```

设计说明:v1 全帧 JPEG(每帧即关键帧),主机端 `<canvas>` 直接绘制,
光标由帧头元数据在画布上叠加绘制——光标永远与帧内容同源,不依赖
主机侧指针位置推算。

**编码协商(2026-10-17 起)**:`display.attach` 以 `codec` 参数协商,
JPEG 路径字节级不变;`codec=h264` 时负载为 H.264 Annex-B,帧头格式
原样复用(flags bit0 = IDR,含周期 IDR 与 attach 即时 IDR),主机端经
WebCodecs 解码。mixumd↔mixcomp 私有通道以记录类型区分
(0x02=JPEG/0x03=H.264),对公有协议不可见;编码器缺失由 mixcomp 经
控制消息回告,broker 统一切换并随 window 事件广播。

**无损 damage 流(2026-10-19 起,实验)**:`display.attach` 可协商
`codec:"png"`。首帧/尺寸变化发全量 PNG;之后只发脏区包围盒,payload 在
27 字节 video 头后追加 20 字节 patch 头(`x,y,w,h,format`,小端):
`format=2` 为 PNG 补丁,`format=0` 仅更新光标;无变化不发帧。帧头 `flags`
bit2 标记补丁。宿主按补丁合成,不能清屏或改变画布尺寸。`quality` 仅对
jpeg 生效。GPU/dmabuf 零拷贝不在本实现内,等 Apple Container/VM 的
virtio-gpu/venus 通路成熟后再接。

**私有通道前向兼容(2026-10-19 起)**:mixumd↔mixcomp 记录类型按增量演进,
接收端必须忽略未知记录类型(计数 + 告警)而不是断开连接;帧长/负载校验
失败是独立的显式错误路径,可以终止控制通道。broker 暴露
`unknown_records` / `protocol_errors` 两个计数便于观测(EmergeShift#8)。

## 6. 快照块传输(跨节点)

推送方向:A 节点把某 SnapNode 推给 B 节点。

1. A→B `snap.push.import`:`{ node: {id,parent,branch,label,kind,createdAt,
   kernel,arch,origin}, manifest, chunks: [hash…] }`;
2. B 逐哈希查自有 CAS,回 `{ missing: [hash…] }`;
3. A 对 missing 逐块发二进制帧:`[0x03][32B 原始 sha256][数据]`
   (数据为明文,压缩加密由 B 端 Store 落盘时处理——哈希永远是明文哈希);
4. A→B `snap.push.commit`:`{ count }`;B 校验 missing 全部到齐后落树;
   父节点不在 B 的树中时,节点成为新分支根,label 追加「(远端根)」。

拉取 = 反向调用对方 push。B 端要求令牌含 `snap.sync`。

## 7. 版本策略

- `envelope.v` 不匹配立即拒绝握手后的所有消息;
- 新增 op、新增可选字段、新增能力值:次版本,向后兼容;
- 修改语义、删除字段、变更二进制格式:主版本,双方协商
  (hello.versions 数组,取交集)。
