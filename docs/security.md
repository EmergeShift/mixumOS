# mixumOS 安全设计

> 原则:攻击面收敛到「一条 TLS 通道 + 一组能力令牌 + 一个 Unix 套接字」;
> 密码学原语零第三方依赖(标准库/系统库);所有安全决策可被一个目录审计。

## 1. 威胁模型(v1)

| 资产 | 敌手 | 防线 |
|------|------|------|
| 任务数据/工作区 | 网络窃听者 | TLS 1.3 强制;块传输明文但哈希校验 + 端到端 TLS |
| 控制面(任务/快照/接管) | 未授权连接 | 能力令牌(ed25519 签名)+ 最小能力授予 |
| 窗口内容 | 越权观看 | display.view 能力 + 可选窗口 id 白名单(claims.windows) |
| 输入控制 | 越权接管 | display.input 能力 + broker 接管状态机(agent 模式硬拒绝) |
| 静态数据(快照仓库) | 磁盘窃取 | AES-256-GCM 块级加密(可配),masterKey 0600 |
| 容器内提权 → 主机 | 恶意指令 | 容器无特权(默认);工作区与快照存储独立卷;CRIU 特权变体单独标记 |
| 主机端伪装容器 | 恶意 mixumd | 证书指纹 TOFU 固定;指纹变化即断连报警 |

明确不防(记录在案):内核漏洞逃逸(交给容器运行时与宿主加固)、
物理攻击、内存快照中的敏感数据驻留(CRIU 镜像加密在阶段 5 列入)。

## 2. 身份与令牌

- 服务端身份 = ed25519 种子(`dataDir/identity.key`,0600,首启生成)+ 自签证书;
- 能力令牌:`map1.<payload>.<sig>`(旧 `mixum1.` 过渡期仍接受),claims 见 protocol.md §3;
- 授予原则:控制台/手机令牌只给 task.run+observe+display.view+input;
  快照同步令牌单独签发、单独 TTL;
- jti 预留吊销列表;长期令牌跨连接复用是设计决策(TLS 防传输重放),
  若需一次性令牌,签发时 exp 掐到分钟级即可;
- bootstrap 管理令牌首启生成落盘(0600),运维通过它派生业务令牌。

## 3. 传输

- TLS 1.3 only(ECDSA P-256 自签);明文 WS 仅存在于测试(代码内显式标注);
- 客户端固定指纹:首次连接显示指纹,确认后固化;`auth.PinVerifier`
  在握手层校验,不匹配即断;
- WS 帧上限:控制 32 MiB / 二进制记录 16 MiB;连接级令牌桶限速;
  慢消费者(帧通道持续满)2 秒后断连,防止拖垮 broker。

### 3.1 gateway 多副本(M3)

- 共享注册表落 k8s Lease,只含节点元数据(name/owner mesh 地址/version),
  不含节点令牌、任务内容或帧数据;
- 副本间 mesh 是集群内明文 WS + Secret 注入的共享 token,只监听
  ClusterIP/headless Service;跨集群或公网部署需自行加 TLS/隧道
  (v1 不做副本间端到端加密,记录在案);
- 副本必须共享 `identity.key`(Secret 挂载)作为令牌签发根,否则 A 签的
  令牌在 B 上验不过;TLS 证书同理共享时才能稳定使用 wss;
- mesh 会话 id(`ms-*`)只下发给节点 relay 作回环标记,节点无法据此跨副本
  提权;节点令牌仍按 `node:*` 前缀在 owner 副本本地校验;
- **M3 多副本不共享** `revoked`/`devices`/`pairs`/`bootstrap_jti`:这些
  状态在每副本 `--data-dir` 内独立。跨副本吊销、配对码 claim 和 bootstrap
  轮换因此不保证一致;M3 的 Service 用 ClientIP affinity 降低跨 Pod 概率,
  但这不是安全边界。需要跨副本吊销/配对时,必须把鉴权状态放共享后端
  (阶段 4),或只通过单副本管理入口执行。

## 4. 快照子系统

- 块哈希 = 明文 SHA-256,读取时全量校验(防静默损坏与块挪用);
- GCM AAD 绑定 storeID + 块哈希(防跨仓库重放块);
- 远端导入路径:清单路径拒绝 `..`/绝对路径;导入节点父不存在时
  落为远端根(origin 标记),不产生跨树悬空引用;
- 推送能力(snap.sync)与快照管理能力分离,推送对端只收不管理。

## 5. 容器内

- mixumd/agentd 不以 root 运行(v2 目标;v1 因 overlayfs 挂载检测
  允许 root,发行版单元用 DynamicUser 收敛,见 distro.md);
- mixcomp 控制套接字 `/run/mixum/display.sock`:0700 目录,
  仅容器内进程可达;窗口流不经过任何网络端口直达主机
  (帧走 mixumd 的 WSS,鉴权能力生效);
- external 驱动拉起的 agent 进程:独立进程组、环境最小化
  (MIXUM_INSTRUCTION/TASK_ID/AGENT_BRIDGE),不继承 shell 历史;
- 快照冻结优先用 **cgroup v2 freezer**（`cgroup.freeze=1`，原子覆盖整棵
  进程树，含脱离进程组的孙进程）；内核/委托不允许时退回进程组 SIGSTOP；
- 文件系统写入白名单：Linux 5.13+ 在 `pre_exec` 里套 **Landlock**，
  任务只能写工作区、`/tmp`、`/dev`、`/run`，以及 HOME 下预建的工具目录
  （`.cache/.local/.config/.npm/.cargo/.rustup/.mixum`，可用
  `MIXUM_LANDLOCK_EXTRA_WRITABLE` 追加），其余只读；内核未启用
  Landlock 时退回容器边界（非特权 + 独立工作区卷）。高危命令的语义
  拦截在 MixumART 侧做，OS 层只负责兜底回滚与资源边界。

## 6. 演进位(阶段标记)

- [阶段 4] 令牌吊销列表(jti → 嵌入式 KV(与快照树同用 sled)或 redis);
- [阶段 4] H.264 路径的密钥帧加密(会话级密钥,握手协商);
- [阶段 5] CRIU 镜像加密与完整性;
- [阶段 5] gateway 多租户:设备配对二维码 → 短期令牌换长期令牌。
