//! 快照宿主 trait 和可测试的内存实现。
//!
//! 真实 mixum-snapshot crate 接入前，agentd 只依赖这组最小异步契约；内存实现
//! 保留 DAG、HEAD 和分支语义，足够跑协议与任务前置快照测试。

use crate::wire::{SnapNode, SnapTreeResponse};
use async_trait::async_trait;
use mixum_protocol::GuiSession;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("快照不存在：{0}")]
    NotFound(String),
    #[error("快照操作冲突：{0}")]
    Conflict(String),
    #[error("快照参数无效：{0}")]
    Invalid(String),
    #[error("快照内部错误：{0}")]
    Internal(String),
}

#[async_trait]
pub trait SnapshotEngine: Send + Sync {
    async fn create(
        &self,
        kind: &str,
        label: Option<&str>,
        parent: Option<&str>,
        branch: Option<&str>,
        auto: bool,
    ) -> Result<SnapNode, SnapshotError>;
    /// Merge extension metadata (base digest, provenance) into one node.
    async fn annotate(
        &self,
        _node: &str,
        _tags: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), SnapshotError> {
        Ok(())
    }
    async fn restore(
        &self,
        node: &str,
        strategy: &str,
        label: Option<&str>,
    ) -> Result<RestoreResult, SnapshotError>;
    async fn fork(&self, node: &str, branch: &str) -> Result<String, SnapshotError>;
    async fn tree(&self) -> Result<SnapTreeResponse, SnapshotError>;
    async fn head(&self) -> Result<Option<String>, SnapshotError>;
    async fn delete(&self, node: &str) -> Result<bool, SnapshotError>;
    async fn gc(&self) -> Result<u32, SnapshotError>;

    /// 导出节点为 `.mart`，返回文件大小。默认不支持。
    async fn export_mart(&self, _node: &str, _out: &Path) -> Result<u64, SnapshotError> {
        Err(SnapshotError::Invalid(
            "当前快照后端不支持 .mart 导出".into(),
        ))
    }
    /// 从 `.mart` 导入节点，返回新节点 id。默认不支持。
    async fn import_mart(&self, _file: &Path, _origin: &str) -> Result<String, SnapshotError> {
        Err(SnapshotError::Invalid(
            "当前快照后端不支持 .mart 导入".into(),
        ))
    }

    /// sys.info 的 snapshotBackend 字段(对齐 reference 的 BackendName)。
    fn backend_name(&self) -> &str {
        "memory"
    }
    /// sys.info 的 criuAvailable 字段。
    fn criu_available(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone)]
pub struct RestoreResult {
    pub restored_to: String,
    pub new_branch: Option<String>,
    pub new_node: Option<String>,
    /// 恢复节点携带的 GUI 会话清单；调用方按策略重放。
    pub gui_sessions: Vec<GuiSession>,
}

#[derive(Clone)]
pub struct MemorySnapshotEngine {
    inner: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    nodes: HashMap<String, SnapNode>,
    head: String,
    branch: String,
}

impl Default for MemorySnapshotEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl MemorySnapshotEngine {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(State {
                branch: "main".into(),
                head: String::new(),
                ..State::default()
            })),
        }
    }

    /// MAP snapshot-node ids are ULIDs (snapshot-node.schema.json).
    fn id() -> String {
        mixum_protocol::envelope::new_id()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

#[async_trait]
impl SnapshotEngine for MemorySnapshotEngine {
    async fn create(
        &self,
        kind: &str,
        label: Option<&str>,
        parent: Option<&str>,
        branch: Option<&str>,
        auto: bool,
    ) -> Result<SnapNode, SnapshotError> {
        if !matches!(kind, "disk" | "memory" | "hybrid") {
            return Err(SnapshotError::Invalid(format!("未知快照类型：{kind}")));
        }
        let mut state = self.inner.lock().await;
        let parent = parent
            .map(str::to_owned)
            .or_else(|| (!state.head.is_empty()).then(|| state.head.clone()));
        if let Some(ref p) = parent {
            if !state.nodes.contains_key(p) {
                return Err(SnapshotError::NotFound(p.clone()));
            }
        }
        let branch = branch
            .map(str::to_owned)
            .unwrap_or_else(|| state.branch.clone());
        let node = SnapNode {
            id: Self::id(),
            parent,
            branch: branch.clone(),
            label: label.filter(|s| !s.is_empty()).map(str::to_owned),
            kind: kind.to_owned(),
            auto,
            created_at: now_ms(),
            origin: None,
            size_bytes: 0,
            chunks: 0,
            kernel: None,
            arch: None,
            gui_sessions: Vec::new(),
            tags: std::collections::BTreeMap::new(),
        };
        state.branch = branch;
        state.head = node.id.clone();
        state.nodes.insert(node.id.clone(), node.clone());
        Ok(node)
    }

    async fn annotate(
        &self,
        node: &str,
        tags: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), SnapshotError> {
        let mut state = self.inner.lock().await;
        let entry = state
            .nodes
            .get_mut(node)
            .ok_or_else(|| SnapshotError::NotFound(node.to_owned()))?;
        entry.tags.extend(tags.clone());
        Ok(())
    }

    async fn restore(
        &self,
        node: &str,
        strategy: &str,
        label: Option<&str>,
    ) -> Result<RestoreResult, SnapshotError> {
        let mut state = self.inner.lock().await;
        let selected = state
            .nodes
            .get(node)
            .cloned()
            .ok_or_else(|| SnapshotError::NotFound(node.into()))?;
        match strategy {
            "" | "inplace" => {
                state.head = selected.id.clone();
                state.branch = selected.branch.clone();
                Ok(RestoreResult {
                    restored_to: selected.id,
                    new_branch: None,
                    new_node: None,
                    gui_sessions: Vec::new(),
                })
            }
            "fork" => {
                let branch = label
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{}-fork", selected.branch));
                state.branch = branch.clone();
                state.head = selected.id.clone();
                Ok(RestoreResult {
                    restored_to: selected.id,
                    new_branch: Some(branch),
                    new_node: None,
                    gui_sessions: Vec::new(),
                })
            }
            other => Err(SnapshotError::Invalid(format!("未知恢复策略：{other}"))),
        }
    }

    async fn fork(&self, node: &str, branch: &str) -> Result<String, SnapshotError> {
        if branch.trim().is_empty() {
            return Err(SnapshotError::Invalid("分支名为空".into()));
        }
        let mut state = self.inner.lock().await;
        if !state.nodes.contains_key(node) {
            return Err(SnapshotError::NotFound(node.into()));
        }
        state.branch = branch.to_owned();
        Ok(branch.to_owned())
    }

    async fn tree(&self) -> Result<SnapTreeResponse, SnapshotError> {
        let state = self.inner.lock().await;
        Ok(SnapTreeResponse {
            head: state.head.clone(),
            branch: state.branch.clone(),
            nodes: state.nodes.values().cloned().collect(),
        })
    }

    async fn head(&self) -> Result<Option<String>, SnapshotError> {
        let head = self.inner.lock().await.head.clone();
        Ok((!head.is_empty()).then_some(head))
    }

    async fn delete(&self, node: &str) -> Result<bool, SnapshotError> {
        let mut state = self.inner.lock().await;
        if state
            .nodes
            .values()
            .any(|n| n.parent.as_deref() == Some(node))
        {
            return Err(SnapshotError::Conflict("仅叶子节点可删除".into()));
        }
        let parent = state.nodes.get(node).and_then(|n| n.parent.clone());
        let removed = state.nodes.remove(node).is_some();
        if removed && state.head == node {
            // 分支头移交父节点,与真实 Tree::delete 语义一致。
            state.head = parent.unwrap_or_default();
        }
        Ok(removed)
    }

    async fn gc(&self) -> Result<u32, SnapshotError> {
        Ok(0)
    }
}

/// 真实 mixum-snapshot 引擎的线程共享句柄:适配器与同步后端共用同一实例。
#[cfg(feature = "real-snapshot")]
pub type SharedEngine = Arc<std::sync::Mutex<mixum_snapshot::Engine>>;
#[cfg(feature = "real-snapshot")]
type RestoreHook = Arc<dyn Fn(String, bool, Vec<GuiSession>) + Send + Sync>;

/// mixum-snapshot crate 的异步适配器:trait 语义与 Go reference 的
/// snap.Engine 调用面逐项对齐(create 的 branch 参数同 Go 一并忽略)。
#[cfg(feature = "real-snapshot")]
pub struct RealSnapshotEngine {
    engine: SharedEngine,
    /// 当前任务树首领 pid（由 LocalSandbox 写入，CRIU dump 用）。
    leader: Option<Arc<AtomicI32>>,
    restore_hook: Option<RestoreHook>,
}

#[cfg(feature = "real-snapshot")]
impl RealSnapshotEngine {
    pub fn open(opts: mixum_snapshot::EngineOptions) -> Result<Self, mixum_snapshot::EngineError> {
        Ok(Self::from_engine(mixum_snapshot::Engine::open(opts)?))
    }

    pub fn from_engine(engine: mixum_snapshot::Engine) -> Self {
        Self {
            engine: Arc::new(std::sync::Mutex::new(engine)),
            leader: None,
            restore_hook: None,
        }
    }

    /// restore 完成（锁已释放）后回调，用于按 gui_sessions 重放窗口。
    /// 回调参数：节点 origin、是否已由 CRIU 恢复内存、GUI 会话清单。
    pub fn set_restore_hook(&mut self, hook: RestoreHook) {
        self.restore_hook = Some(hook);
    }

    /// 包住既有共享句柄(与 SyncBackend 共用同一引擎实例)。
    pub fn from_shared(engine: SharedEngine) -> Self {
        Self {
            engine,
            leader: None,
            restore_hook: None,
        }
    }

    pub fn shared(&self) -> SharedEngine {
        self.engine.clone()
    }

    /// 绑定任务 leader pid 来源。
    pub fn set_leader(&mut self, leader: Arc<AtomicI32>) {
        self.leader = Some(leader);
    }

    /// 快照冻结回调(冻结全部任务进程组,快照点即冻结点)。
    pub fn set_freeze(&self, freeze: mixum_snapshot::FreezeFn) {
        self.engine.lock().expect("快照引擎锁").freeze = Some(freeze);
    }

    fn to_wire(node: mixum_snapshot::TreeNode) -> SnapNode {
        SnapNode {
            id: node.id,
            parent: (!node.parent.is_empty()).then_some(node.parent),
            branch: node.branch,
            label: (!node.label.is_empty()).then_some(node.label),
            kind: node.kind,
            auto: node.auto,
            created_at: node.created_at,
            origin: (!node.origin.is_empty()).then_some(node.origin),
            size_bytes: node.size_bytes as i64,
            chunks: node.chunks as u32,
            kernel: (!node.kernel.is_empty()).then_some(node.kernel),
            arch: (!node.arch.is_empty()).then_some(node.arch),
            gui_sessions: node.gui_sessions,
            tags: node.tags,
        }
    }

    fn map_error(error: mixum_snapshot::EngineError) -> SnapshotError {
        use mixum_snapshot::{EngineError, TreeError};
        match error {
            EngineError::Tree(TreeError::NotFound(id))
            | EngineError::Tree(TreeError::ParentNotFound(id)) => SnapshotError::NotFound(id),
            EngineError::Tree(TreeError::HasChildren(_))
            | EngineError::Tree(TreeError::BranchExists(_)) => {
                SnapshotError::Conflict(error.to_string())
            }
            EngineError::Kind(_)
            | EngineError::MemoryOnly
            | EngineError::KernelMismatch { .. }
            | EngineError::ArchMismatch { .. } => SnapshotError::Invalid(error.to_string()),
            other => SnapshotError::Internal(other.to_string()),
        }
    }

    /// Tree 直接返回 TreeError(不经 Engine 包装),单独映射。
    fn map_tree_error(error: mixum_snapshot::TreeError) -> SnapshotError {
        use mixum_snapshot::TreeError;
        match error {
            TreeError::NotFound(id) | TreeError::ParentNotFound(id) => SnapshotError::NotFound(id),
            TreeError::HasChildren(_) | TreeError::BranchExists(_) => {
                SnapshotError::Conflict(error.to_string())
            }
            other => SnapshotError::Internal(other.to_string()),
        }
    }
}

#[cfg(feature = "real-snapshot")]
#[async_trait]
impl SnapshotEngine for RealSnapshotEngine {
    async fn annotate(
        &self,
        node: &str,
        tags: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), SnapshotError> {
        let engine = self.engine.lock().expect("快照引擎锁");
        engine
            .tree
            .set_tags(node, tags)
            .map(|_| ())
            .map_err(Self::map_tree_error)
    }

    async fn create(
        &self,
        kind: &str,
        label: Option<&str>,
        parent: Option<&str>,
        _branch: Option<&str>,
        auto: bool,
    ) -> Result<SnapNode, SnapshotError> {
        let mut engine = self.engine.lock().expect("快照引擎锁");
        if kind == "memory" || kind == "hybrid" {
            let pid = self
                .leader
                .as_ref()
                .map(|leader| leader.load(Ordering::SeqCst))
                .unwrap_or(0);
            engine.set_task_pid(pid);
            if pid > 0 {
                engine.set_dump_roots(std::iter::once(pid).chain(compositor_pids()));
            } else {
                engine.set_dump_roots(Vec::<i32>::new());
            }
        }
        let label = label.unwrap_or_default();
        let node = match parent.filter(|p| !p.is_empty()) {
            Some(parent) => engine.snapshot_on_branch(kind, label, auto, parent),
            None => engine.snapshot(kind, label, auto),
        };
        node.map(Self::to_wire).map_err(Self::map_error)
    }

    async fn restore(
        &self,
        node: &str,
        strategy: &str,
        label: Option<&str>,
    ) -> Result<RestoreResult, SnapshotError> {
        let (branch, selected) = {
            let mut engine = self.engine.lock().expect("快照引擎锁");
            engine
                .restore(node, strategy, label.unwrap_or_default())
                .map_err(Self::map_error)?
        };
        let origin = selected.origin.clone();
        let memory_restored = selected.memory.is_some();
        let sessions = selected.gui_sessions;
        if let Some(hook) = &self.restore_hook {
            hook(origin, memory_restored, sessions.clone());
        }
        Ok(RestoreResult {
            restored_to: node.to_owned(),
            new_branch: (strategy == "fork").then_some(branch),
            new_node: None,
            gui_sessions: sessions,
        })
    }

    async fn fork(&self, node: &str, branch: &str) -> Result<String, SnapshotError> {
        let engine = self.engine.lock().expect("快照引擎锁");
        engine.fork(node, branch).map_err(Self::map_error)
    }

    async fn tree(&self) -> Result<SnapTreeResponse, SnapshotError> {
        let engine = self.engine.lock().expect("快照引擎锁");
        let (branch, head) = engine.tree.head().map_err(Self::map_tree_error)?;
        let nodes = engine
            .tree
            .all()
            .map_err(Self::map_tree_error)?
            .into_iter()
            .map(Self::to_wire)
            .collect();
        Ok(SnapTreeResponse {
            head: head.map(|n| n.id).unwrap_or_default(),
            branch,
            nodes,
        })
    }

    async fn head(&self) -> Result<Option<String>, SnapshotError> {
        let engine = self.engine.lock().expect("快照引擎锁");
        let branch = engine.tree.current_branch().map_err(Self::map_tree_error)?;
        Ok(engine
            .tree
            .head_id(&branch)
            .map_err(Self::map_tree_error)?
            .filter(|id| !id.is_empty()))
    }

    async fn delete(&self, node: &str) -> Result<bool, SnapshotError> {
        let engine = self.engine.lock().expect("快照引擎锁");
        engine.tree.delete(node).map_err(Self::map_tree_error)?;
        Ok(true)
    }

    async fn gc(&self) -> Result<u32, SnapshotError> {
        let engine = self.engine.lock().expect("快照引擎锁");
        engine
            .gc()
            .map(|removed| removed as u32)
            .map_err(Self::map_error)
    }

    async fn export_mart(&self, node: &str, out: &Path) -> Result<u64, SnapshotError> {
        let engine = self.engine.lock().expect("快照引擎锁");
        mixum_mart::export_node(&engine, node, out)
            .map_err(|error| SnapshotError::Internal(error.to_string()))?;
        std::fs::metadata(out)
            .map(|meta| meta.len())
            .map_err(|error| SnapshotError::Internal(error.to_string()))
    }

    async fn import_mart(&self, file: &Path, origin: &str) -> Result<String, SnapshotError> {
        let mut engine = self.engine.lock().expect("快照引擎锁");
        let report = mixum_mart::import_mart(&mut engine, file, origin)
            .map_err(|error| SnapshotError::Internal(error.to_string()))?;
        Ok(report.node.id)
    }

    fn backend_name(&self) -> &str {
        self.engine.lock().expect("快照引擎锁").backend_name()
    }

    fn criu_available(&self) -> bool {
        self.engine
            .lock()
            .expect("快照引擎锁")
            .criu
            .as_ref()
            .is_some_and(|criu| criu.available())
    }
}

fn compositor_pids() -> Vec<i32> {
    #[cfg(target_os = "linux")]
    {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return out;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name.to_str().and_then(|value| value.parse::<i32>().ok()) else {
                continue;
            };
            let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else {
                continue;
            };
            if comm.trim() == "mixcomp" {
                out.push(pid);
            }
        }
        out.sort_unstable();
        out
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_engine_annotates_base_tags() {
        let engine = MemorySnapshotEngine::new();
        let node = engine
            .create("disk", Some("tagged"), None, None, false)
            .await
            .unwrap();
        let mut tags = std::collections::BTreeMap::new();
        tags.insert("base".into(), "node22:v1".into());
        tags.insert("baseDigest".into(), "sha256:abc".into());
        engine.annotate(&node.id, &tags).await.unwrap();
        let tree = engine.tree().await.unwrap();
        let found = tree.nodes.iter().find(|item| item.id == node.id).unwrap();
        assert_eq!(
            found.tags.get("base").map(String::as_str),
            Some("node22:v1")
        );
        assert_eq!(
            found.tags.get("baseDigest").map(String::as_str),
            Some("sha256:abc")
        );
    }

    #[tokio::test]
    async fn memory_engine_keeps_head_and_fork() {
        let engine = MemorySnapshotEngine::new();
        let first = engine
            .create("disk", Some("before"), None, None, true)
            .await
            .unwrap();
        let second = engine
            .create("disk", None, None, None, false)
            .await
            .unwrap();
        assert_eq!(engine.head().await.unwrap(), Some(second.id.clone()));
        assert_eq!(
            engine.fork(&first.id, "experiment").await.unwrap(),
            "experiment"
        );
        let restored = engine
            .restore(&first.id, "fork", Some("trial"))
            .await
            .unwrap();
        assert_eq!(restored.new_branch.as_deref(), Some("trial"));
        assert_eq!(engine.tree().await.unwrap().nodes.len(), 2);
    }

    #[cfg(feature = "real-snapshot")]
    mod real {
        use super::*;
        use mixum_snapshot::EngineOptions;

        fn open_engine(tag: &str) -> (tempfile::TempDir, RealSnapshotEngine) {
            let dir = tempfile::tempdir().unwrap();
            let engine = RealSnapshotEngine::open(EngineOptions {
                data_dir: dir.path().join("snapshots"),
                root: dir.path().join("work"),
                skip: vec![".mixum".into()],
                encrypt: false,
            })
            .unwrap_or_else(|e| panic!("{tag}: {e}"));
            (dir, engine)
        }

        #[tokio::test]
        async fn real_engine_round_trip_matches_reference_semantics() {
            let (_dir, engine) = open_engine("round-trip");
            assert_eq!(engine.backend_name(), "directory");
            let genesis = engine.head().await.unwrap().expect("genesis");
            let second = engine
                .create("disk", Some("after"), None, None, false)
                .await
                .unwrap();
            assert_eq!(second.parent.as_deref(), Some(genesis.as_str()));
            let tree = engine.tree().await.unwrap();
            assert_eq!(tree.nodes.len(), 2);
            assert_eq!(tree.head, second.id);
            let restored = engine
                .restore(&genesis, "fork", Some("trial"))
                .await
                .unwrap();
            assert_eq!(restored.restored_to, genesis);
            assert_eq!(restored.new_branch.as_deref(), Some("trial"));
            assert_eq!(engine.tree().await.unwrap().branch, "trial");
            engine.delete(&second.id).await.unwrap();
            assert_eq!(engine.tree().await.unwrap().nodes.len(), 1);
        }
    }

    #[cfg(feature = "real-snapshot")]
    #[cfg(unix)]
    #[tokio::test]
    async fn real_engine_uses_shared_leader_for_memory_snapshot() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::AtomicI32;

        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        let script = tools.path().join("criu");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut engine = mixum_snapshot::Engine::open(mixum_snapshot::EngineOptions {
            data_dir: data.path().into(),
            root: root.path().into(),
            skip: vec![],
            encrypt: false,
        })
        .unwrap();
        engine.criu = Some(mixum_snapshot::Criu::from_binary(script));
        let mut adapter = RealSnapshotEngine::from_engine(engine);
        let shared = adapter.shared();
        let leader = Arc::new(AtomicI32::new(4321));
        adapter.set_leader(Arc::clone(&leader));

        let node = adapter
            .create("memory", Some("mem"), None, None, false)
            .await
            .unwrap();
        let stored = shared.lock().unwrap().tree.get(&node.id).unwrap();
        assert!(stored.memory.is_some(), "memory 快照应带 memory 清单");
    }

    #[cfg(feature = "real-snapshot")]
    #[tokio::test]
    async fn restore_hook_receives_gui_sessions() {
        use std::sync::{Arc, Mutex as StdMutex};

        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut engine = mixum_snapshot::Engine::open(mixum_snapshot::EngineOptions {
            data_dir: data.path().into(),
            root: root.path().into(),
            skip: vec![],
            encrypt: false,
        })
        .unwrap();
        engine.metadata = Some(Box::new(|node| {
            node.gui_sessions.push(mixum_protocol::GuiSession {
                command: "/usr/bin/foot".into(),
                ..Default::default()
            });
        }));
        let mut adapter = RealSnapshotEngine::from_engine(engine);
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let hook_seen = seen.clone();
        adapter.set_restore_hook(Arc::new(move |origin, memory_restored, sessions| {
            assert!(origin.is_empty(), "本地快照 origin 应为空");
            assert!(!memory_restored, "disk 快照不应标记内存恢复");
            hook_seen.lock().unwrap().extend(sessions);
        }));

        let node = adapter
            .create("disk", Some("with-gui"), None, None, false)
            .await
            .unwrap();
        adapter.restore(&node.id, "inplace", None).await.unwrap();

        let sessions = seen.lock().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].command, "/usr/bin/foot");
    }
}
