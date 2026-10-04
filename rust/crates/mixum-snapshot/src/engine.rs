use crate::criu::kernel_release;
use crate::{
    diff_manifests, Criu, CriuError, Manifest, Store, StoreError, Tree, TreeError, TreeNode,
    KIND_DISK, KIND_HYBRID, KIND_MEMORY,
};
use rand::RngCore;
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use thiserror::Error;

pub type FreezeFn =
    Box<dyn FnMut(&mut dyn FnMut() -> Result<(), EngineError>) -> Result<(), EngineError> + Send>;
pub type DiffResult = (Vec<String>, Vec<String>, Vec<String>, usize);

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    Manifest(#[from] crate::manifest::ManifestError),
    #[error(transparent)]
    Criu(#[from] CriuError),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid snapshot kind: {0}")]
    Kind(String),
    #[error("memory snapshot cannot be restored without disk data")]
    MemoryOnly,
    #[error("snapshot kernel mismatch: {snapshot} vs {current}")]
    KernelMismatch { snapshot: String, current: String },
    #[error("snapshot architecture mismatch: {snapshot} vs {current}")]
    ArchMismatch { snapshot: String, current: String },
    #[error("freeze hook failed: {0}")]
    Freeze(String),
}

#[derive(Debug, Clone)]
pub struct EngineOptions {
    pub data_dir: PathBuf,
    pub root: PathBuf,
    pub skip: Vec<String>,
    pub encrypt: bool,
}

pub struct Engine {
    pub store: Store,
    pub tree: Tree,
    pub criu: Option<Criu>,
    pub root: PathBuf,
    pub skip: Vec<String>,
    pub freeze: Option<FreezeFn>,
    task_pid: Option<i32>,
    operation_lock: Arc<Mutex<()>>,
}

impl Engine {
    pub fn open(opts: EngineOptions) -> Result<Self, EngineError> {
        fs::create_dir_all(&opts.data_dir)?;
        fs::create_dir_all(&opts.root)?;
        let master = load_or_create_key(&opts.data_dir)?;
        let key = opts.encrypt.then_some(master.as_slice());
        let store = Store::open(
            opts.data_dir.join("store"),
            key,
            format!(
                "mixum-{}",
                opts.data_dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("store")
            ),
        )?;
        let tree = Tree::open(opts.data_dir.join("tree"))?;
        let mut engine = Self {
            store,
            tree,
            criu: Criu::probe(),
            root: opts.root,
            skip: opts.skip,
            freeze: None,
            task_pid: None,
            operation_lock: Arc::new(Mutex::new(())),
        };
        if engine.tree.head()?.1.is_none() {
            engine.snapshot_inner(KIND_DISK, "genesis", true, None)?;
        }
        Ok(engine)
    }
    pub fn set_task_pid(&mut self, pid: i32) {
        self.task_pid = Some(pid);
    }
    pub fn snapshot(
        &mut self,
        kind: &str,
        label: &str,
        auto: bool,
    ) -> Result<TreeNode, EngineError> {
        self.snapshot_inner(kind, label, auto, None)
    }
    pub fn snapshot_on_branch(
        &mut self,
        kind: &str,
        label: &str,
        auto: bool,
        parent: &str,
    ) -> Result<TreeNode, EngineError> {
        self.snapshot_inner(kind, label, auto, Some(parent))
    }
    fn snapshot_inner(
        &mut self,
        kind: &str,
        label: &str,
        auto: bool,
        explicit_parent: Option<&str>,
    ) -> Result<TreeNode, EngineError> {
        if ![KIND_DISK, KIND_MEMORY, KIND_HYBRID].contains(&kind) {
            return Err(EngineError::Kind(kind.into()));
        }
        let lock = Arc::clone(&self.operation_lock);
        let _guard = lock
            .lock()
            .map_err(|_| EngineError::Freeze("engine lock poisoned".into()))?;
        let (mut branch, head) = self.tree.head()?;
        let parent = if let Some(id) = explicit_parent {
            self.tree.get(id)?
        } else {
            head.clone().unwrap_or_else(|| {
                TreeNode::new(String::new(), String::new(), branch.clone(), KIND_DISK)
            })
        };
        if explicit_parent.is_some() {
            branch = self.tree.current_branch()?;
        }
        let mut node = TreeNode::new(String::new(), parent.id.clone(), branch, kind);
        node.label = label.into();
        node.auto = auto;
        node.kernel = kernel_release();
        node.arch = std::env::consts::ARCH.into();
        let do_disk = kind == KIND_DISK || kind == KIND_HYBRID;
        let do_mem = kind == KIND_MEMORY || kind == KIND_HYBRID;
        // pid=0(无任务运行或驱动未注册首领进程)时退化为 disk:
        // CRIU 不接受 --tree 0;纯 memory 快照仍要求 CRIU 与 pid 就绪。
        if kind == KIND_MEMORY && (self.criu.is_none() || self.task_pid.is_none()) {
            return Err(CriuError::Unavailable.into());
        }
        let mut action = || {
            if do_disk {
                let (m, stats) = Manifest::pack_directory(&self.root, &self.store, &self.skip)?;
                node.size_bytes += stats.bytes_logical;
                node.chunks += m.chunks().len();
                node.disk = Some(m);
            }
            if do_mem && self.criu.is_some() && self.task_pid.map(|p| p > 0) == Some(true) {
                let dir = std::env::temp_dir().join(format!("mixum-criu-{}", node.id));
                self.criu.as_ref().ok_or(CriuError::Unavailable)?.dump(
                    self.task_pid.unwrap_or(0),
                    &dir,
                    false,
                )?;
                let (m, stats) = Manifest::pack_directory(&dir, &self.store, &[])?;
                node.size_bytes += stats.bytes_logical;
                node.chunks += m.chunks().len();
                node.memory = Some(m);
                let _ = fs::remove_dir_all(dir);
            }
            Ok(())
        };
        if let Some(freeze) = self.freeze.as_mut() {
            freeze(&mut action)?;
        } else {
            action()?;
        }
        if let Some(m) = &node.disk {
            for h in m.chunks() {
                self.store.mark(h);
            }
        }
        if let Some(m) = &node.memory {
            for h in m.chunks() {
                self.store.mark(h);
            }
        }
        self.tree.add(node).map_err(Into::into)
    }
    pub fn restore(
        &mut self,
        id: &str,
        strategy: &str,
        label: &str,
    ) -> Result<(String, TreeNode), EngineError> {
        let lock = Arc::clone(&self.operation_lock);
        let _guard = lock
            .lock()
            .map_err(|_| EngineError::Freeze("engine lock poisoned".into()))?;
        let node = self.tree.get(id)?;
        if node.kind == KIND_MEMORY || node.disk.is_none() {
            return Err(EngineError::MemoryOnly);
        }
        if !node.kernel.is_empty() && node.kernel != kernel_release() {
            return Err(EngineError::KernelMismatch {
                snapshot: node.kernel.clone(),
                current: kernel_release(),
            });
        }
        if !node.arch.is_empty() && node.arch != std::env::consts::ARCH {
            return Err(EngineError::ArchMismatch {
                snapshot: node.arch.clone(),
                current: std::env::consts::ARCH.into(),
            });
        }
        self.clear_root()?;
        node.disk
            .as_ref()
            .expect("checked above")
            .materialize(&self.store, &self.root)?;
        if strategy == "fork" {
            let default_label = format!("{}-fork", node.branch);
            let want = if label.is_empty() {
                &default_label
            } else {
                label
            };
            let name = self.tree.fork_branch(id, want)?;
            self.tree.set_current_branch(&name)?;
            Ok((name, node))
        } else {
            self.tree.set_head(&node.branch, id)?;
            self.tree.set_current_branch(&node.branch)?;
            Ok((node.branch.clone(), node))
        }
    }
    pub fn fork(&self, id: &str, want: &str) -> Result<String, EngineError> {
        let _guard = self
            .operation_lock
            .lock()
            .map_err(|_| EngineError::Freeze("engine lock poisoned".into()))?;
        Ok(self.tree.fork_branch(id, want)?)
    }
    pub fn diff(&self, from: &str, to: &str) -> Result<DiffResult, EngineError> {
        let from = self.tree.get(from)?;
        let to = self.tree.get(to)?;
        let from_manifest = from.disk.as_ref().ok_or(EngineError::MemoryOnly)?;
        let to_manifest = to.disk.as_ref().ok_or(EngineError::MemoryOnly)?;
        Ok(diff_manifests(from_manifest, to_manifest))
    }
    pub fn backend_name(&self) -> &'static str {
        "directory"
    }
    pub fn gc(&self) -> Result<usize, EngineError> {
        let _guard = self
            .operation_lock
            .lock()
            .map_err(|_| EngineError::Freeze("engine lock poisoned".into()))?;
        self.store.clear_marks();
        let mut marks = std::collections::HashSet::new();
        for n in self.tree.all()? {
            if let Some(m) = n.disk {
                marks.extend(m.chunks());
            }
            if let Some(m) = n.memory {
                marks.extend(m.chunks());
            }
        }
        for h in marks {
            self.store.mark(h);
        }
        Ok(self.store.sweep()?)
    }
    fn clear_root(&self) -> Result<(), EngineError> {
        for item in fs::read_dir(&self.root)? {
            let e = item?;
            let name = e.file_name().to_string_lossy().into_owned();
            if self
                .skip
                .iter()
                .any(|s| name == *s || name.starts_with(&format!("{s}/")))
            {
                continue;
            }
            if e.file_type()?.is_dir() {
                fs::remove_dir_all(e.path())?;
            } else {
                fs::remove_file(e.path())?;
            }
        }
        Ok(())
    }
}

fn load_or_create_key(dir: &Path) -> Result<Vec<u8>, io::Error> {
    let path = dir.join("master.key");
    if let Ok(key) = fs::read(&path) {
        if key.len() == 32 {
            return Ok(key);
        }
    }
    let mut key = vec![0u8; 32];
    rand::thread_rng().fill_bytes(&mut key);
    fs::write(&path, &key)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(key)
}
