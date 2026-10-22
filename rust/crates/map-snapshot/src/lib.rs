//! M-ART snapshots (`.mart`) for MAP.
//!
//! A `.mart` file is a memory-mappable snapshot:
//!
//! * a fixed 128-byte header (magic, regions, archive digest),
//! * a raw chunk region of content-addressed (SHA-256) payloads,
//! * an rkyv archive of [`MartManifest`] describing dirs, files and chunks.
//!
//! [`MartArchive::open`] validates the digest before interpreting the rkyv
//! root, then all reads (verify, restore, diff) work directly off the mapping.
//!
//! ```no_run
//! use map_snapshot::{pack, PackOptions, MartArchive};
//!
//! let stats = pack("/workspace", "/tmp/ws.mart", &PackOptions::default())?;
//! let archive = MartArchive::open("/tmp/ws.mart")?;
//! assert!(archive.verify()?.ok);
//! archive.restore("/tmp/restored")?;
//! # Ok::<(), map_snapshot::MartError>(())
//! ```

pub mod archive;
pub mod error;
pub mod format;
pub mod manifest;
pub mod pack;

pub use archive::{
    safe_join, DiffEntry, DiffKind, DiffReport, MartArchive, RestoreStats, VerifyReport,
};
pub use error::{MartError, Result};
pub use format::{sha256_hex, validate_rel_path, MartHeader, FORMAT_VERSION, HEADER_LEN};
pub use manifest::{MartChunk, MartDir, MartFile, MartManifest, MANIFEST_VERSION};
pub use pack::{pack, PackOptions, PackStats, DEFAULT_CHUNK_SIZE};
