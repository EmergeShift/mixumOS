use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};
use thiserror::Error;

pub const CHUNK_SIZE: usize = 1 << 20;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid chunk hash: {0}")]
    InvalidHash(String),
    #[error("chunk integrity check failed for {0}")]
    Integrity(String),
    #[error("chunk decryption failed")]
    Decryption,
    #[error("compression error: {0}")]
    Compression(String),
}

pub fn hash_bytes(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub struct Store {
    dir: PathBuf,
    store_id: String,
    cipher: Option<Aes256Gcm>,
    marks: Mutex<std::collections::HashSet<String>>,
}

impl Store {
    pub fn open(
        dir: impl AsRef<Path>,
        master_key: Option<&[u8]>,
        store_id: impl Into<String>,
    ) -> Result<Self, StoreError> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(dir.join("objects"))?;
        let store_id = store_id.into();
        let cipher = match master_key {
            Some(key) => {
                let hk =
                    Hkdf::<Sha256>::new(Some(format!("mixum/snap/v1:{store_id}").as_bytes()), key);
                let mut derived = [0u8; 32];
                hk.expand(b"chunk-aead", &mut derived)
                    .map_err(|_| StoreError::Compression("HKDF expansion failed".into()))?;
                Some(
                    Aes256Gcm::new_from_slice(&derived)
                        .map_err(|_| StoreError::Compression("invalid AES key".into()))?,
                )
            }
            None => None,
        };
        Ok(Self {
            dir,
            store_id,
            cipher,
            marks: Mutex::new(Default::default()),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn encrypted(&self) -> bool {
        self.cipher.is_some()
    }

    fn path(&self, hash: &str) -> Result<PathBuf, StoreError> {
        validate_hash(hash)?;
        Ok(self.dir.join("objects").join(&hash[..2]).join(&hash[2..]))
    }

    pub fn has(&self, hash: &str) -> bool {
        self.path(hash).map(|p| p.is_file()).unwrap_or(false)
    }

    pub fn put(&self, plain: &[u8]) -> Result<String, StoreError> {
        let hash = hash_bytes(plain);
        let path = self.path(&hash)?;
        if path.is_file() {
            return Ok(hash);
        }
        let compressed =
            zstd::bulk::compress(plain, 1).map_err(|e| StoreError::Compression(e.to_string()))?;
        let blob = if let Some(cipher) = &self.cipher {
            let mut nonce = [0u8; 12];
            rand::thread_rng().fill_bytes(&mut nonce);
            let aad = format!("mixum-chunk:{}:{hash}", self.store_id);
            let mut out = nonce.to_vec();
            out.extend(
                cipher
                    .encrypt(
                        Nonce::from_slice(&nonce),
                        aes_gcm::aead::Payload {
                            msg: &compressed,
                            aad: aad.as_bytes(),
                        },
                    )
                    .map_err(|_| StoreError::Decryption)?,
            );
            out
        } else {
            compressed
        };
        fs::create_dir_all(path.parent().expect("object path has parent"))?;
        let mut suffix = [0u8; 8];
        rand::thread_rng().fill_bytes(&mut suffix);
        let tmp = path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            u64::from_ne_bytes(suffix)
        ));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        use std::io::Write;
        file.write_all(&blob)?;
        file.sync_all()?;
        drop(file);
        match fs::rename(&tmp, &path) {
            Ok(()) => Ok(hash),
            Err(_e) if path.is_file() => {
                let _ = fs::remove_file(tmp);
                Ok(hash)
            }
            Err(e) => {
                let _ = fs::remove_file(tmp);
                Err(StoreError::Io(e))
            }
        }
    }

    pub fn put_hashed(&self, hash: &str, plain: &[u8]) -> Result<(), StoreError> {
        if hash_bytes(plain) != hash {
            return Err(StoreError::Integrity(hash.into()));
        }
        self.put(plain).map(|_| ())
    }

    pub fn get(&self, hash: &str) -> Result<Vec<u8>, StoreError> {
        let path = self.path(hash)?;
        let mut blob = fs::read(path)?;
        if let Some(cipher) = &self.cipher {
            if blob.len() < 12 {
                return Err(StoreError::Decryption);
            }
            let nonce = blob[..12].to_vec();
            let aad = format!("mixum-chunk:{}:{hash}", self.store_id);
            blob = cipher
                .decrypt(
                    Nonce::from_slice(&nonce),
                    aes_gcm::aead::Payload {
                        msg: &blob[12..],
                        aad: aad.as_bytes(),
                    },
                )
                .map_err(|_| StoreError::Decryption)?;
        }
        let plain = zstd::stream::decode_all(blob.as_slice())
            .map_err(|e| StoreError::Compression(e.to_string()))?;
        if hash_bytes(&plain) != hash {
            return Err(StoreError::Integrity(hash.into()));
        }
        Ok(plain)
    }

    pub fn count(&self) -> usize {
        fs::read_dir(self.dir.join("objects"))
            .map(|levels| {
                levels
                    .filter_map(Result::ok)
                    .filter(|e| e.path().is_dir())
                    .map(|level| {
                        fs::read_dir(level.path())
                            .map(|x| x.filter_map(Result::ok).count())
                            .unwrap_or(0)
                    })
                    .sum()
            })
            .unwrap_or(0)
    }

    pub fn mark(&self, hash: impl Into<String>) {
        if let Ok(mut marks) = self.marks.lock() {
            marks.insert(hash.into());
        }
    }

    pub fn clear_marks(&self) {
        if let Ok(mut marks) = self.marks.lock() {
            marks.clear();
        }
    }

    /// Sweep 未标记块。`protect` 窗口内的块(按 mtime)不删——
    /// 防止进行中的推送/导入刚写入的块被并发 GC 清掉(协议 §6 竞态)。
    pub fn sweep(&self, protect: std::time::Duration) -> Result<usize, StoreError> {
        let marks = std::mem::take(&mut *self.marks.lock().expect("store marks mutex poisoned"));
        let now = std::time::SystemTime::now();
        let mut removed = 0;
        for level in fs::read_dir(self.dir.join("objects"))?
            .filter_map(Result::ok)
            .filter(|e| e.path().is_dir())
        {
            for entry in fs::read_dir(level.path())?.filter_map(Result::ok) {
                let hash = format!(
                    "{}{}",
                    level.file_name().to_string_lossy(),
                    entry.file_name().to_string_lossy()
                );
                if !marks.contains(&hash) {
                    if !protect.is_zero() {
                        let age = entry
                            .metadata()?
                            .modified()
                            .ok()
                            .and_then(|m| now.duration_since(m).ok())
                            .unwrap_or_default();
                        if age < protect {
                            continue;
                        }
                    }
                    fs::remove_file(entry.path())?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

fn validate_hash(hash: &str) -> Result<(), StoreError> {
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(StoreError::InvalidHash(hash.into()));
    }
    Ok(())
}
