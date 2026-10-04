# 目标:mixum-snapshot crate(Rust 快照引擎)

> 你负责 mixumOS 核心快照引擎的 Rust 实现(roadmap 阶段 1 的主体)。
> 这是一份自包含任务书:先读本文档,再动手。

## 必读(动手前按序读完)

1. `/Users/ashreo/agent-project/mixumos/docs/snapshots.md` —— 快照引擎规格(你的主规格,含 Go 对照表)
2. `/Users/ashreo/agent-project/mixumos/docs/architecture.md` —— 全局架构、语言分工、reference 定位
3. `/Users/ashreo/agent-project/mixumos/docs/protocol.md` §6 —— 块传输协议(推送/导入语义)
4. Go 参考实现(只读,行为规格):`reference/internal/snap/`(chunk/manifest/tree/engine/memory/backend)与
   `reference/internal/snap/engine_test.go`、`reference/internal/server/server_test.go`(行为测试 = 可执行规格)

## 项目背景(30 秒版)

mixumOS 是基于 Arch Linux 的 Agent OS。快照引擎提供:磁盘/内存/混合三种快照、
git 式快照树(无感前置快照、原地回退、fork 分叉、跨设备流转)、
内容寻址块存储(小而快,跨设备去重)。已有 Go 参考实现全部测试通过;
你的任务是用 Rust 把它做出来,并通过与 Go 的交叉向量证明语义一致。

## 方向铁律(不得违背)

- **Rust + 动态链接常规发行版依赖**,不做 musl 静态宇宙(发行版哲学,
  用户明确要求);
- `reference/` 是可执行规格:**只读**;唯一允许的改动是扩展
  `reference/cmd/genvector`(交叉向量生成器);
- 语义以 Go 实现为准;发现规格/实现不一致,记录到
  `goals/notes/snapshot.md`,不要擅自改语义;
- 中文注释与文档全角标点,平实文风;
- 不扩范围:GC 的「最近 N 分钟保护」、推送竞态等列在 docs/snapshots.md
  的演进项,本次不做,除非验收需要。

## 你的边界(只准动这些)

- 允许:`rust/crates/mixum-snapshot/`(新建)、`rust/Cargo.toml`
  (workspace members 与 workspace.dependencies 增量)、
  `tests/cross-protocol/`(新增向量文件)、`reference/cmd/genvector/`(扩展)、
  `docs/snapshots.md`(只允许追加「Rust 实现记录」附录节)
- 只读/禁改:`reference/` 其余部分、`mixcomp/`、`rust/crates/mixum-protocol/`
  (如确需改,先记录到 notes 并说明理由,保持向后兼容)

## 现状(2026-10-04)

- Rust workspace 已立:`rust/`,crate `mixum-protocol` 18 测试全绿,
  含 Go↔Rust 令牌交叉验证(`tests/cross-protocol/token-vector.json`,
  生成器 `reference/cmd/genvector`);
- Go 快照引擎全部测试通过(树语义/回退/fork/GC/加密,见 engine_test.go);
- 注意 Go 实现中的关键语义(测试锁定,勿改):
  - fork 后新快照跟随**当前工作分支**,不是父节点原分支;
  - 块哈希 = **明文** SHA-256(zstd/AES-GCM 只影响落盘);
  - GCM 的 AAD 绑定 `storeID + 块哈希`,nonce 前置;
  - HKDF:sha256,masterKey,salt=`mixum/snap/v1:<storeID>`,info=`chunk-aead`;
  - 删除仅允许叶子;恢复校验 kernel/arch 指纹。

## 任务

1. 设计 crate 结构(建议 `store`/`manifest`/`tree`/`engine`/`criu` 模块),
   选型落库(bbolt → redb 或 sled,二选一并把理由写进 docs 附录);
   依赖建议:zstd、aes-gcm、hkdf、sha2、serde、thiserror;越少越好。
2. 实现并测试:CAS 块存储(压缩/加密/损坏检测/GC)、清单打包与落地
   (路径安全检查)、树 DAG(genesis/回退/fork/分支改名/叶子删除)、
   引擎门面(冻结回调 hook、快照/恢复/fork/diff)、CRIU 驱动
   (进程封装,非 Linux 下 Available()=false,编译不 gate)。
3. **交叉向量**(核心验收,证明与 Go 互通):
   - 扩展 `reference/cmd/genvector`:导出固定内容的清单 JSON、
     固定明文的块哈希表、(可选)固定 nonce 的 GCM 密文;
   - Rust 侧消费向量:清单互读、哈希互验、树操作结果一致性;
   - 向量文件放 `tests/cross-protocol/`,命名清晰。
4. 行为测试:把 engine_test.go 的每个用例翻译成 Rust 测试
   (用例名对应关系写在测试注释里)。

## 验收标准

- [ ] `cargo test -p mixum-snapshot` 全绿;`cargo clippy -- -D warnings` 干净;
- [ ] 交叉向量测试通过(清单互读 + 块哈希互验 + 至少一个 GCM 密文互解);
- [ ] Go 侧 `cd reference && go test ./internal/snap/` 仍然全绿(未动语义);
- [ ] docs/snapshots.md 追加的「Rust 实现记录」节:选型理由、与 Go 的差异表;
- [ ] `goals/notes/snapshot.md`:交付清单、选型决定、遗留问题。

## 环境备忘

- 宿主 mac(arm64),Rust 1.97(rustup),直接 `cargo` 即可,无需容器;
- Go 1.27 已装(brew);向量生成:`cd reference && go run ./cmd/genvector`;
- workspace 依赖集中管理:改 `rust/Cargo.toml` 的
  `[workspace.dependencies]`,crate 里引用 `{ workspace = true }`。

## 协作规程

- 开工先:`cd /Users/ashreo/agent-project/mixumos && git worktree add
  ../mixumos-snapshot -b module/snapshot`,在 `../mixumos-snapshot` 里工作;
- 只提交你边界内的路径;提交信息中文一句话;
- 完成后:分支提交齐全 + notes 交付说明;不要合并 main;
- 遇阻:记录 notes 并标注 BLOCKER,不要绕开边界。
