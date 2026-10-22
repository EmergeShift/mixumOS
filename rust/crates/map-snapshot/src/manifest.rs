//! `.mart` manifest: the rkyv-archived root of a M-ART snapshot.
//!
//! The manifest is serialized with rkyv so opening an archive is a memory map
//! plus a header check — no allocation and no JSON parsing. Chunk payloads are
//! addressed by plaintext SHA-256 (MAP snapshot rule) and stored raw so they
//! can be restored zero-copy from the mapping.

/// One content-addressed chunk. `offset` is the absolute byte offset of the
/// chunk payload inside the `.mart` file, so a reader can slice the mmap
/// directly without a second index.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone)]
#[archive(check_bytes)]
pub struct MartChunk {
    /// Lowercase hex SHA-256 of the plaintext chunk bytes.
    pub hash: String,
    /// Absolute offset inside the `.mart` file.
    pub offset: u64,
    /// Stored (plaintext) length.
    pub len: u64,
    /// Uncompressed length; equals `len` for v1 (compression is orthogonal).
    pub raw_len: u64,
}

/// One file entry. `chunks` indexes `MartManifest::chunks`.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone)]
#[archive(check_bytes)]
pub struct MartFile {
    /// Relative path with `/` separators; never absolute, never contains `..`.
    pub path: String,
    /// Octal permission bits (420 = 0o644).
    pub mode: u32,
    pub size: u64,
    /// Symlink target, when this entry is a symlink (chunks are then empty).
    pub symlink: Option<String>,
    pub chunks: Vec<u32>,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone)]
#[archive(check_bytes)]
pub struct MartDir {
    pub path: String,
    pub mode: u32,
}

/// Root archive payload. Everything the reader needs is reachable from here;
/// chunk bytes stay in place in the mapping.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone)]
#[archive(check_bytes)]
pub struct MartManifest {
    /// `.mart` format version (independent of the MAP protocol version).
    pub version: u32,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Free-form origin label (host, task id, ...).
    pub source: String,
    pub files: Vec<MartFile>,
    pub dirs: Vec<MartDir>,
    pub chunks: Vec<MartChunk>,
}

pub const MANIFEST_VERSION: u32 = 1;
