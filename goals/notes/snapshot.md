# mixum-snapshot 交付记录

## 已完成

- 新增 `rust/crates/mixum-snapshot`：CAS 块存储、清单、快照树、引擎与 CRIU 驱动。
- CAS 使用 zstd + 明文 SHA-256；加密模式使用 AES-256-GCM，HKDF 与 AAD 和 Go 参考实现一致。
- 清单支持目录、常规文件、符号链接、跳过路径、原子落地、路径安全检查和 diff。
- 快照树支持 genesis、当前分支/head、inplace 恢复、fork 分支、自动编号、分支改名和叶子删除。
- 引擎支持 disk/memory/hybrid、冻结回调、恢复指纹校验和块 GC；非 Linux/无 CRIU 时保持可编译并报告不可用。
- `reference/cmd/genvector` 增加清单、块哈希和固定 nonce GCM 向量；Rust 测试消费 `tests/cross-protocol/snapshot-vector.json`。
- Rust 行为测试覆盖 Go `engine_test.go` 的 genesis/增量、恢复/fork、命名、diff、GC、加密和冻结 hook，并加入持久化树、分支改名、符号链接父目录防护、对象损坏检测和 Go 向量互验。

## 选型决定

树后端选用 sled。它是嵌入式有序 KV，部署不需要服务进程，且在 Rust 中可以直接按树组织 nodes、heads、meta；阶段 1 的持久化需求不需要额外数据库抽象。

## 验证

- `cargo test -p mixum-snapshot`：10 个测试通过。
- `cargo clippy -p mixum-snapshot -- -D warnings`：通过。
- `cd reference && go test ./internal/snap/...`：通过。
- `cargo test --workspace`：`mixum-protocol` 18 个、`mixum-snapshot` 10 个测试通过。

## 遗留问题

- `Tree` 的节点 JSON 与 Go 互读，数据库文件本身不互通；跨设备应使用协议层的节点/清单/块传输。
- 最近 N 分钟 GC 保护、推送期间的并发保护和 CRIU 实机恢复验收留到后续阶段。
