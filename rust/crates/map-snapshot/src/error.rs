//! Errors for the `.mart` snapshot format.

#[derive(Debug, thiserror::Error)]
pub enum MartError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a .mart archive (bad magic)")]
    BadMagic,
    #[error("unsupported .mart version {0}")]
    Unsupported(u32),
    #[error("corrupt .mart: {0}")]
    Corrupt(String),
    #[error("unsafe path in manifest: {0}")]
    UnsafePath(String),
    #[error("manifest path is not a directory: {0}")]
    NotADirectory(String),
}

pub type Result<T> = std::result::Result<T, MartError>;
