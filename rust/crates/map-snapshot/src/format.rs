//! On-disk `.mart` header and small byte helpers.
//!
//! Layout (all integers little-endian):
//!
//! ```text
//! 0        magic "MART"
//! 4        format version u32
//! 8..128   fixed header (see `MartHeader`)
//! 128      chunk payload region, chunk_off == 128
//! ...      rkyv archive (MartManifest)
//! EOF
//! ```
//!
//! The manifest archive is hashed and the digest stored in the header, so a
//! reader validates the archive before touching rkyv — memory-mapping is only
//! ever used on bytes whose integrity was checked.

use crate::error::{MartError, Result};

pub const MAGIC: &[u8; 4] = b"MART";
pub const FORMAT_VERSION: u32 = 1;
pub const HEADER_LEN: u64 = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MartHeader {
    pub version: u32,
    pub archive_off: u64,
    pub archive_len: u64,
    pub chunk_off: u64,
    pub chunk_len: u64,
    pub file_len: u64,
    pub created_at: i64,
    pub flags: u64,
    /// SHA-256 of the rkyv archive bytes.
    pub archive_sha256: [u8; 32],
}

impl MartHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN as usize] {
        let mut b = [0u8; HEADER_LEN as usize];
        b[0..4].copy_from_slice(MAGIC);
        b[4..8].copy_from_slice(&self.version.to_le_bytes());
        b[8..16].copy_from_slice(&self.archive_off.to_le_bytes());
        b[16..24].copy_from_slice(&self.archive_len.to_le_bytes());
        b[24..32].copy_from_slice(&self.chunk_off.to_le_bytes());
        b[32..40].copy_from_slice(&self.chunk_len.to_le_bytes());
        b[40..48].copy_from_slice(&self.file_len.to_le_bytes());
        b[48..56].copy_from_slice(&self.created_at.to_le_bytes());
        b[56..64].copy_from_slice(&self.flags.to_le_bytes());
        b[64..96].copy_from_slice(&self.archive_sha256);
        b
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN as usize {
            return Err(MartError::Corrupt("file shorter than header".into()));
        }
        if &bytes[0..4] != MAGIC {
            return Err(MartError::BadMagic);
        }
        let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
        let i64_at = |o: usize| i64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
        let mut sha = [0u8; 32];
        sha.copy_from_slice(&bytes[64..96]);
        Ok(Self {
            version: u32_at(4),
            archive_off: u64_at(8),
            archive_len: u64_at(16),
            chunk_off: u64_at(24),
            chunk_len: u64_at(32),
            file_len: u64_at(40),
            created_at: i64_at(48),
            flags: u64_at(56),
            archive_sha256: sha,
        })
    }

    /// Reject headers whose regions do not fit in `file_len`.
    pub fn validate(&self, actual_len: u64) -> Result<()> {
        if self.version != FORMAT_VERSION {
            return Err(MartError::Unsupported(self.version));
        }
        if self.flags != 0 {
            return Err(MartError::Corrupt(
                "reserved header flags must be zero".into(),
            ));
        }
        if self.chunk_off != HEADER_LEN {
            return Err(MartError::Corrupt(format!(
                "chunk_off {} (expected {HEADER_LEN})",
                self.chunk_off
            )));
        }
        let chunk_end = self
            .chunk_off
            .checked_add(self.chunk_len)
            .ok_or_else(|| MartError::Corrupt("chunk region overflow".into()))?;
        let archive_end = self
            .archive_off
            .checked_add(self.archive_len)
            .ok_or_else(|| MartError::Corrupt("archive region overflow".into()))?;
        if chunk_end > actual_len || archive_end > actual_len {
            return Err(MartError::Corrupt("region exceeds file length".into()));
        }
        if self.archive_off < chunk_end {
            return Err(MartError::Corrupt("archive overlaps chunk region".into()));
        }
        if !self.archive_off.is_multiple_of(16) {
            return Err(MartError::Corrupt(
                "archive offset is not 16-byte aligned".into(),
            ));
        }
        if archive_end != self.file_len || self.file_len != actual_len {
            return Err(MartError::Corrupt(
                "file length disagrees with header".into(),
            ));
        }
        Ok(())
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Validate a manifest path: relative, no `..`, no root prefix.
pub fn validate_rel_path(path: &str) -> Result<std::path::PathBuf> {
    use std::path::{Component, Path};
    if path.is_empty() {
        return Err(MartError::UnsafePath("empty path".into()));
    }
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err(MartError::UnsafePath(path.into()));
    }
    for component in candidate.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(MartError::UnsafePath(path.into())),
        }
    }
    Ok(candidate.to_path_buf())
}
