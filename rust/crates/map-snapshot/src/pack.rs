//! Pack a directory tree into a `.mart` archive.
//!
//! Chunks are content-addressed (SHA-256) and deduplicated inside the archive.
//! `PackOptions::base` additionally reuses chunk payloads from an existing
//! `.mart`, which makes incremental snapshots cheap: only new bytes are
//! appended, the manifest carries the new tree.

use crate::archive::MartArchive;
use crate::error::{MartError, Result};
use crate::format::{sha256_hex, validate_rel_path, MartHeader, FORMAT_VERSION, HEADER_LEN};
use crate::manifest::{MartChunk, MartDir, MartFile, MartManifest, MANIFEST_VERSION};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Default chunk size: 1 MiB. Large enough to keep syscall overhead low while
/// still giving the dedup filter useful granularity for edited files.
pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct PackOptions {
    pub chunk_size: usize,
    /// Existing `.mart` whose chunks may be reused.
    pub base: Option<PathBuf>,
    /// Free-form origin label stored in the manifest.
    pub source: String,
}

impl Default for PackOptions {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            base: None,
            source: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackStats {
    pub files: u64,
    pub dirs: u64,
    /// Chunk references across all files.
    pub chunk_refs: u64,
    /// Chunks physically appended to this archive.
    pub chunks_stored: u64,
    /// Chunk references served from dedupe (in-archive or base archive).
    pub chunks_reused: u64,
    pub bytes: u64,
}

struct ArchiveIndex {
    archive: MartArchive,
    by_hash: HashMap<String, usize>,
}

impl ArchiveIndex {
    fn open(path: &Path) -> Result<Self> {
        let archive = MartArchive::open(path)?;
        // base 的 chunk 区不在 archive 摘要内；复用前先验每个 chunk。
        archive.verify()?;
        let mut by_hash = HashMap::new();
        {
            let manifest = archive.manifest();
            for (index, chunk) in manifest.chunks.iter().enumerate() {
                by_hash.insert(chunk.hash.as_str().to_owned(), index);
            }
        }
        Ok(Self { archive, by_hash })
    }

    fn lookup(&self, hash: &str) -> Option<&[u8]> {
        self.by_hash
            .get(hash)
            .and_then(|index| self.archive.chunk_bytes(*index).ok())
    }
}

/// Pack `root` into a new `.mart` at `out`.
pub fn pack(
    root: impl AsRef<Path>,
    out: impl AsRef<Path>,
    options: &PackOptions,
) -> Result<PackStats> {
    let root = root.as_ref();
    let metadata = fs::metadata(root)?;
    if !metadata.is_dir() {
        return Err(MartError::NotADirectory(root.display().to_string()));
    }
    let base = match &options.base {
        Some(path) => Some(ArchiveIndex::open(path)?),
        None => None,
    };

    let out = out.as_ref().to_path_buf();
    let tmp = out.with_extension("mart.tmp");
    let mut output = File::create(&tmp)?;
    output.write_all(&[0u8; HEADER_LEN as usize])?;
    let mut cursor = HEADER_LEN;

    let mut stats = PackStats::default();
    let mut dirs: Vec<MartDir> = Vec::new();
    let mut files: Vec<MartFile> = Vec::new();
    let mut chunks: Vec<MartChunk> = Vec::new();
    let mut seen: HashMap<String, u32> = HashMap::new();
    let chunk_size = options.chunk_size.max(1);
    let mut buffer = vec![0u8; chunk_size];

    for entry in WalkDir::new(root).follow_links(false).sort_by_file_name() {
        let entry =
            entry.map_err(|e| MartError::Io(std::io::Error::other(format!("walk: {e}"))))?;
        if entry.depth() == 0 {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| MartError::UnsafePath(entry.path().display().to_string()))?;
        // 非 UTF-8 文件名直接拒绝;分隔符只在 Windows 归一化为 `/`。
        let rel = rel
            .to_str()
            .ok_or_else(|| MartError::UnsafePath("non-UTF-8 path".into()))?;
        #[cfg(windows)]
        let rel = rel.replace('\\', "/");
        #[cfg(not(windows))]
        let rel = rel.to_owned();
        validate_rel_path(&rel)?;
        let file_type = entry.file_type();
        let link_meta = fs::symlink_metadata(entry.path())?;

        if file_type.is_dir() {
            dirs.push(MartDir {
                path: rel,
                mode: mode_of(&link_meta),
            });
            stats.dirs += 1;
            continue;
        }

        if file_type.is_symlink() {
            let target = fs::read_link(entry.path())?;
            files.push(MartFile {
                path: rel,
                mode: mode_of(&link_meta),
                size: 0,
                symlink: Some(target.to_string_lossy().into_owned()),
                chunks: Vec::new(),
            });
            stats.files += 1;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let size = link_meta.len();
        let mut file_chunks = Vec::new();
        let mut reader = BufReader::new(File::open(entry.path())?);
        let mut total = 0u64;
        loop {
            let mut filled = 0usize;
            while filled < buffer.len() {
                let read = reader.read(&mut buffer[filled..])?;
                if read == 0 {
                    break;
                }
                filled += read;
            }
            if filled == 0 {
                break;
            }
            total += filled as u64;
            stats.chunk_refs += 1;
            let hash = sha256_hex(&buffer[..filled]);
            let index = if let Some(index) = seen.get(&hash) {
                stats.chunks_reused += 1;
                *index
            } else if let Some(bytes) = base.as_ref().and_then(|base| base.lookup(&hash)) {
                // Reuse from the base archive: copy the payload so this file is
                // self-contained, but keep the plaintext hash invariant.
                if sha256_hex(bytes) != hash {
                    return Err(MartError::Corrupt(format!(
                        "base chunk {hash} does not match its hash"
                    )));
                }
                stats.chunks_reused += 1;
                stats.chunks_stored += 1;
                append_chunk(
                    &mut output,
                    &mut cursor,
                    &mut chunks,
                    &mut seen,
                    &hash,
                    bytes,
                )?
            } else {
                stats.chunks_stored += 1;
                append_chunk(
                    &mut output,
                    &mut cursor,
                    &mut chunks,
                    &mut seen,
                    &hash,
                    &buffer[..filled],
                )?
            };
            file_chunks.push(index);
        }
        if total != size {
            return Err(MartError::Corrupt(format!(
                "{}: read {total} bytes, metadata says {size}",
                entry.path().display()
            )));
        }
        files.push(MartFile {
            path: rel,
            mode: mode_of(&link_meta),
            size,
            symlink: None,
            chunks: file_chunks,
        });
        stats.files += 1;
    }

    let manifest = MartManifest {
        version: MANIFEST_VERSION,
        created_at: now_ms(),
        source: options.source.clone(),
        files,
        dirs,
        chunks,
    };
    // rkyv archives must start at an 8/16-byte aligned address. The mmap base
    // is page aligned, so aligning the file offset is enough.
    let unaligned = output.stream_position()?;
    let padding = (16 - (unaligned % 16)) % 16;
    if padding != 0 {
        output.write_all(&vec![0u8; padding as usize])?;
    }
    let archive = rkyv::to_bytes::<_, 1024>(&manifest)
        .map_err(|e| MartError::Corrupt(format!("manifest serialize: {e}")))?;
    let archive_off = output.stream_position()?;
    output.write_all(&archive)?;
    let file_len = output.stream_position()?;

    let header = MartHeader {
        version: FORMAT_VERSION,
        archive_off,
        archive_len: archive.len() as u64,
        chunk_off: HEADER_LEN,
        chunk_len: archive_off - HEADER_LEN,
        file_len,
        created_at: manifest.created_at,
        flags: 0,
        archive_sha256: sha256_bytes(&archive),
    };
    output.seek(SeekFrom::Start(0))?;
    output.write_all(&header.encode())?;
    output.flush()?;
    drop(output);
    if let Err(e) = std::fs::rename(&tmp, &out) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }

    stats.bytes = file_len;
    Ok(stats)
}

fn append_chunk(
    output: &mut File,
    cursor: &mut u64,
    chunks: &mut Vec<MartChunk>,
    seen: &mut HashMap<String, u32>,
    hash: &str,
    bytes: &[u8],
) -> Result<u32> {
    if chunks.len() >= u32::MAX as usize {
        return Err(MartError::Corrupt("too many chunks".into()));
    }
    let offset = *cursor;
    output.write_all(bytes)?;
    *cursor += bytes.len() as u64;
    let index = chunks.len() as u32;
    chunks.push(MartChunk {
        hash: hash.to_owned(),
        offset,
        len: bytes.len() as u64,
        raw_len: bytes.len() as u64,
    });
    seen.insert(hash.to_owned(), index);
    Ok(index)
}

fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn mode_of(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        if metadata.is_dir() {
            0o755
        } else {
            0o644
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}
