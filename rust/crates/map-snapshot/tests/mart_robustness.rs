//! Negative tests for `.mart` archive robustness:
//! - Malformed / malicious headers (bad magic, version, flags, overflow, alignment, digest mismatch)
//! - Corrupted chunk payloads and boundary violations
//! - Path traversal and symlink escapes (relative .., absolute targets)
//! - Symlink destination overwrite prevention

use map_snapshot::{pack, validate_rel_path, MartArchive, MartError, PackOptions, HEADER_LEN};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn create_valid_archive(dir: &Path, filename: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let src = dir.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("hello.txt"), b"hello world").unwrap();
    fs::create_dir_all(src.join("subdir")).unwrap();
    fs::write(src.join("subdir/nested.txt"), b"nested content").unwrap();

    let mart_path = dir.join(filename);
    pack(&src, &mart_path, &PackOptions::default()).expect("packing valid mart");
    (src, mart_path)
}

fn open_err(path: &Path) -> MartError {
    match MartArchive::open(path) {
        Err(e) => e,
        Ok(_) => panic!("expected open to fail for {}", path.display()),
    }
}

#[test]
fn reject_short_file() {
    let td = tempdir().unwrap();
    let short_file = td.path().join("short.mart");
    fs::write(&short_file, b"MART1234").unwrap();

    let err = open_err(&short_file);
    assert!(
        matches!(&err, MartError::Corrupt(msg) if msg.contains("file shorter than header")),
        "expected file shorter than header, got {err:?}"
    );
}

#[test]
fn reject_bad_magic() {
    let td = tempdir().unwrap();
    let (_, mart_path) = create_valid_archive(td.path(), "test.mart");
    let mut bytes = fs::read(&mart_path).unwrap();
    bytes[0..4].copy_from_slice(b"BADM");
    fs::write(&mart_path, bytes).unwrap();

    let err = open_err(&mart_path);
    assert!(matches!(&err, MartError::BadMagic), "expected BadMagic, got {err:?}");
}

#[test]
fn reject_unsupported_version() {
    let td = tempdir().unwrap();
    let (_, mart_path) = create_valid_archive(td.path(), "test.mart");
    let mut bytes = fs::read(&mart_path).unwrap();
    let unsupported_version: u32 = 999;
    bytes[4..8].copy_from_slice(&unsupported_version.to_le_bytes());
    fs::write(&mart_path, bytes).unwrap();

    let err = open_err(&mart_path);
    assert!(
        matches!(&err, MartError::Unsupported(999)),
        "expected Unsupported(999), got {err:?}"
    );
}

#[test]
fn reject_non_zero_flags() {
    let td = tempdir().unwrap();
    let (_, mart_path) = create_valid_archive(td.path(), "test.mart");
    let mut bytes = fs::read(&mart_path).unwrap();
    let flags: u64 = 0x01;
    bytes[56..64].copy_from_slice(&flags.to_le_bytes());
    fs::write(&mart_path, bytes).unwrap();

    let err = open_err(&mart_path);
    assert!(
        matches!(&err, MartError::Corrupt(msg) if msg.contains("reserved header flags must be zero")),
        "expected reserved header flags error, got {err:?}"
    );
}

#[test]
fn reject_tampered_header_digest() {
    let td = tempdir().unwrap();
    let (_, mart_path) = create_valid_archive(td.path(), "test.mart");
    let mut bytes = fs::read(&mart_path).unwrap();
    // Invert a byte in archive SHA-256 (offset 64..96)
    bytes[64] ^= 0xff;
    fs::write(&mart_path, bytes).unwrap();

    let err = open_err(&mart_path);
    assert!(
        matches!(&err, MartError::Corrupt(msg) if msg.contains("archive digest mismatch")),
        "expected archive digest mismatch, got {err:?}"
    );
}

#[test]
fn reject_tampered_archive_body() {
    let td = tempdir().unwrap();
    let (_, mart_path) = create_valid_archive(td.path(), "test.mart");
    let mut bytes = fs::read(&mart_path).unwrap();
    // Tamper with the very last byte of the file (inside the rkyv archive)
    let last = bytes.len() - 1;
    bytes[last] ^= 0xaa;
    fs::write(&mart_path, bytes).unwrap();

    let err = open_err(&mart_path);
    assert!(
        matches!(&err, MartError::Corrupt(msg) if msg.contains("archive digest mismatch")),
        "expected archive digest mismatch, got {err:?}"
    );
}

#[test]
fn reject_corrupted_chunk_payload() {
    let td = tempdir().unwrap();
    let (_, mart_path) = create_valid_archive(td.path(), "test.mart");
    let archive = MartArchive::open(&mart_path).expect("original archive should open");
    let report = archive.verify().expect("original archive should verify");
    assert!(report.ok);
    drop(archive);

    // Corrupt a byte in the chunk payload region (offset 128..128+N)
    let mut bytes = fs::read(&mart_path).unwrap();
    bytes[HEADER_LEN as usize] ^= 0x55;

    // Notice: archive SHA-256 remains valid because only chunk bytes changed,
    // so open() succeeds, but verify() must detect chunk digest mismatch!
    fs::write(&mart_path, &bytes).unwrap();
    let archive = MartArchive::open(&mart_path).expect("open should succeed because rkyv archive is untouched");
    let err = archive.verify().unwrap_err();
    assert!(
        matches!(&err, MartError::Corrupt(msg) if msg.contains("digest mismatch")),
        "expected chunk digest mismatch, got {err:?}"
    );

    // restore() also enforces verify() and must fail
    let restore_dir = td.path().join("restored");
    let err = archive.restore(&restore_dir).unwrap_err();
    assert!(
        matches!(&err, MartError::Corrupt(msg) if msg.contains("digest mismatch")),
        "restore should fail on corrupt chunk, got {err:?}"
    );
}

#[test]
fn reject_path_traversal() {
    // validate_rel_path must reject leading slashes, backslashes, or .. components
    assert!(validate_rel_path("a/b/c").is_ok());
    assert!(validate_rel_path("/etc/passwd").is_err());
    assert!(validate_rel_path("../escape").is_err());
    assert!(validate_rel_path("a/../../root").is_err());
    assert!(validate_rel_path("").is_err());
}

#[test]
fn reject_symlink_dest_escape() {
    let td = tempdir().unwrap();
    let (_, mart_path) = create_valid_archive(td.path(), "test.mart");
    let archive = MartArchive::open(&mart_path).expect("archive open");

    #[cfg(unix)]
    {
        // If restore destination itself is a symlink, restore must reject it
        let real_dest = td.path().join("real_dest");
        fs::create_dir_all(&real_dest).unwrap();
        let symlink_dest = td.path().join("symlink_dest");
        std::os::unix::fs::symlink(&real_dest, &symlink_dest).unwrap();

        let err = archive.restore(&symlink_dest).unwrap_err();
        assert!(
            matches!(&err, MartError::Corrupt(msg) if msg.contains("restore dest is a symlink")),
            "expected 'restore dest is a symlink', got {err:?}"
        );
    }

    #[cfg(not(unix))]
    {
        let _ = archive;
    }
}
