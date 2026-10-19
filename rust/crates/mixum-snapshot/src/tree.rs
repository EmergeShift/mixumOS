use crate::Manifest;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

pub const KIND_DISK: &str = "disk";
pub const KIND_MEMORY: &str = "memory";
pub const KIND_HYBRID: &str = "hybrid";

#[derive(Debug, Error)]
pub enum TreeError {
    #[error("sled error: {0}")]
    Sled(#[from] sled::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("snapshot node not found: {0}")]
    NotFound(String),
    #[error("snapshot parent not found: {0}")]
    ParentNotFound(String),
    #[error("node {0} has children and cannot be deleted")]
    HasChildren(String),
    #[error("branch already exists: {0}")]
    BranchExists(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TreeNode {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent: String,
    pub branch: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto: bool,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<Manifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<Manifest>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kernel: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub arch: String,
    #[serde(rename = "sizeBytes", default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub chunks: usize,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
}

impl TreeNode {
    pub fn new(
        id: impl Into<String>,
        parent: impl Into<String>,
        branch: impl Into<String>,
        kind: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            parent: parent.into(),
            branch: branch.into(),
            label: String::new(),
            kind: kind.into(),
            auto: false,
            created_at: now_ms(),
            origin: String::new(),
            disk: None,
            memory: None,
            kernel: String::new(),
            arch: String::new(),
            size_bytes: 0,
            chunks: 0,
            tags: BTreeMap::new(),
        }
    }
}

pub struct Tree {
    db: sled::Db,
    nodes: sled::Tree,
    heads: sled::Tree,
    meta: sled::Tree,
}

impl Tree {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, TreeError> {
        std::fs::create_dir_all(dir.as_ref()).map_err(sled::Error::from)?;
        // sled 的文件锁在 Db drop 后由后台线程异步释放,同进程紧接着重开会
        // 撞锁(WouldBlock)。短暂重试等待释放,避免正常的重启路径偶发失败。
        let dir = dir.as_ref();
        let mut db = None;
        for _ in 0..20 {
            match sled::open(dir) {
                Ok(opened) => {
                    db = Some(opened);
                    break;
                }
                Err(sled::Error::Io(e)) if e.to_string().contains("could not acquire lock") => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(e.into()),
            }
        }
        let db = db.ok_or(sled::Error::Io(std::io::Error::other("could not acquire sled lock after 20 retries")))?;
        let nodes = db.open_tree("nodes")?;
        let heads = db.open_tree("heads")?;
        let meta = db.open_tree("meta")?;
        Ok(Self {
            db,
            nodes,
            heads,
            meta,
        })
    }
    pub fn flush(&self) -> Result<(), TreeError> {
        self.db.flush()?;
        Ok(())
    }

    pub fn add(&self, mut node: TreeNode) -> Result<TreeNode, TreeError> {
        if node.id.is_empty() {
            node.id = mixum_protocol::envelope::new_id();
        }
        if node.created_at == 0 {
            node.created_at = now_ms();
        }
        if node.branch.is_empty() {
            node.branch = "main".into();
        }
        if node.kind.is_empty() {
            node.kind = KIND_DISK.into();
        }
        if !node.parent.is_empty() && self.nodes.get(node.parent.as_bytes())?.is_none() {
            return Err(TreeError::ParentNotFound(node.parent));
        }
        if self.nodes.get(node.id.as_bytes())?.is_some() {
            return Err(TreeError::BranchExists(node.id));
        }
        self.nodes
            .insert(node.id.as_bytes(), serde_json::to_vec(&node)?)?;
        self.heads
            .insert(node.branch.as_bytes(), node.id.as_bytes())?;
        self.db.flush()?;
        Ok(node)
    }

    pub fn get(&self, id: &str) -> Result<TreeNode, TreeError> {
        self.nodes
            .get(id.as_bytes())?
            .map(|v| serde_json::from_slice(&v))
            .transpose()?
            .ok_or_else(|| TreeError::NotFound(id.into()))
    }

    pub fn all(&self) -> Result<Vec<TreeNode>, TreeError> {
        self.nodes
            .iter()
            .map(|r| {
                let (_, v) = r?;
                Ok(serde_json::from_slice(&v)?)
            })
            .collect()
    }

    pub fn children(&self, id: &str) -> Result<Vec<TreeNode>, TreeError> {
        let mut out: Vec<_> = self.all()?.into_iter().filter(|n| n.parent == id).collect();
        out.sort_by_key(|n| n.created_at);
        Ok(out)
    }

    pub fn current_branch(&self) -> Result<String, TreeError> {
        Ok(self
            .meta
            .get("currentBranch")?
            .map(|x| String::from_utf8_lossy(&x).into_owned())
            .unwrap_or_else(|| "main".into()))
    }
    pub fn set_current_branch(&self, branch: &str) -> Result<(), TreeError> {
        self.meta.insert("currentBranch", branch.as_bytes())?;
        self.db.flush()?;
        Ok(())
    }
    pub fn head(&self) -> Result<(String, Option<TreeNode>), TreeError> {
        let branch = self.current_branch()?;
        let node = self.head_id(&branch)?.map(|id| self.get(&id)).transpose()?;
        Ok((branch, node))
    }
    pub fn head_id(&self, branch: &str) -> Result<Option<String>, TreeError> {
        Ok(self
            .heads
            .get(branch.as_bytes())?
            .map(|v| String::from_utf8_lossy(&v).into_owned()))
    }
    pub fn set_head(&self, branch: &str, id: &str) -> Result<(), TreeError> {
        self.get(id)?;
        self.heads.insert(branch.as_bytes(), id.as_bytes())?;
        self.db.flush()?;
        Ok(())
    }
    pub fn branches(&self) -> Result<BTreeMap<String, String>, TreeError> {
        self.heads
            .iter()
            .map(|r| {
                let (k, v) = r?;
                Ok((
                    String::from_utf8_lossy(&k).into_owned(),
                    String::from_utf8_lossy(&v).into_owned(),
                ))
            })
            .collect()
    }

    pub fn fork_branch(&self, id: &str, want: &str) -> Result<String, TreeError> {
        self.get(id)?;
        let existing = self.branches()?;
        let mut name: String = if want.is_empty() {
            "fork".into()
        } else {
            want.into()
        };
        let base = name.clone();
        let mut n = 2;
        while existing.contains_key(&name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        self.set_head(&name, id)?;
        Ok(name)
    }

    pub fn rename_branch(&self, old: &str, want: &str) -> Result<String, TreeError> {
        let id = self
            .head_id(old)?
            .ok_or_else(|| TreeError::NotFound(old.into()))?;
        if old == want {
            return Ok(old.into());
        }
        let name = self.fork_branch(&id, want)?;
        // 重写该分支所有节点的 branch 字段:否则旧节点 inplace 恢复时
        // 会按记录里的旧名把已改名的分支复活。
        for mut node in self.all()? {
            if node.branch == old {
                node.branch = name.clone();
                self.nodes
                    .insert(node.id.as_bytes(), serde_json::to_vec(&node)?)?;
            }
        }
        self.heads.remove(old)?;
        self.db.flush()?;
        if self.current_branch()? == old {
            self.set_current_branch(&name)?;
        }
        Ok(name)
    }

    pub fn delete(&self, id: &str) -> Result<(), TreeError> {
        let node = self.get(id)?;
        if !self.children(id)?.is_empty() {
            return Err(TreeError::HasChildren(id.into()));
        }
        self.nodes.remove(id.as_bytes())?;
        // 指向该节点的分支头移交其父节点,分支不因删除而失头
        // (根节点没有父节点,相应分支条目随之移除)。
        let mut affected: Vec<String> = Vec::new();
        for item in self.heads.iter() {
            let (k, v) = item?;
            if v.as_ref() == id.as_bytes() {
                affected.push(String::from_utf8_lossy(&k).into_owned());
            }
        }
        for branch in affected {
            if node.parent.is_empty() {
                self.heads.remove(branch.as_bytes())?;
            } else {
                self.heads
                    .insert(branch.as_bytes(), node.parent.as_bytes())?;
            }
        }
        self.db.flush()?;
        Ok(())
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
