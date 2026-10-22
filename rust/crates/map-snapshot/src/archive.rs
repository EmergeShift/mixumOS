//! Read, verify, restore and diff `.mart` archives.
//!
//! Opening memory-maps the file, validates the header and the archive digest,
//! then interprets the rkyv root in place. Chunk payloads are slices of the
//! mapping, so restore writes straight from page cache without copying the
//! snapshot into a second in-memory representation.

use crate::error::{MartError, Result};
use crate::format::{sha256_hex, validate_rel_path, MartHeader, HEADER_LEN};
use crate::manifest::{
    ArchivedMartChunk, ArchivedMartFile, ArchivedMartManifest, MartManifest, MANIFEST_VERSION,
};
use memmap2::Mmap;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct MartArchive {
    mmap: Mmap,
    header: MartHeader,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyReport {
    pub version: u32,
    pub files: u64,
    pub dirs: u64,
    pub chunks: u64,
    pub bytes: u64,
    pub ok: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreStats {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    Added,
    Removed,
    Modified,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffEntry {
    pub path: String,
    pub kind: DiffKind,
    pub from_size: u64,
    pub to_size: u64,
    pub chunks_added: u64,
    pub chunks_removed: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffReport {
    pub files_added: Vec<String>,
    pub files_removed: Vec<String>,
    pub files_modified: Vec<DiffEntry>,
}

impl MartArchive {
    /// Memory-map and validate an archive. Integrity is checked before rkyv is
    /// allowed to interpret any bytes.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        let header = MartHeader::decode(&mmap)?;
        header.validate(mmap.len() as u64)?;
        let archive =
            &mmap[header.archive_off as usize..(header.archive_off + header.archive_len) as usize];
        if sha256_bytes(archive) != header.archive_sha256 {
            return Err(MartError::Corrupt("archive digest mismatch".into()));
        }
        // 先做 rkyv 结构校验,任何 archived 访问之前;摘要只证明字节未被改动,
        // 不证明字节是合法 rkyv。
        rkyv::check_archived_root::<MartManifest>(archive)
            .map_err(|e| MartError::Corrupt(format!("invalid rkyv archive: {e}")))?;
        let this = Self { mmap, header };
        this.check_manifest()?;
        if this.manifest().version != MANIFEST_VERSION {
            return Err(MartError::Unsupported(this.manifest().version));
        }
        Ok(this)
    }

    pub fn header(&self) -> &MartHeader {
        &self.header
    }

    pub fn manifest(&self) -> &ArchivedMartManifest {
        let bytes = self.archive_bytes();
        // SAFETY: `open` validated the header, the full archive digest and the
        // structural invariants below before exposing this type.
        unsafe { rkyv::archived_root::<MartManifest>(bytes) }
    }

    fn archive_bytes(&self) -> &[u8] {
        let start = self.header.archive_off as usize;
        let end = start + self.header.archive_len as usize;
        &self.mmap[start..end]
    }

    /// Validate chunk bounds, file size sums and manifest paths.
    pub fn check_manifest(&self) -> Result<()> {
        let manifest = self.manifest();
        if manifest.chunks.len() > u32::MAX as usize {
            return Err(MartError::Corrupt("too many chunks".into()));
        }
        let mut paths = std::collections::HashSet::new();
        for dir in manifest.dirs.iter() {
            validate_rel_path(dir.path.as_str())?;
            if dir.mode > 0o777 {
                return Err(MartError::Corrupt(format!(
                    "{}: mode {:#o} exceeds 0o777",
                    dir.path.as_str(),
                    dir.mode
                )));
            }
            if !paths.insert(dir.path.as_str().to_owned()) {
                return Err(MartError::Corrupt(format!(
                    "duplicate manifest path: {}",
                    dir.path.as_str()
                )));
            }
        }
        for file in manifest.files.iter() {
            validate_rel_path(file.path.as_str())?;
            if file.mode > 0o777 {
                return Err(MartError::Corrupt(format!(
                    "{}: mode {:#o} exceeds 0o777",
                    file.path.as_str(),
                    file.mode
                )));
            }
            if !paths.insert(file.path.as_str().to_owned()) {
                return Err(MartError::Corrupt(format!(
                    "duplicate manifest path: {}",
                    file.path.as_str()
                )));
            }
            let mut total = 0u64;
            for index in file.chunks.iter() {
                let chunk = manifest.chunks.get(*index as usize).ok_or_else(|| {
                    MartError::Corrupt(format!("chunk index {index} out of range"))
                })?;
                total = total
                    .checked_add(chunk.len)
                    .ok_or_else(|| MartError::Corrupt("file chunk sum overflow".into()))?;
            }
            if file.symlink.is_none() && total != file.size {
                return Err(MartError::Corrupt(format!(
                    "{}: chunks sum to {total}, manifest size {}",
                    file.path.as_str(),
                    file.size
                )));
            }
        }
        let chunk_region_end = self.header.chunk_off + self.header.chunk_len;
        for (index, chunk) in manifest.chunks.iter().enumerate() {
            let start = chunk.offset;
            let end = start
                .checked_add(chunk.len)
                .ok_or_else(|| MartError::Corrupt("chunk offset overflow".into()))?;
            if end > chunk_region_end {
                return Err(MartError::Corrupt(format!(
                    "chunk {index} ({}) lies outside the chunk region",
                    chunk.hash.as_str()
                )));
            }
            if start < HEADER_LEN {
                return Err(MartError::Corrupt(format!(
                    "chunk {index} overlaps the header"
                )));
            }
        }
        Ok(())
    }

    pub fn chunk_bytes(&self, index: usize) -> Result<&[u8]> {
        let manifest = self.manifest();
        let chunk: &ArchivedMartChunk = manifest
            .chunks
            .get(index)
            .ok_or_else(|| MartError::Corrupt(format!("chunk index {index} out of range")))?;
        let start = chunk.offset as usize;
        let end = start
            .checked_add(chunk.len as usize)
            .ok_or_else(|| MartError::Corrupt("chunk length overflow".into()))?;
        self.mmap
            .get(start..end)
            .ok_or_else(|| MartError::Corrupt("chunk slice out of bounds".into()))
    }

    /// Recompute every chunk digest and verify structural invariants.
    pub fn verify(&self) -> Result<VerifyReport> {
        let manifest = self.manifest();
        let mut bytes = 0u64;
        for (index, chunk) in manifest.chunks.iter().enumerate() {
            let payload = self.chunk_bytes(index)?;
            if sha256_hex(payload) != chunk.hash.as_str() {
                return Err(MartError::Corrupt(format!(
                    "chunk {index} digest mismatch ({}), archive is damaged",
                    chunk.hash.as_str()
                )));
            }
            bytes += payload.len() as u64;
        }
        self.check_manifest()?;
        Ok(VerifyReport {
            version: manifest.version,
            files: manifest.files.len() as u64,
            dirs: manifest.dirs.len() as u64,
            chunks: manifest.chunks.len() as u64,
            bytes,
            ok: true,
        })
    }

    /// Restore the archived tree into `dest` (created if missing).
    pub fn restore(&self, dest: impl AsRef<Path>) -> Result<RestoreStats> {
        let dest = dest.as_ref();
        fs::create_dir_all(dest)?;
        if fs::symlink_metadata(dest)?.file_type().is_symlink() {
            return Err(MartError::Corrupt("restore dest is a symlink".into()));
        }
        // restore 是内容真正被使用的点:默认先全量校验 chunk 摘要与结构。
        self.verify()?;
        let manifest = self.manifest();
        let mut stats = RestoreStats::default();

        for dir in manifest.dirs.iter() {
            let relative = validate_rel_path(dir.path.as_str())?;
            let target = dest.join(&relative);
            create_dir_no_symlink(dest, &relative)?;
            set_mode(&target, dir.mode)?;
            stats.dirs += 1;
        }

        for file in manifest.files.iter() {
            let relative = validate_rel_path(file.path.as_str())?;
            let target = dest.join(&relative);
            if let Some(parent) = relative.parent() {
                if !parent.as_os_str().is_empty() {
                    create_dir_no_symlink(dest, parent)?;
                }
            }
            if let Some(link) = file.symlink.as_ref() {
                validate_symlink_target(&relative, link.as_str())?;
                create_symlink(link.as_str(), &target)?;
                stats.files += 1;
                continue;
            }
            let mut output = open_nofollow(&target)?;
            for index in file.chunks.iter() {
                let payload = self.chunk_bytes(*index as usize)?;
                output.write_all(payload)?;
                stats.bytes += payload.len() as u64;
            }
            output.flush()?;
            drop(output);
            set_mode(&target, file.mode)?;
            stats.files += 1;
        }
        Ok(stats)
    }

    /// MAP snapshot-manifest JSON (`schemas/snapshot-manifest.schema.json`).
    pub fn to_snapshot_json(&self) -> serde_json::Value {
        let manifest = self.manifest();
        let files: Vec<serde_json::Value> = manifest
            .files
            .iter()
            .map(|file| file_json(manifest, file))
            .collect();
        let dirs: Vec<serde_json::Value> = manifest
            .dirs
            .iter()
            .map(|dir| serde_json::json!({ "path": dir.path.as_str(), "mode": dir.mode }))
            .collect();
        serde_json::json!({ "version": 1, "files": files, "dirs": dirs })
    }

    /// Full inspection document: archive metadata + MAP manifest.
    pub fn inspect_json(&self) -> serde_json::Value {
        let manifest = self.manifest();
        let mut chunk_bytes = 0u64;
        for (index, _) in manifest.chunks.iter().enumerate() {
            chunk_bytes += self.chunk_bytes(index).map(|b| b.len() as u64).unwrap_or(0);
        }
        serde_json::json!({
            "mart": {
                "version": self.header.version,
                "createdAt": self.header.created_at,
                "source": manifest.source.as_str(),
                "fileBytes": self.header.file_len,
                "chunkBytes": chunk_bytes,
                "chunks": manifest.chunks.len(),
                "files": manifest.files.len(),
                "dirs": manifest.dirs.len(),
            },
            "manifest": self.to_snapshot_json(),
        })
    }

    /// Structural diff against another archive. Compare by path, mode, size and
    /// chunk digests (content).
    pub fn diff(&self, other: &MartArchive) -> DiffReport {
        let from = fingerprints(self.manifest());
        let to = fingerprints(other.manifest());
        let mut report = DiffReport::default();
        for (path, old) in from.iter() {
            match to.get(path) {
                None => report.files_removed.push(path.clone()),
                Some(new) if new != old => {
                    let old_chunks: BTreeSet<&String> = old.chunks.iter().collect();
                    let new_chunks: BTreeSet<&String> = new.chunks.iter().collect();
                    report.files_modified.push(DiffEntry {
                        path: path.clone(),
                        kind: DiffKind::Modified,
                        from_size: old.size,
                        to_size: new.size,
                        chunks_added: new_chunks.difference(&old_chunks).count() as u64,
                        chunks_removed: old_chunks.difference(&new_chunks).count() as u64,
                    });
                }
                Some(_) => {}
            }
        }
        for path in to.keys() {
            if !from.contains_key(path) {
                report.files_added.push(path.clone());
            }
        }
        report
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    mode: u32,
    size: u64,
    symlink: Option<String>,
    chunks: Vec<String>,
}

fn fingerprints(manifest: &ArchivedMartManifest) -> BTreeMap<String, Fingerprint> {
    let mut map = BTreeMap::new();
    for file in manifest.files.iter() {
        let chunks = file
            .chunks
            .iter()
            .map(|index| {
                manifest
                    .chunks
                    .get(*index as usize)
                    .map(|chunk| chunk.hash.as_str().to_owned())
                    .unwrap_or_default()
            })
            .collect();
        map.insert(
            file.path.as_str().to_owned(),
            Fingerprint {
                mode: file.mode,
                size: file.size,
                symlink: file.symlink.as_ref().map(|s| s.as_str().to_owned()),
                chunks,
            },
        );
    }
    map
}

fn file_json(manifest: &ArchivedMartManifest, file: &ArchivedMartFile) -> serde_json::Value {
    let chunks: Vec<String> = file
        .chunks
        .iter()
        .filter_map(|index| {
            manifest
                .chunks
                .get(*index as usize)
                .map(|chunk| chunk.hash.as_str().to_owned())
        })
        .collect();
    let mut value = serde_json::json!({
        "path": file.path.as_str(),
        "mode": file.mode,
        "size": file.size,
        "chunks": chunks,
    });
    if let Some(link) = file.symlink.as_ref() {
        value["symlink"] = serde_json::json!(link.as_str());
    }
    value
}

fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// 逐级创建目录,拒绝穿过符号链接(包括 dest 内已存在的 symlink)。
fn create_dir_no_symlink(dest: &Path, relative: &Path) -> Result<()> {
    let mut current = dest.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(MartError::Corrupt(format!(
                    "refusing to traverse symlink: {}",
                    current.display()
                )));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(MartError::NotADirectory(current.display().to_string()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// symlink target 策略:v1 只允许相对路径且不含 `..`/绝对路径。
fn validate_symlink_target(link: &Path, target: &str) -> Result<()> {
    use std::path::Component;
    let path = Path::new(target);
    if path.is_absolute() || path.has_root() {
        return Err(MartError::UnsafePath(format!(
            "{}: absolute symlink target {target}",
            link.display()
        )));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(MartError::UnsafePath(format!(
            "{}: symlink target escapes: {target}",
            link.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn open_nofollow(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(MartError::from)
}

#[cfg(not(unix))]
fn open_nofollow(path: &Path) -> Result<File> {
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(MartError::Corrupt(format!(
            "refusing to overwrite symlink: {}",
            path.display()
        )));
    }
    File::create(path).map_err(MartError::from)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = fs::Permissions::from_mode(mode & 0o777);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &str, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_symlink(_target: &str, link: &Path) -> Result<()> {
    Err(MartError::Corrupt(format!(
        "symlink restore is not supported on this platform: {}",
        link.display()
    )))
}

/// Re-export for callers that only need to know it is a path inside `dest`.
pub fn safe_join(dest: &Path, relative: &str) -> Result<PathBuf> {
    Ok(dest.join(validate_rel_path(relative)?))
}

#[cfg(test)]
mod safety_tests {
    use super::*;

    #[test]
    fn symlink_targets_cannot_escape() {
        assert!(validate_symlink_target(Path::new("a/link"), "target").is_ok());
        assert!(validate_symlink_target(Path::new("a/link"), "../outside").is_err());
        assert!(validate_symlink_target(Path::new("a/link"), "/etc/passwd").is_err());
    }
}
