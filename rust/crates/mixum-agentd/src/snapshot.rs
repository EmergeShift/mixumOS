//! 快照宿主 trait 和可测试的内存实现。
//!
//! 真实 mixum-snapshot crate 接入前，agentd 只依赖这组最小异步契约；内存实现
//! 保留 DAG、HEAD 和分支语义，足够跑协议与任务前置快照测试。

use crate::wire::{SnapNode, SnapTreeResponse};
use async_trait::async_trait;
use std::collections::HashMap;
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
}

#[derive(Debug, Clone)]
pub struct RestoreResult {
    pub restored_to: String,
    pub new_branch: Option<String>,
    pub new_node: Option<String>,
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
    seq: u64,
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

    fn id(state: &mut State) -> String {
        state.seq += 1;
        format!("snap_{}_{}", mixum_protocol::envelope::new_id(), state.seq)
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
            id: Self::id(&mut state),
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
        };
        state.branch = branch;
        state.head = node.id.clone();
        state.nodes.insert(node.id.clone(), node.clone());
        Ok(node)
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
        if state.head == node {
            return Err(SnapshotError::Conflict("HEAD 不能删除".into()));
        }
        if state
            .nodes
            .values()
            .any(|n| n.parent.as_deref() == Some(node))
        {
            return Err(SnapshotError::Conflict("仅叶子节点可删除".into()));
        }
        Ok(state.nodes.remove(node).is_some())
    }

    async fn gc(&self) -> Result<u32, SnapshotError> {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
