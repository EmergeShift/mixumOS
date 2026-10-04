# 快照引擎设计

> 状态:语义已经 Go 参考实现验证(单测 + e2e);本文档是 Rust 重写的规格。

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
  chunks }`;存储为单文件 KV 库(Go 版 bbolt;Rust 版选型 redb/sled,阶段 1 定);
- **分支 = 节点属性,head = 指针**。新快照永远挂在「当前 head」之下并成为
  该分支新 head;
- **原地回退(inplace)**:head 移回旧节点。此后继续工作产生的新快照
  自然成为旧节点的新孩子——与旧时间线并存,即树状分叉;
- **显式 fork**:在某节点上开新分支(head 指向该节点,不改工作区),
  分支名冲突自动追加 `-2`、`-3`;
- **回到任一未来快照**:树里节点永不因回退而丢失,直接 restore 即可;
- 容器初始化时自动打 `genesis` 快照(空工作区),是全树的根;
- 删除:仅允许删叶子节点,防止悬空引用;
- 恢复的安全闸:节点 kernel/arch 与当前环境不一致时拒绝
  (memory 快照跨内核恢复不可靠;disk 快照的拒绝策略阶段 5 再放宽)。

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
不存在「被删节点仍被引用」的中间态)。Sweep 清掉 CAS 中未标记块。
Rust 版可加「保护最近 N 分钟的块」避免与进行中的推送竞态。

## 6. 内存快照(CRIU)

- 探测:`criu` 二进制存在即启用;`sys.info.criuAvailable` 暴露;
- dump:`criu dump --tree <pid> --images-dir <dir> [--leave-running]`,
  镜像目录再走一次目录打包进 CAS(挂到节点的 memory 清单);
- restore:`criu restore --images-dir <dir> --shell-job`;
- 兼容指纹:节点记录 `arch/kernelRelease`,恢复时强校验;
  跨内核版本迁移(如容器滚动升级后)在阶段 5 评估 CRIU 的
  `--external` 与内核版本匹配矩阵;
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
  细节见 host-integration.md(待写)。

## 8. Go 参考实现对照表

| 本文概念 | reference/ 位置 |
|----------|-----------------|
| CAS + 加密 | internal/snap/chunk.go |
| 清单/打包/落地/diff | internal/snap/manifest.go |
| 树 DAG | internal/snap/tree.go |
| 引擎(快照/恢复/fork/GC) | internal/snap/engine.go |
| CRIU 驱动 | internal/snap/memory.go |
| 后端探测 | internal/snap/backend.go |
| 推送/导入(协议侧) | internal/server/handlers_snap.go |
| 行为测试 | internal/snap/engine_test.go, internal/server/server_test.go |
