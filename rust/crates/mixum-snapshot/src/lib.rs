//! mixumOS 快照引擎。
//!
//! 块哈希始终是明文 SHA-256；压缩和静态加密只影响本地对象文件。

mod criu;
mod engine;
mod manifest;
mod store;
mod tree;

pub use criu::{env_fingerprint, Criu, CriuError};
pub use engine::{
    DiffResult, Engine, EngineError, EngineOptions, FreezeFn, GC_PROTECT_WINDOW,
};
/// 与 Go 参考实现 `Options` 名称对应的便捷别名。
pub type Options = EngineOptions;
pub use manifest::{diff_manifests, DirEntry, FileEntry, Manifest, ManifestError, PackStats};
pub use store::{hash_bytes, Store, StoreError, CHUNK_SIZE};
pub use tree::{Tree, TreeError, TreeNode, KIND_DISK, KIND_HYBRID, KIND_MEMORY};

#[cfg(test)]
mod tests;
