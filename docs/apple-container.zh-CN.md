# Apple Container 运行时调通记录

> 日期:2026-10-18。记录 mixumOS 在 macOS 宿主 Apple Container 上的完整实测:
> 安装/启动/共享模型/CRIU 可用性/MAP 全链路握手/双节点整树迁移,以及过程中
> 发现并修复的 sync 0 块节点缺陷。数字均为本机实测,复现命令随文给出。

## 1. 测试机环境

| 项 | 值 |
|----|----|
| 机型/芯片 | Apple M1(arm64),8 核,16 GB |
| 宿主系统 | macOS 26.6(25G72) |
| 容器运行时 | Apple `container` CLI 1.5.0(brew;非 Docker Desktop) |
| guest 内核 | kata-containers 3.32.0 → vmlinux 6.18.35(-197-debug,30 MB) |
| 被测镜像 | mixumos 0.1.0 aarch64 OCI(2026-10-04 构建;Arch rootfs + mixumd + mixcomp) |
| 宿主侧客户端 | @mixum/map TS SDK(tsx 直跑源码) |
| 参考对照 | Docker 不可用;dist 重打在 Windows/WSL2 机(见 distro.md) |

## 2. 安装与启动

```bash
brew install container
container system kernel set --recommended   # 拉取 kata 内核
container system start                      # 首次需交互确认装内核,脚本可 echo Y |
```

启动耗时(容器内执行 `true` 端到端):

| 场景 | 耗时 |
|------|------|
| 冷启动(首次建基础设施) | 26.4 s |
| 热启动 ×3 | **1.00 / 1.07 / 1.00 s** |

Apple 官方「<1 s」宣称在 M1 上基本成立。machine 形态首次启动存在
「Operation not supported by device」瞬态报错,重试即好,不阻塞。

## 3. 共享模型实测(多开成本)

**磁盘**:内核 29 MB 全局一份;镜像 blob(492 MB)每镜像一份;同镜像多开
容器走 APFS clonefile 写时复制——`df` 实测新起一个 alpine 容器**物理净增
0 MB**。注意 `du` 显示的每容器 1.3 GB 是克隆重复计数假象(`st_blocks` 按满额
计入),磁盘真相要看 `df`。容器目录内 `initfs.ext4`(640 MB 标称/180 MB 实际
分配)与 rootfs 均为克隆。

**内存**(container stats 实测):

| 容器 | 实际用量 | 配额 |
|------|---------|------|
| mixumOS 节点(agentd + mixcomp 全开) | **37.7 MiB** | 1 GiB |
| 第二个 mixumOS 实例(裸跑) | 35.6 MiB | 1 GiB |
| alpine machine 空转 | 5.6 MiB | 8 GiB |

配额是天花板不是预留(宿主按用量惰性提交);rootfs 走 demand paging,
764 MB 解包镜像裸跑仅 35 MB。**结论:同基座多开 ≈ 内核/镜像/块全部共享,
每实例私有成本 ≈ 40 MB 内存 + 私有写入**。

**两种形态的差异**:ephemeral 容器(`container run`,MAP Node 常态)共享
彻底;machine(持久环境)各自一块稀疏虚拟盘(~1 GB 标称)随写入独立增长,
`homeMount=rw`($HOME 自动挂载)已在 inspect 确认。

## 4. CRIU 可用性(快照降级阶梯的 macOS 分支)

容器内直接读内核配置(`/proc/config.gz` 可读,共 3761 行):

```
# CONFIG_CHECKPOINT_RESTORE is not set
```

- **默认 kata 内核无 CRIU** → macOS 路线 memory/hybrid 快照按 L3
  (协作式 checkpoint)起步,详见 MAP RFC 0002 的四级阶梯;
- CRIU 其余内核依赖全部在位(CGROUP_FREEZER / USER_NS / IO_URING /
  SECCOMP / MEMFD / virtio 全套)——缺的只有这一个开关;
- `container system kernel set --binary` 支持**自定义内核** → **L1 可行**:
  以 kata 锁定的 6.18.35 源码 + 运行时导出的 config(`/tmp/kata-6.18.35.config`)
  翻开 `CHECKPOINT_RESTORE=y`,WSL2 cross-compile(arm64),产物即
  `mixum-kernel` 包,同一 vmlinux 服务 Apple Container / WSL2 / QEMU micro-VM
  三个 runtime;
- 内核选型定论:**kata 基底 = 容器/VM guest 热路径**(亚秒启动 + 每实例
  省 30–80 MB 常驻);**Arch 原生内核 = 完整磁盘镜像形态**(阶段 5,启动
  2–5 s,自带 CRIU)。

## 5. mixumOS 镜像导入与 MAP 全链路

导入路径(OCI layout tar → 本地 registry → apple container):

```bash
container run -d --name mapreg --publish 127.0.0.1:15000:5000 registry:2
skopeo copy --dest-tls-verify=false oci-archive:mixumos-0.1.0-aarch64.oci.tar \
  docker://127.0.0.1:15000/mixumos:0.1.0
container image pull --platform linux/arm64 --scheme http 127.0.0.1:15000/mixumos:0.1.0
container run -d --name map-node --publish 127.0.0.1:17601:7601 \
  127.0.0.1:15000/mixumos:0.1.0
```

三个坑,逐个记录:

1. **pull 对 registry 默认强制 HTTPS**,本地明文 registry 必须显式
   `--scheme http`(否则报 `bad protocol version`);
2. **run 必须用完整引用**(`127.0.0.1:15000/mixumos:0.1.0`),裸名
   `mixumos:0.1.0` 会被解析到 docker.io;
3. **Arch 官方两个镜像都倒在 platform 校验**(疑似 buildx 证明文件
   manifest)——自有 OCI 打包须保持 apple-container 友好(简单 OCI index、
   无 attestation),mkoci.sh 产物已验证合规。

全链路结果:`mixumd --config /dev/null` 起服务(READY token 吐出)→
mixcomp headless(pixman)→ 宿主 TS SDK 经 `wss://127.0.0.1:17601/ws`
完成握手 / `sys.info`(**os=Arch Linux**,backend=directory-reflink)/
`snap.tree`(genesis)/ `task.submit` → queued→running→log→completed。
`sys.info.compositor=false` 与 mixcomp 日志矛盾,待查(字段语义或时序)。

**版本缝隙(旧 0.1.0 ↔ MAP 命名,实证 RFC 0001「adapter trivial」)**:
子协议 `mixum.v1`↔`map.v1`、握手 op `hello`↔`session.hello`、state 事件
`data: "queued"`↔`{state:"queued"}`。2026-10-19 已在 agentd 侧采纳 MAP
命名并同时接受旧子协议/旧握手名;TS SDK 提供 `legacy: "mixum-v1"` 选项
直连旧镜像,不再需要冒烟脚本里的 WebSocket 子类。

## 6. 快照体积实测(真实节点)

| 快照点 | 节点逻辑大小 | 新增块 | CAS 落盘 |
|--------|-------------|--------|---------|
| genesis(空) | 0 B | 0 | 44 K |
| 写入 5 MiB 文本 | 5.0 MiB | 5 | 文本压后 ~1 MB |
| 再写 10 MiB 随机(最坏情况) | 15.0 MiB | 10 | +10 MB |
| **零变化再打一张** | 15.0 MiB | **0** | **零新增** |

引擎延迟基准(300×2 KiB 工作区)另见 snapshots.md §9:无感前置快照
9.3 ms / 恢复物化 1.6 s。**结论:快照大小 = 变化量,与历史快照数量无关**。

## 7. 双节点整树迁移(「迁移后回退任意版本」实证)

第二个容器(map-node-2,同镜像)作为目标:

```text
== A→B 整树推送(拓扑序逐节点 snap.push) ==
  genesis/empty-baseline → 0 块;  text-5m → 5 块 5 MB;
  random-10m → 10 块 10 MB;       no-change → 0 块
== B 端回退验证 ==
  restore(text-5m)  → 工作区: text-5m.txt          ← random-10m.bin 消失
  restore(random-10m) → 工作区: random-10m.bin + text-5m.txt
```

B 端 CAS 11 M,与 A 完全一致;树同构(同 id 同父链),restore 语义与本地
无差。**迁移的不是「最新状态」,是整条时间线**。实操注意:`remote` 格式为
`wss://host:7601`(不带 `/ws`,推送端自拼路径)。

## 8. 缺陷修复:0 块节点推送后不到达目标树

**现象**:上表推送中 empty-baseline/no-change(0 块)成功返回统计,但 B 树
无此节点;text-5m 被迫标记「远端根」。

**根因**(`mixum-agentd/src/sync.rs`):推送端「有块才 commit」——`sent==0`
时跳过 `snap.push.commit`,而落树恰恰发生在 commit;0 块节点与幂等重推的
缺失列表恒为空,节点永不到达。旧回环测试只断言推送统计,恰好掩盖。

**修复**(两处成对,dev `253b3ef` / module/agentd `4975781`):

1. `push_node` 无条件发 commit——落树与是否传输块无关;
2. 接收端 commit 对同 id 已存在节点幂等返回既有 id(`Tree::add` 拒绝重复
   id,不配套则恒发令重推炸出 BranchExists,破坏「重推零成本」承诺)。

**回归测试**:sync.rs 单元两例(0 块落树/同 id 幂等)+ sync_loopback 端到端
一例(0 块节点必须落树且幂等重推成功——旧实现下必红)。验证:dev 全套
15+4 通过,module/agentd 14+3 通过,clippy `-D warnings` 干净。

## 9. 遗留与下一步

- [ ] L1 内核编译(kata 6.18.35 + CHECKPOINT_RESTORE=y,WSL2 编译机),
      编成后 `criu check` + 真 dump/restore 功能验证;
- [ ] 重打镜像(含本修复与 H.264)替换 map-node,复测窗口流与整树迁移
      (0 块节点应全部落树,不再出现「远端根」妥协);
- [ ] machine 形态 systemd 验证(首启瞬态报错后未完成;容器形态
      PID1=入口脚本,不受影响);
- [ ] `sys.info.compositor=false` 与 mixcomp 日志矛盾的排查;
- [ ] `snap.push` 整树 sugar(或 mixum-ctl 一条 tree-sync 命令)。
