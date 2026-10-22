//! mixumOS ↔ MAP `.mart` 迁移桥。
//!
//! - `export_node`：把 CAS 里的磁盘快照物化到临时目录，用 `map-snapshot` 打成 `.mart`；
//! - `import_mart`：校验 `.mart`，恢复到临时目录，再打包进本地 CAS，挂到当前 head。
//!
//! 块哈希两边都是明文 SHA-256，所以导出/导入不需要重新计算内容地址。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use map_snapshot::{pack, MartArchive, PackOptions};
use mixum_snapshot::{Engine, EngineError, Manifest, TreeNode, KIND_DISK};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MartError {
    #[error(transparent)]
    Snapshot(#[from] EngineError),
    #[error(transparent)]
    Mart(#[from] map_snapshot::MartError),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Tree(#[from] mixum_snapshot::TreeError),
    #[error(transparent)]
    Manifest(#[from] mixum_snapshot::ManifestError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("节点 {0} 没有磁盘清单，无法导出 .mart")]
    NoDisk(String),
    #[error(".mart 校验失败：{0}")]
    Verify(String),
}

#[derive(Debug, Clone)]
pub struct ImportReport {
    pub node: TreeNode,
    /// 本次写进 CAS 的块数。
    pub chunks: usize,
    /// 逻辑字节数。
    pub bytes: u64,
}

/// 导出指定节点的磁盘快照为 `.mart`。
pub fn export_node(engine: &Engine, node_id: &str, out: impl AsRef<Path>) -> Result<(), MartError> {
    let node = engine.tree.get(node_id)?;
    let disk = node
        .disk
        .as_ref()
        .ok_or_else(|| MartError::NoDisk(node_id.to_string()))?;
    let temp = tempfile::tempdir()?;
    disk.materialize(&engine.store, temp.path())?;
    pack(temp.path(), out.as_ref(), &PackOptions::default())?;
    Ok(())
}

/// 导入 `.mart`：校验 → 恢复 → 打包进 CAS → 挂到当前 head。
pub fn import_mart(
    engine: &mut Engine,
    file: impl AsRef<Path>,
    origin: &str,
) -> Result<ImportReport, MartError> {
    // origin 会进入节点标签/同步元数据;空白值没有可信来源,直接拒绝。
    let origin = origin.trim();
    if origin.is_empty() {
        return Err(MartError::Invalid("origin 不能为空或纯空白".into()));
    }
    let archive = MartArchive::open(file.as_ref())?;
    let verify = archive.verify()?;
    if !verify.ok {
        return Err(MartError::Verify("块哈希或清单校验失败".into()));
    }
    let temp = tempfile::tempdir()?;
    archive.restore(temp.path())?;
    let (manifest, stats) = Manifest::pack_directory(temp.path(), &engine.store, &engine.skip)?;

    let (branch, head) = engine.tree.head()?;
    let parent = head.map(|node| node.id).unwrap_or_default();
    let mut node = TreeNode::new(new_node_id(), parent, branch.clone(), KIND_DISK);
    node.label = format!("imported from {origin}");
    node.origin = origin.to_owned();
    node.kernel = kernel_release();
    node.arch = std::env::consts::ARCH.to_owned();
    node.size_bytes = stats.bytes_logical;
    node.chunks = manifest.chunks().len();
    node.disk = Some(manifest);

    let node = engine.tree.add(node)?;
    engine.tree.set_head(&branch, &node.id)?;
    engine.tree.set_current_branch(&branch)?;

    Ok(ImportReport {
        node,
        chunks: stats.chunks,
        bytes: stats.bytes_logical,
    })
}

fn new_node_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}{:x}", std::process::id())
}

fn kernel_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|value| value.trim().to_owned())
        .unwrap_or_else(|_| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mixum_snapshot::EngineOptions;

    fn engine(root: &Path, data: &Path) -> Engine {
        Engine::open(EngineOptions {
            data_dir: data.into(),
            root: root.into(),
            skip: vec![".mixum".into()],
            encrypt: false,
        })
        .unwrap()
    }

    #[test]
    fn import_rejects_blank_origin() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut engine = engine(root.path(), data.path());
        let err = import_mart(&mut engine, root.path().join("missing.mart"), "   ").unwrap_err();
        assert!(matches!(err, MartError::Invalid(_)), "{err}");
    }

    #[test]
    fn export_import_roundtrip() {
        let source_root = tempfile::tempdir().unwrap();
        let source_data = tempfile::tempdir().unwrap();
        std::fs::write(source_root.path().join("hello.txt"), "from source").unwrap();
        std::fs::create_dir(source_root.path().join("dir")).unwrap();
        std::fs::write(source_root.path().join("dir/nested.txt"), "nested").unwrap();

        let mut source = engine(source_root.path(), source_data.path());
        let node = source.snapshot(KIND_DISK, "export-me", false).unwrap();

        let mart = tempfile::tempdir().unwrap();
        let file = mart.path().join("node.mart");
        export_node(&source, &node.id, &file).unwrap();
        assert!(file.exists());

        let target_root = tempfile::tempdir().unwrap();
        let target_data = tempfile::tempdir().unwrap();
        let mut target = engine(target_root.path(), target_data.path());
        let report = import_mart(&mut target, &file, "test-host").unwrap();

        assert_eq!(report.node.origin, "test-host");
        assert!(report.bytes > 0);
        target
            .restore(&report.node.id, "inplace", "")
            .expect("restore imported node");
        assert_eq!(
            std::fs::read_to_string(target_root.path().join("hello.txt")).unwrap(),
            "from source"
        );
        assert_eq!(
            std::fs::read_to_string(target_root.path().join("dir/nested.txt")).unwrap(),
            "nested"
        );
    }

    #[test]
    fn rejects_corrupted_mart() {
        let source_root = tempfile::tempdir().unwrap();
        let source_data = tempfile::tempdir().unwrap();
        std::fs::write(source_root.path().join("a.txt"), "data").unwrap();
        let mut source = engine(source_root.path(), source_data.path());
        let node = source.snapshot(KIND_DISK, "bad", false).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bad.mart");
        export_node(&source, &node.id, &file).unwrap();
        let mut bytes = std::fs::read(&file).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&file, bytes).unwrap();

        let target_root = tempfile::tempdir().unwrap();
        let target_data = tempfile::tempdir().unwrap();
        let mut target = engine(target_root.path(), target_data.path());
        assert!(import_mart(&mut target, &file, "test").is_err());
    }
}
