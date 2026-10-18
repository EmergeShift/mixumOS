# gateway:云端枢纽设计(多容器路由、设备配对、令牌分发)

状态:设计定稿(2026-10-18),未实现。实施里程碑见 §9。

## 1. 定位与拓扑

录音愿景"手机发指令 → 云端 agent 开 Chrome → mac 上看到独立原生窗口"
缺一块枢纽:客户端(手机/mac 控制台)在公网,节点(mixumd+mixcomp 容器)
在容器/NAT 后面,互不可达。gateway 是唯一的公网 WSS 入口:

```
手机 PWA ──┐                      ┌── WSS 出站注册 ── mixumd 节点 A(云端容器)
           ├── WSS/TLS ── gateway ┼── WSS 出站注册 ── mixumd 节点 B(家里机器)
mac PWA ───┘                      └── WSS 出站注册 ── mixumd 节点 C(k8s Pod,阶段5)
```

三条设计红线:

1. **节点出站连接**:容器在 NAT 后,一律由 mixumd 主动向 gateway 发起
   WSS 注册,gateway 不反连;
2. **mixumd 协议零破坏**:gateway 只做信封路由,业务语义(task.* /
   snap.* / display.*)原样透传,mixumd 不知道客户端存在;
3. **gateway 不做业务决策**:回退/fork/接管全部发生在节点侧,gateway
   只管「这条信封该去哪」。

## 2. 身份与令牌(签发根上移)

现状:mixumd 自签 ed25519 令牌(`mixum1.*`,claims=sub/caps/iat/exp/jti
/taskId/windowIds)。gateway 成为**令牌签发根**,mixumd 的引导令牌退役:

| 令牌 | sub | caps | 用途 |
|------|-----|------|------|
| 用户令牌 | `user:<id>` | 用户全量能力 | 人持有的主凭据,由 gateway 引导流程签发 |
| 节点令牌 | `node:<name>` | `gateway.route` | mixumd 注册出站连接用,只此一用 |
| 设备令牌 | `device:<id>` | 用户能力子集 | 经设备配对签发给手机/mac,可吊销 |

- 签名算法/前缀不变(ed25519 + `mixum1.*`),gateway 侧验签即可;
- 节点令牌限制 `sub` 前缀,gateway 注册通道只认 `node:*`;节点向
  mixumd 本机 socket 转发信封时仍需 mixumd 本地校验(令牌透传或本地
  引导令牌,v1 用后者:gateway→节点连接视为可信内网,节点侧不二次
  验签,详见 §7 安全边界);
- 吊销:gateway 维护 jti 撤销表(内存,v1;持久化留给 k8s 阶段)。

## 3. 节点注册与保活

新增 gateway op(v1 不入 protocol.md 主表,单独 §gateway):

| op | 方向 | 载荷 | 应答 |
|----|------|------|------|
| `gateway.register` | 节点→gw | `{ name, token(节点令牌), version }` | `{ lease: <秒> }` |
| `gateway.heartbeat` | 节点→gw | `{}` | `{ ok: true }` |

- 节点侧改动(**mixumd 唯一新增**):`--gateway <url>` 参数 + 出站
  relay 模块——把 gateway 当成一条"额外可信客户端连接"接入既有
  Session 分发,不需要理解 gateway 语义;
- 保活:lease 内无 heartbeat 视为离线;指数退避重连(复用 console
  客户端同款策略),避免 k8s 滚动更新时的注册风暴;
- 重名:`gateway.register` 遇到同名在线节点拒绝(旧连接先注销才可
  重入),防止误配的容器顶掉同名生产节点;
- gateway 只保留节点在线表(name → 连接句柄 + 版本 + 最后心跳),
  v1 内存态,不持久化。

## 4. 客户端信封路由

客户端(手机/mac PWA)连 gateway 后,信封加一个可选字段:

```json
{ "v": 1, "id": "...", "op": "task.submit", "node": "home-01", "payload": {...} }
```

- envelope 加 `node: String`(serde default 空串 + skip_serializing,
  纯增量,mixumd 忽略之);
- **无 node 字段**:单节点时路由到唯一在线节点;多节点时返回
  `bad_request`("请指定 node")——不做隐式默认,避免歧义;
- **请求-应答关联**:gateway 维护 `replyTo → (客户端会话, 原节点)`,
  节点应答回填转发;节点离线时未决请求统一回 `internal` 错误;
- **订阅类 op**(task.events / display.windows):订阅与客户端会话
  绑定,事件按来源节点原样转发;
- **二进制帧**(0x01 video):gateway 不解析,按「客户端已附流的
  窗口表 → 来源节点」转发;附流时记下 (client, windowId) → node,
  帧反向寻址;多节点同名窗口 id 冲突在 attach 时即按节点隔离;
- **背压**:沿用节点的每订阅者有界队列语义,gateway 转发队列亦
  有界(64),溢出丢弃保实时——帧流实时性优先原则不变。

## 5. 设备配对

手机/mac 新设备首次使用的凭据引导:

| op | 方向 | 载荷 | 说明 |
|----|------|------|------|
| `gateway.pair.create` | 用户令牌→gw | `{}` | 生成 6 位配对码,5 分钟有效 |
| `gateway.pair.claim` | 无凭据→gw | `{ code, deviceName }` | 换设备令牌 |

- 流程:用户在已信任设备点「添加设备」得配对码 → 新设备输码 →
  gateway 签发 `device:<id>` 令牌(能力 = 用户能力子集,缺省不含
  `admin`/`snap.push`)→ 新设备持久化到 localStorage;
- 设备列表与吊销:`gateway.device.list/revoke`(用户令牌),吊销进
  jti 撤销表;
- v1 配对码单次使用、5 分钟过期;暴力破解由尝试限速(10 次/分钟)
  覆盖。

## 6. 安全边界(诚实清单)

- **gateway 可见明文**:TLS 在 gateway 终结,信封与帧内容对 gateway
  透明可见。前提是自托管——gateway 与节点同属用户信任域。端到端
  加密(客户端与节点协商会话密钥,gateway 纯转发密文)列为 v2 演进;
- **节点侧二次验签缺失**:gateway→节点连接按可信内网对待(节点
  socket 仅监听本机/容器内网)。v2 可给节点配本地验证:gateway 转发
  时附带原始令牌,节点自验;
- 限速:gateway 对未认证连接 50 req/min,认证后复用节点侧限速;
- 审计:配对/吊销/注册事件落结构化日志(v1 stdout)。

## 7. 与 k8s operator 的衔接(阶段 5)

gateway 的节点注册表就是 operator 的对账目标:

- 节点 = MixumTask CRD 的实例,operator 创建 Pod 时以节点令牌注入,
  Pod 起 mixumd `--gateway` 自动注册;
- gateway v1 内存态注册表 → 阶段 5 换 k8s API/etcd 后端(接口预留
  `NodeRegistry` trait,内存实现为默认);
- 快照卷挂载:节点启动时挂预置快照卷,复用既有 snap.pull 语义。

## 8. 客户端改动(console PWA)

- Settings 增加「节点」选择(从 `gateway.node.list` 拉取;单节点
  隐藏选择器);
- 配对入口:「添加设备」(生成配对码)与首启「输码接入」;
- 信封统一带当前选中节点(无选择时不带,单节点场景自动路由);
- 无 gateway 直连 mixumd 的既有用法**保持不变**(协议不识别 node
  字段即忽略)。

## 9. 实施里程碑

- **M1(✅ 2026-10-18 落地)**:gateway 二进制(crate `mixum-gateway`,
  复用 mixum-protocol)+ 节点出站注册 + envelope.node 路由(文本与
  二进制)+ 用户/节点令牌签发(bootstrap 管理令牌打印于启动日志,
  `gateway.token.issue` 签发,`gateway.node.list` 查询);e2e 双节点
  拓扑全链路通过(tests/integration/gateway-e2e.mjs:注册/令牌/任务
  提交/事件流隔离/窗口表隔离/帧按附流路由)。M1 传输为明文 ws(TLS
  于 M2 补);mixumd 新增 `--gateway/--gateway-name/--gateway-token`
  出站 relay(rust/crates/mixum-agentd/src/gateway.rs)。

### 9.1 节点通道私有约定(gateway ↔ mixumd relay,非公有协议)

- gateway 为每条客户端连接分配 `gatewaySession`;relay 按会话开一条
  到本机 mixumd 的回环明文 WS(127.0.0.1,复用
  server.serve_plain_listener)——mixumd 的订阅/广播是会话级的,按
  会话开回环天然隔离多客户端;
- gateway→节点文本:信封 + `gatewaySession` 字段;
  `gateway.session.open/close` 控制会话生命周期(relay 幂等处理);
- 节点→gateway 文本:同上带 `gatewaySession`,gateway 剥离后回送
  客户端;二进制:`u32 LE 会话 id 长度 + 会话 id 字节 + 原始帧`;
- 会话在首次路由到某节点时惰性开启(重复 open 由 relay 幂等忽略),
  客户端断开或节点重连时关闭(客户端按既有语义重连/重订阅)。
- **M2**:设备配对 + 吊销 + gateway.node.list + console 节点选择器
  与配对 UI;
- **M3**(阶段 5 合流):NodeRegistry 换 k8s 后端、多 gateway 副本。

测试计划:新增 tests/integration/gateway-e2e.mjs——两容器栈 +
gateway 进程,断言路由隔离(节点 A 的任务不出现在节点 B 的事件流)、
断线语义(节点掉线 → 未决请求报错、恢复后重订阅)、帧按节点隔离。

## 10. 开放问题

1. 多 gateway 副本下注册表共享(v1 单实例,k8s 阶段解决);
2. 节点令牌泄漏的应急:revoke + 重签发流程(人工,v1);
3. 帧转发的 gateway 内存水位(多客户端多窗口并发)——M1 以单用户
   自托管规模为目标,不做跨用户多租户;
4. 配对码 UI 在手表等窄设备上的形态(后置)。
