use crate::{store::sync_dir, Store, StoreError, CHUNK_SIZE};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};
use thiserror::Error;

pub const GO_MODE_SYMLINK: u32 = 1 << 27;
pub const GO_MODE_DIR: u32 = 1 << 31;
const GO_MODE_NAMED_PIPE: u32 = 1 << 25;
const GO_MODE_SOCKET: u32 = 1 << 24;
const GO_MODE_DEVICE: u32 = 1 << 26;
const GO_MODE_CHAR_DEVICE: u32 = 1 << 21;
const GO_MODE_IRREGULAR: u32 = 1 << 19;
const GO_MODE_TYPE: u32 = GO_MODE_DIR
    | GO_MODE_SYMLINK
    | GO_MODE_NAMED_PIPE
    | GO_MODE_SOCKET
    | GO_MODE_DEVICE
    | GO_MODE_CHAR_DEVICE
    | GO_MODE_IRREGULAR;

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("unsafe manifest path: {0}")]
    UnsafePath(String),
    #[error("manifest format error: {0}")]
    Format(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub mode: u32,
    #[serde(default)]
    pub size: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub symlink: String,
    #[serde(default)]
    pub chunks: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dev: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ino: u64,
    /// 纳秒级 mtime，只用于复用判定（`dev`/`ino` 同理）。
    ///
    /// 0 表示"未知"：老 manifest 和 Go 参考实现产出的清单都没有这个字段，
    /// 这类条目一律当作不可复用，宁可多读一遍也不能错用旧块。
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub mtime: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirEntry {
    pub path: String,
    pub mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub version: u32,
    pub files: Vec<FileEntry>,
    pub dirs: Vec<DirEntry>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PackStats {
    pub files: usize,
    pub chunks: usize,
    pub bytes_logical: u64,
    /// 复用上一条 manifest 的条目数：这些文件只做了 `stat`，没有读内容。
    pub reused_files: usize,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_zero_i64(value: &i64) -> bool {
    *value == 0
}

impl Manifest {
    pub fn empty() -> Self {
        Self {
            version: 1,
            files: Vec::new(),
            dirs: Vec::new(),
        }
    }

    pub fn pack_directory(
        root: impl AsRef<Path>,
        store: &Store,
        skip: &[String],
    ) -> Result<(Self, PackStats), ManifestError> {
        Self::pack_directory_reusing(root, store, skip, None)
    }

    /// 打包目录，并尽量复用 `previous` 里未变化文件的块列表。
    ///
    /// 无变更快照原来要把每个文件完整读一遍再逐块哈希，50 MiB 工作区光这一
    /// 步就是几十毫秒。`dev`/`ino`/`size`/`mtime` 全对上时内容不可能变（和
    /// restic 同一套假设），直接沿用旧块列表，这一轮只剩 `stat`。
    ///
    /// 复用是**保守**的：任一条件不满足、或旧块已被 GC 掉，就退回读文件。
    pub fn pack_directory_reusing(
        root: impl AsRef<Path>,
        store: &Store,
        skip: &[String],
        previous: Option<&Manifest>,
    ) -> Result<(Self, PackStats), ManifestError> {
        let root = root.as_ref();
        let mut manifest = Self::empty();
        let mut stats = PackStats::default();
        let index: HashMap<&str, &FileEntry> = previous
            .map(|m| m.files.iter().map(|f| (f.path.as_str(), f)).collect())
            .unwrap_or_default();
        walk(root, root, store, skip, &mut manifest, &mut stats, &index)?;
        // 块是 `put_deferred` 写的（逐块不 fsync），这里一次落盘。
        store.sync()?;
        manifest.files.sort_by(|a, b| a.path.cmp(&b.path));
        manifest.dirs.sort_by(|a, b| a.path.cmp(&b.path));
        Ok((manifest, stats))
    }

    pub fn materialize(
        &self,
        store: &Store,
        dst: impl AsRef<Path>,
    ) -> Result<usize, ManifestError> {
        let mut paths = BTreeMap::new();
        for d in &self.dirs {
            paths.insert(validate_rel_path(&d.path)?, false);
        }
        for f in &self.files {
            let path = validate_rel_path(&f.path)?;
            if paths.insert(path, f.mode & GO_MODE_SYMLINK != 0).is_some() {
                return Err(ManifestError::Format(format!("duplicate path: {}", f.path)));
            }
        }
        for (path, is_symlink) in &paths {
            if *is_symlink {
                continue;
            }
            let mut parent = PathBuf::new();
            let components: Vec<_> = path.components().collect();
            for component in components.iter().take(components.len().saturating_sub(1)) {
                parent.push(component.as_os_str());
                if paths.get(&parent) == Some(&true) {
                    return Err(ManifestError::UnsafePath(path.display().to_string()));
                }
            }
        }
        let dst = dst.as_ref();
        let mut directory_modes = Vec::new();
        for d in &self.dirs {
            let p = ensure_directory(dst, &d.path)?;
            directory_modes.push((p, d.mode & 0o777));
        }
        let mut count = 0;
        for f in &self.files {
            let target = dst.join(&f.path);
            if f.mode & GO_MODE_SYMLINK != 0 {
                ensure_parent_dirs(dst, &f.path)?;
                match fs::remove_file(&target) {
                    Ok(()) => (),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                    Err(e) => return Err(e.into()),
                }
                create_symlink(&f.symlink, &target)?;
            } else if f.mode & GO_MODE_DIR != 0 {
                let dir = ensure_directory(dst, &f.path)?;
                directory_modes.push((dir, f.mode & 0o777));
            } else if f.mode & GO_MODE_TYPE == 0 {
                ensure_parent_dirs(dst, &f.path)?;
                let tmp = target.with_extension("mixum-restore");
                match fs::symlink_metadata(&target) {
                    Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
                        fs::remove_file(&target)?
                    }
                    Ok(_) => (),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                    Err(e) => return Err(e.into()),
                }
                match fs::remove_file(&tmp) {
                    Ok(()) => (),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                    Err(e) => return Err(e.into()),
                }
                let mut out = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)?;
                for hash in &f.chunks {
                    out.write_all(&store.get(hash)?)?;
                    count += 1;
                }
                sync_file(&out)?;
                drop(out);
                set_mode(&tmp, f.mode & 0o777)?;
                fs::rename(tmp, target)?;
            }
        }
        directory_modes.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
        for (path, mode) in directory_modes {
            set_mode(&path, mode)?;
        }
        // 一万个文件逐个 fsync 要二十多秒；整批写完刷一次就够。
        if dst.is_dir() {
            sync_dir(dst)?;
        }
        Ok(count)
    }

    pub fn chunks(&self) -> Vec<String> {
        self.files
            .iter()
            .flat_map(|f| f.chunks.iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn size(&self) -> u64 {
        self.files.iter().map(|f| f.size.max(0) as u64).sum()
    }
}

fn walk(
    root: &Path,
    dir: &Path,
    store: &Store,
    skip: &[String],
    m: &mut Manifest,
    stats: &mut PackStats,
    index: &HashMap<&str, &FileEntry>,
) -> Result<(), ManifestError> {
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|e| ManifestError::Format(e.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        if skip
            .iter()
            .any(|s| rel == *s || rel.strip_prefix(s).is_some_and(|r| r.starts_with('/')))
        {
            continue;
        }
        let meta = fs::symlink_metadata(&path)?;
        let mode = mode_from_meta(&meta);
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&path)?.to_string_lossy().into_owned();
            m.files.push(FileEntry {
                path: rel,
                mode: GO_MODE_SYMLINK | 0o777,
                size: 0,
                symlink: target,
                chunks: Vec::new(),
                dev: 0,
                ino: 0,
                mtime: 0,
            });
            stats.files += 1;
        } else if meta.is_dir() {
            m.dirs.push(DirEntry { path: rel, mode });
            walk(root, &path, store, skip, m, stats, index)?;
        } else if meta.is_file() {
            let stat = FileStat::of(&meta);
            if let Some(entry) = index.get(rel.as_str()) {
                if let Some(chunks) = reusable_chunks(entry, &stat, store) {
                    m.files.push(FileEntry {
                        path: rel,
                        mode,
                        size: stat.size as i64,
                        symlink: String::new(),
                        chunks,
                        dev: stat.dev,
                        ino: stat.ino,
                        mtime: stat.mtime,
                    });
                    stats.files += 1;
                    stats.reused_files += 1;
                    stats.bytes_logical += stat.size;
                    continue;
                }
            }
            let mut file = fs::File::open(&path)?;
            let mut chunks = Vec::new();
            let mut size = 0u64;
            loop {
                let mut buf = vec![0; CHUNK_SIZE];
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                buf.truncate(n);
                chunks.push(store.put_deferred(&buf)?);
                size += n as u64;
                stats.chunks += 1;
            }
            m.files.push(FileEntry {
                path: rel,
                mode,
                size: size as i64,
                symlink: String::new(),
                chunks,
                dev: stat.dev,
                ino: stat.ino,
                mtime: stat.mtime,
            });
            stats.files += 1;
            stats.bytes_logical += size;
        } else {
            m.files.push(FileEntry {
                path: rel,
                mode,
                size: 0,
                symlink: String::new(),
                chunks: Vec::new(),
                dev: 0,
                ino: 0,
                mtime: 0,
            });
            stats.files += 1;
        }
    }
    Ok(())
}

/// 复用判定要用的元数据快照。
struct FileStat {
    size: u64,
    mtime: i64,
    dev: u64,
    ino: u64,
}

impl FileStat {
    fn of(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                size: meta.len(),
                mtime: mtime_nanos(meta),
                dev: meta.dev(),
                ino: meta.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                size: meta.len(),
                mtime: mtime_nanos(meta),
                dev: 0,
                ino: 0,
            }
        }
    }
}

fn mtime_nanos(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|delta| delta.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// 旧条目能不能直接沿用。任一条件不满足就返回 `None`，调用方退回读文件。
///
/// 判定沿用 restic 的假设：`ino`+`dev`+`size`+`mtime` 都没变，内容就没变。
/// 反例是"改完内容再把 mtime 改回去"，这种只有真读一遍才看得出来——代价是
/// 每次快照都退化成全量读取，不划算。
fn reusable_chunks(entry: &FileEntry, stat: &FileStat, store: &Store) -> Option<Vec<String>> {
    // 只复用普通文件：符号链接、管道、设备节点没有内容。
    if entry.mode & GO_MODE_TYPE != 0 {
        return None;
    }
    if entry.mtime == 0 || entry.mtime != stat.mtime {
        return None;
    }
    if entry.size != stat.size as i64 {
        return None;
    }
    if entry.ino != 0 && entry.ino != stat.ino {
        return None;
    }
    if entry.dev != 0 && entry.dev != stat.dev {
        return None;
    }
    // 上一条 manifest 所属的节点可能已经被裁剪，块对象不在了。这时沿用旧块
    // 会让恢复拿不到数据，只能重读。
    if !entry.chunks.iter().all(|hash| store.has(hash)) {
        return None;
    }
    Some(entry.chunks.clone())
}

/// 单文件落盘策略：Linux 上推迟到 `sync_dir` 一次完成，其他平台当场 fsync。
#[cfg(target_os = "linux")]
fn sync_file(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn sync_file(file: &fs::File) -> io::Result<()> {
    file.sync_all()
}

#[cfg(unix)]
fn mode_from_meta(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    let ty = if meta.file_type().is_symlink() {
        GO_MODE_SYMLINK
    } else {
        0
    };
    ty | (meta.permissions().mode() & 0o777)
}
#[cfg(not(unix))]
fn mode_from_meta(_meta: &fs::Metadata) -> u32 {
    0o644
}

fn validate_rel_path(value: &str) -> Result<PathBuf, ManifestError> {
    let p = Path::new(value);
    if value.is_empty()
        || p.is_absolute()
        || p.components().any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(ManifestError::UnsafePath(value.into()));
    }
    Ok(p.to_path_buf())
}

fn ensure_parent_dirs(root: &Path, relative: &str) -> Result<PathBuf, ManifestError> {
    let rel = validate_rel_path(relative)?;
    let mut parent = root.to_path_buf();
    if let Some(p) = rel.parent() {
        parent.push(p);
    }
    let mut current = root.to_path_buf();
    if let Some(p) = rel.parent() {
        for component in p.components() {
            current.push(component.as_os_str());
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                    return Err(ManifestError::UnsafePath(relative.into()))
                }
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(root.join(rel))
}

fn ensure_directory(root: &Path, relative: &str) -> Result<PathBuf, ManifestError> {
    let target = ensure_parent_dirs(root, relative)?;
    match fs::symlink_metadata(&target) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(ManifestError::UnsafePath(relative.into()))
        }
        Ok(_) => (),
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&target)?,
        Err(e) => return Err(e.into()),
    }
    Ok(target)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}
#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &str, path: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}
#[cfg(windows)]
fn create_symlink(target: &str, path: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, path)
}
#[cfg(not(any(unix, windows)))]
fn create_symlink(_target: &str, _path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symlinks unsupported",
    ))
}

pub fn diff_manifests(
    from: &Manifest,
    to: &Manifest,
) -> (Vec<String>, Vec<String>, Vec<String>, usize) {
    let old: BTreeMap<_, _> = from.files.iter().map(|f| (&f.path, f)).collect();
    let new: BTreeMap<_, _> = to.files.iter().map(|f| (&f.path, f)).collect();
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut chunks = BTreeSet::new();
    for (path, file) in &new {
        match old.get(path) {
            None => {
                added.push((*path).clone());
                chunks.extend(file.chunks.iter().cloned());
            }
            Some(prev) if prev.chunks != file.chunks => {
                changed.push((*path).clone());
                chunks.extend(file.chunks.iter().cloned());
            }
            _ => (),
        }
    }
    for path in old.keys() {
        if !new.contains_key(path) {
            removed.push((*path).clone());
        }
    }
    (added, removed, changed, chunks.len())
}
