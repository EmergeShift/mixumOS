# 快照引擎设计

> 状态:语义经参考实现验证后以 Rust 落地(`rust/crates/mixum-snapshot`,
> 见 §9);本文档仍是快照语义的规格权威。

## 1. 三种快照

| kind | 内容 | 技术 | 典型耗时 | 用途 |
|------|------|------|----------|------|
| disk | 工作区文件系统 | 目录打包进 CAS(见 §2) | 随增量变化,前置快照毫秒级(热数据小) | 任务前置无感快照、常规保存点 |
| memory | 任务进程树完整状态 | CRIU dump | 秒级 | 任务进程迁移/恢复 |
| hybrid | disk + memory | 两者叠加 | 两者之和 | 盒盖迁移、YOLO 模式全态 |

前置快照的「无感」来自两点:任务进程在打包窗口内被冻结
(SIGSTOP 进程组,cgroup v2 可用时用 freezer),进程对冻结无感知;
打包只触及增量(块级去重后,未变更文件零成本)。

## 2. 内容寻址块存储(CAS)

- 块:固定 1 MiB 标称分块,尾块可短;**哈希 = 明文 SHA-256**,
  与压缩/加密无关,保证跨设备去重不受影响;
- 落盘:`objects/<h[:2]>/<h[2:]>`,zstd(fastest 级)压缩后写临时文件
  再原子 rename;
- 静态加密(可配):AES-256-GCM,密钥 = HKDF-SHA256(masterKey,
  salt="mixum/snap/v1:<storeID>", info="chunk-aead"),nonce 前置,
  AAD 绑定 storeID + 块哈希(防块挪用);masterKey 32B,
  首次生成落 `dataDir/master.key`(0600);
- 读取时校验:解压后重算哈希必须等于块名,不匹配即报损坏;
- GC:节点可达性收集全部引用块 → Sweep 删除未标记块(见 §5)。

## 3. 文件清单(manifest)

```
Manifest { version, files: [FileEntry], dirs: [DirEntry] }
FileEntry { path, mode, size, symlink?, chunks: [hash], ino? }
```

- 打包(PackDirectory):遍历工作区,常规文件切块入 CAS,符号链接记目标,
  其他类型(sockets 等)仅记类型;skip 列表排除块仓库自身等;
- 落地(Materialize):目录先建,文件「写临时 + chmod + rename」逐个原子落地;
- 路径安全:清单路径来自不可信来源(远端推送)时,拒绝 `..` 与绝对路径;
- diff:两清单对比得 added/removed/changed 文件与新块统计。

## 4. 快照树(git 式 DAG)

- 节点 TreeNode:`{ id, parent?, branch, label?, kind, auto?, createdAt,
  origin?, disk: Manifest?, memory: Manifest?, kernel, arch, sizeBytes,
  chunks }`;存储为单文件 KV 库(参考实现曾用 bbolt;Rust 版已选 sled,见 §9);
- **分支 = 节点属性,head = 指针**。新快照永远挂在「当前 head」之下并成为
  该分支新 head;
- **原地回退(inplace)**:head 移回旧节点。此后继续工作产生的新快照
  自然成为旧节点的新孩子——与旧时间线并存,即树状分叉;
- **显式 fork**:在某节点上开新分支(head 指向该节点,不改工作区),
  分支名冲突自动追加 `-2`、`-3`;
- **回到任一未来快照**:树里节点永不因回退而丢失,直接 restore 即可;
- 容器初始化时自动打 `genesis` 快照(空工作区),是全树的根;
- 删除:仅允许删叶子节点,防止悬空引用;
- 恢复的安全闸:含 CRIU 内存镜像的节点(memory/hybrid)要求 kernel 完全一致;
  纯 disk 节点跨内核只记 warn 并放行(文件语义不依赖内核)。`arch` 仍强校验。
- 恢复期间由宿主冻结全部任务进程组,`materialize` 完成后再解冻,
  避免 `clear_root` 与运行中的进程读写打架。

### 用户视角的例子

```
20:00 打快照 n2(任务前置)
22:00 工作区变化,快照 n3
        main:  genesis ── n1 ── n2 ── n3
22:30 回退到 n2(inplace),继续工作 → n4
        main:  genesis ── n1 ── n2 ── n3
                                    └─ n4   ← 平行时间线
23:00 发现 22:00 的参数更优,restore(n3, fork, "调参-激进") → 新分支
        调参-激进: n3 → …
```

## 5. GC

可达集合 = 树中全部现存节点引用的块(节点删除只允许删叶子,所以
不存在「被删节点仍被引用」的中间态)。Sweep 清掉 CAS 中未标记块;
「保护最近 10 分钟的块」已实现(`engine.rs::GC_PROTECT_WINDOW`,
按 mtime 跳过),进行中的推送/导入写入的块不会被并发 GC 清掉。

## 6. 内存快照(CRIU)

- 探测:`criu` 二进制存在即启用;`sys.info.criuAvailable` 暴露;
- dump:`criu dump --tree <pid> --images-dir <dir> [--leave-running]`,
  镜像目录再走一次目录打包进 CAS(挂到节点的 memory 清单);
- restore:`criu restore --images-dir <dir> --shell-job`;
- 兼容指纹:节点记录 `arch/kernelRelease`,恢复时强校验(保守门禁);
  **受控实验(2026-10-20)已验证最小静态进程跨内核 restore 可行**——源 6.18+CRIU 4.2.1
  → 目标 6.1.0-47+同版本 CRIU,目标 control PASS 后跨内核 restore 两轮
  PASS(进程计数继续);此前「不可行」系环境混杂,已纠正,方法与证据见
  docs/criu-feasibility.md §4.4。覆盖范围仅最小静态进程,精确指纹门禁
  仍为默认,按场景扩展后再评估放宽;
- v1 限制:纯 memory 快照不允许恢复(必须配合 disk);
  进程持有的外部 fd(unix socket 到 mixumd)由任务引擎在快照前规范化。

## 7. 跨设备流转

协议见 protocol.md §6。补充语义:

- origin 字段记录来源主机名,树里可见「这个分支从哪来」;
- 推送是幂等的:第二次推送 missing 为空,零字节传输;
- 传输只发明文块,哈希即校验和;落盘加密是各端自己的事;
- 「mac 盒盖 → 云端续跑」链路:mixum-ctl 监听电源事件 → hybrid 快照
  → snap.push 到云端 → 云端容器 restore 该节点 → 任务继续;
  反向回迁同理。平台侧(macOS)用 IOKN 通知在盒盖前抢出几秒窗口,
  细节见 host-integration.md。

## 8. Go 参考实现对照表(已移除)

Go 参考实现已移除(2026-10-16),下表保留为概念溯源;行为以
`rust/crates/mixum-snapshot` 为准。

| 本文概念 | 原 reference/ 位置 |
|----------|-----------------|
| CAS + 加密 | internal/snap/chunk.go |
| 清单/打包/落地/diff | internal/snap/manifest.go |
| 树 DAG | internal/snap/tree.go |
| 引擎(快照/恢复/fork/GC) | internal/snap/engine.go |
| CRIU 驱动 | internal/snap/memory.go |
| 后端探测 | internal/snap/backend.go |
| 推送/导入(协议侧) | internal/server/handlers_snap.go |
| 行为测试 | internal/snap/engine_test.go, internal/server/server_test.go |

## 9. Rust 实现记录

Rust 实现位于 `rust/crates/mixum-snapshot`，并以本节与本文规格为准。树的持久化后端选用 **sled**：它提供嵌入式有序 KV、事务式单键更新和跨平台文件存储，接口比 bbolt 的事务回调更适合 Rust；快照节点、分支 head 和当前分支分别存放在 `nodes`、`heads`、`meta` 树中。阶段 1 不依赖外部数据库服务。

与 Go 参考实现的对应关系如下：

| 能力 | Rust 实现 | 说明 |
|------|-----------|------|
| CAS | `store.rs` | zstd、明文 SHA-256、可选 AES-256-GCM；HKDF 盐、`chunk-aead` info、AAD 与 Go 一致 |
| 清单 | `manifest.rs` | 固定 1 MiB 分块、目录/符号链接、原子落地、路径穿越拒绝、diff |
| 树 DAG | `tree.rs` | genesis、head、回退、fork 自动编号、叶子删除、分支改名 |
| 引擎 | `engine.rs` | disk/memory/hybrid、冻结 hook、恢复、GC；纯 memory 恢复拒绝 |
| CRIU | `criu.rs` | 启动时探测 `criu`，非 Linux 仍可编译；不可用时返回明确错误 |

Rust 节点 ID 使用本地随机十六进制后缀，时间字段仍为 Unix 毫秒；JSON 字段使用协议规定的 camelCase。Go 侧 bbolt 的事务存储和 Rust 侧 sled 的文件格式不互通，但节点/清单/块的线格式与语义由交叉向量测试锁定。GC 的推送竞态保护已实现(§5,10 分钟 mtime 窗口)。

### 引擎延迟基准（2026-10-17，Linux 容器 release 构建）

复现:`cargo run --release -p mixum-snapshot --example bench`。

| 工作区 | 冷打包 | 无变更(中位) | 部分变更 | 恢复物化 |
|--------|--------|--------------|----------|----------|
| code:300 × 2 KiB 小文件 | 1651 ms | 9.3 ms | 813 ms | 1593 ms |
| assets:50 MiB 单文件 | 78.5 ms | 36.7 ms | 77.3 ms | 92.2 ms |

结论:**「无感前置快照」在热路径成立**(未变更工作区 9 ms / 大文件 37 ms),
大文件吞吐约 640 MB/s。短板是小文件工作区:逐块「临时文件+rename」与
sled 单键提交的每文件开销 ≈2.7 ms(打包)/ ≈5.3 ms(恢复),几百个
小文件即进入秒级。优化方向(未实施):块分组批量落盘、清单复用
(ino/size/mtime 未变直接引用旧块)、sled 批量事务。跨节点流转
(盒盖→云端续跑)已由双节点 e2e 全链验证,见 tests/integration/sync-e2e.mjs。
