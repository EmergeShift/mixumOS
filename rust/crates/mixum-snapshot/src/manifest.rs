use crate::{Store, StoreError, CHUNK_SIZE};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
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
}

fn is_zero(value: &u64) -> bool {
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
        let root = root.as_ref();
        let mut manifest = Self::empty();
        let mut stats = PackStats::default();
        walk(root, root, store, skip, &mut manifest, &mut stats)?;
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
                out.sync_all()?;
                drop(out);
                set_mode(&tmp, f.mode & 0o777)?;
                fs::rename(tmp, target)?;
            }
        }
        directory_modes.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
        for (path, mode) in directory_modes {
            set_mode(&path, mode)?;
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
            });
            stats.files += 1;
        } else if meta.is_dir() {
            m.dirs.push(DirEntry { path: rel, mode });
            walk(root, &path, store, skip, m, stats)?;
        } else if meta.is_file() {
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
                chunks.push(store.put(&buf)?);
                size += n as u64;
                stats.chunks += 1;
            }
            m.files.push(FileEntry {
                path: rel,
                mode,
                size: size as i64,
                symlink: String::new(),
                chunks,
                dev: 0,
                ino: 0,
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
            });
            stats.files += 1;
        }
    }
    Ok(())
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
