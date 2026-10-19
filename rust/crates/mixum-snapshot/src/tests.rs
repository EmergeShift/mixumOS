use super::*;
use std::{
    fs,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tempfile::TempDir;

fn engine(encrypt: bool) -> (Engine, TempDir, TempDir) {
    let data = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("hello.txt"), "v1").unwrap();
    let e = Engine::open(EngineOptions {
        data_dir: data.path().into(),
        root: root.path().into(),
        skip: vec![],
        encrypt,
    })
    .unwrap();
    (e, data, root)
}

#[test]
fn store_round_trip_and_integrity() {
    let dir = TempDir::new().unwrap();
    let key = [0xabu8; 32];
    let s = Store::open(dir.path(), Some(&key), "test-store").unwrap();
    let hash = s.put(b"secret payload").unwrap();
    assert_eq!(s.get(&hash).unwrap(), b"secret payload");
    assert_eq!(s.count(), 1);
    let path = dir.path().join("objects").join(&hash[..2]).join(&hash[2..]);
    assert!(!String::from_utf8_lossy(&fs::read(path).unwrap()).contains("secret"));
}

#[test]
fn encrypted_engine_round_trip_and_corruption_is_detected() {
    // 对应 Go: TestEncryptedStoreRoundTrip。
    let (mut e, _data, root) = engine(true);
    let genesis = e.tree.head().unwrap().1.unwrap();
    fs::write(root.path().join("secret.txt"), "机密内容").unwrap();
    let saved = e.snapshot(KIND_DISK, "encrypted", false).unwrap();
    let hash = saved
        .disk
        .as_ref()
        .unwrap()
        .files
        .iter()
        .find(|f| f.path == "secret.txt")
        .unwrap()
        .chunks[0]
        .clone();
    let object = e
        .store
        .dir()
        .join("objects")
        .join(&hash[..2])
        .join(&hash[2..]);
    let raw = fs::read(&object).unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("机密内容"));
    e.restore(&genesis.id, "inplace", "").unwrap();
    assert!(!root.path().join("secret.txt").exists());
    fs::write(&object, [raw.as_slice(), b"bad"].concat()).unwrap();
    assert!(e.store.get(&hash).is_err());
}

#[test]
fn manifest_rejects_unsafe_paths() {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path(), None, "test").unwrap();
    let m = Manifest {
        version: 1,
        files: vec![FileEntry {
            path: "../escape".into(),
            mode: 0o100644,
            size: 0,
            symlink: String::new(),
            chunks: vec![],
            dev: 0,
            ino: 0,
        }],
        dirs: vec![],
    };
    assert!(m.materialize(&store, dir.path().join("out")).is_err());
}

#[test]
fn genesis_incremental_and_diff() {
    // 对应 Go: TestGenesisAndIncremental、TestDiff。
    let (mut e, _data, root) = engine(false);
    let (_, genesis) = e.tree.head().unwrap();
    assert_eq!(genesis.unwrap().label, "genesis");
    fs::write(root.path().join("hello.txt"), "v2").unwrap();
    let n2 = e.snapshot(KIND_DISK, "second", false).unwrap();
    let n1 = e.tree.get(&n2.parent).unwrap();
    assert_eq!(n2.branch, "main");
    assert_eq!(n2.parent, n1.id);
    let (added, removed, changed, new_chunks) =
        diff_manifests(n1.disk.as_ref().unwrap(), n2.disk.as_ref().unwrap());
    assert!(
        added.is_empty() && removed.is_empty() && changed == vec!["hello.txt"] && new_chunks > 0
    );
    let via_engine = e.diff(&n1.id, &n2.id).unwrap();
    assert_eq!(via_engine, (added, removed, changed, new_chunks));
}

#[test]
fn restore_inplace_and_fork_follow_current_branch() {
    // 对应 Go: TestRestoreInplaceAndDiverge。
    let (mut e, _data, root) = engine(false);
    let (_, n1) = e.tree.head().unwrap();
    let n1 = n1.unwrap();
    fs::write(root.path().join("hello.txt"), "v2").unwrap();
    let n2 = e.snapshot(KIND_DISK, "20:00", false).unwrap();
    fs::write(root.path().join("hello.txt"), "v3").unwrap();
    fs::write(root.path().join("extra.txt"), "extra").unwrap();
    let n3 = e.snapshot(KIND_DISK, "22:00", false).unwrap();
    e.restore(&n2.id, "inplace", "").unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join("hello.txt")).unwrap(),
        "v2"
    );
    assert!(!root.path().join("extra.txt").exists());
    e.restore(&n1.id, "fork", "fix-param").unwrap();
    fs::write(root.path().join("param.txt"), "changed").unwrap();
    let nf = e
        .snapshot_on_branch(KIND_DISK, "fork", false, &n1.id)
        .unwrap();
    assert_eq!(nf.parent, n1.id);
    assert_ne!(nf.branch, "main");
    assert!(e.tree.get(&n3.id).is_ok());
}

/// 把仓库内全部块文件的 mtime 回拨到纪元,模拟「保护窗口之外的旧块」。
fn backdate_store_chunks(data_dir: &Path) {
    for level in fs::read_dir(data_dir.join("store/objects"))
        .expect("store objects dir")
        .filter_map(Result::ok)
    {
        for f in fs::read_dir(level.path()).unwrap().filter_map(Result::ok) {
            fs::OpenOptions::new()
                .write(true)
                .open(f.path())
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
                .unwrap();
        }
    }
}

#[test]
fn fork_names_and_leaf_gc() {
    // 对应 Go: TestForkBranchNaming、TestGcRemovesOrphans。
    let (mut e, data, root) = engine(false);
    let (_, n1) = e.tree.head().unwrap();
    let n1 = n1.unwrap();
    let b1 = e.fork(&n1.id, "experiment").unwrap();
    let b2 = e.fork(&n1.id, "experiment").unwrap();
    assert_eq!(b1, "experiment");
    assert_eq!(b2, "experiment-2");
    fs::write(
        root.path().join("hello.txt"),
        "totally different content for gc test",
    )
    .unwrap();
    let n2 = e.snapshot(KIND_DISK, "b", false).unwrap();
    let before = e.store.count();
    assert!(before >= 2);
    assert!(e.tree.delete(&n2.id).is_ok());
    // 删除产生的孤儿块是刚写入的(mtime 在保护窗口内):第一轮 GC 不动它们。
    assert_eq!(e.gc().unwrap(), 0);
    backdate_store_chunks(data.path());
    assert!(e.gc().unwrap() > 0);
    assert!(e.restore(&n1.id, "inplace", "").is_ok());
}

#[test]
fn gc_protects_recent_orphan_chunks() {
    // 推送竞态保护(§5):远端刚收到、树还没落的块不能被并发 GC 清掉。
    let (mut e, data, root) = engine(false);
    let (_, genesis) = e.tree.head().unwrap();
    let genesis = genesis.unwrap();
    fs::write(root.path().join("hello.txt"), "内容变了,旧块成为孤儿").unwrap();
    let n2 = e.snapshot(KIND_DISK, "second", false).unwrap();
    e.restore(&genesis.id, "inplace", "").unwrap();
    e.tree.delete(&n2.id).unwrap();
    // 孤儿块很新:保护窗口内 GC 零删除,数据仍可恢复。
    assert_eq!(e.gc().unwrap(), 0);
    // 窗口过后:旧孤儿块被清,被树引用的块(genesis)保留。
    backdate_store_chunks(data.path());
    let removed = e.gc().unwrap();
    assert!(removed > 0);
    assert!(e.restore(&genesis.id, "inplace", "").is_ok());
}

#[test]
fn tree_persists_and_renames_branch() {
    let dir = TempDir::new().unwrap();
    let node_id;
    {
        let tree = Tree::open(dir.path()).unwrap();
        node_id = tree
            .add(TreeNode::new("root", "", "main", KIND_DISK))
            .unwrap()
            .id;
        let branch = tree.fork_branch(&node_id, "experiment").unwrap();
        // 分支上落一个真实节点,rename 后其 branch 字段必须跟着改写。
        tree.add(TreeNode::new("child", &node_id, &branch, KIND_DISK))
            .unwrap();
        assert_eq!(tree.rename_branch(&branch, "renamed").unwrap(), "renamed");
        assert_eq!(tree.current_branch().unwrap(), "main");
    }
    let tree = Tree::open(dir.path()).unwrap();
    assert_eq!(tree.get(&node_id).unwrap().id, node_id);
    assert!(tree.branches().unwrap().contains_key("renamed"));
    assert!(!tree.branches().unwrap().contains_key("experiment"));
    assert_eq!(tree.get("child").unwrap().branch, "renamed");
}

#[test]
fn delete_branch_head_hands_head_to_parent() {
    let dir = TempDir::new().unwrap();
    let tree = Tree::open(dir.path()).unwrap();
    let root = tree.add(TreeNode::new("a", "", "main", KIND_DISK)).unwrap();
    let child = tree
        .add(TreeNode::new("b", "a", "main", KIND_DISK))
        .unwrap();
    assert_eq!(
        tree.head_id("main").unwrap().as_deref(),
        Some(child.id.as_str())
    );
    // 删除 head:分支头移交父节点,分支不失头。
    tree.delete(&child.id).unwrap();
    assert_eq!(
        tree.head_id("main").unwrap().as_deref(),
        Some(root.id.as_str())
    );
    // 删除根节点:没有父节点可移交,分支条目随之移除。
    tree.delete(&root.id).unwrap();
    assert!(tree.head_id("main").unwrap().is_none());
}

// symlink 攻击面构造依赖 unix symlink;Windows 上无断言意义,整测门控。
#[cfg(unix)]
#[test]
fn materialize_rejects_symlink_parent() {
    let dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let root = dir.path().join("dst");
    fs::create_dir(&root).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
    let store = Store::open(dir.path().join("store"), None, "test").unwrap();
    let hash = store.put(b"escape").unwrap();
    let manifest = Manifest {
        version: 1,
        dirs: vec![],
        files: vec![FileEntry {
            path: "link/escape.txt".into(),
            mode: 0o644,
            size: 6,
            symlink: String::new(),
            chunks: vec![hash],
            dev: 0,
            ino: 0,
        }],
    };
    #[cfg(unix)]
    assert!(manifest.materialize(&store, &root).is_err());
}

#[test]
fn freeze_hook_runs_and_criu_is_optional() {
    // 对应 Go: TestFreezeCallbackRuns；CRIU 可用性是 Rust 侧新增的跨平台检查。
    let (mut e, _data, _root) = engine(false);
    let called = Arc::new(AtomicBool::new(false));
    let marker = Arc::clone(&called);
    e.freeze = Some(Box::new(move |f| {
        marker.store(true, Ordering::SeqCst);
        f()
    }));
    e.snapshot(KIND_DISK, "frozen", false).unwrap();
    assert!(called.load(Ordering::SeqCst));
    if cfg!(not(target_os = "linux")) {
        assert!(Criu::probe().is_none());
    }
    assert!(Path::new(&e.root).is_dir());
}

#[test]
fn go_snapshot_vector_manifest_hash_and_gcm() {
    let vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../tests/cross-protocol/snapshot-vector.json"
    ))
    .unwrap();
    let manifest: Manifest = serde_json::from_value(vector["manifest"].clone()).unwrap();
    assert_eq!(manifest.version, 1);
    assert_eq!(manifest.files.len(), 2);
    for file in &manifest.files {
        let entry = &vector["chunks"][&file.chunks[0]];
        let plain = decode_hex(entry["plain_hex"].as_str().unwrap());
        assert_eq!(hash_bytes(&plain), file.chunks[0]);
    }

    let gcm = &vector["gcm"];
    let dir = TempDir::new().unwrap();
    let key = decode_hex(gcm["master_key_hex"].as_str().unwrap());
    let store = Store::open(dir.path(), Some(&key), gcm["store_id"].as_str().unwrap()).unwrap();
    let hash = gcm["hash"].as_str().unwrap();
    let object = dir.path().join("objects").join(&hash[..2]).join(&hash[2..]);
    fs::create_dir_all(object.parent().unwrap()).unwrap();
    fs::write(object, decode_hex(gcm["ciphertext_hex"].as_str().unwrap())).unwrap();
    assert_eq!(
        store.get(hash).unwrap(),
        decode_hex(vector["chunks"][hash]["plain_hex"].as_str().unwrap())
    );
}

fn decode_hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16).unwrap();
            let low = (pair[1] as char).to_digit(16).unwrap();
            ((high << 4) | low) as u8
        })
        .collect()
}
