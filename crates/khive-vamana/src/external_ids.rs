//! External-id sidecar codec for ANN segment directories (ADR-079).
//!
//! `external_ids.bin` maps segment ordinals back to caller UUIDs. It is bound
//! to one specific segment commit by storing the blake3 digest of the commit
//! record (`metadata.bin`) it was written against — the commit record carries
//! every segment file hash plus the fingerprint and watermark, so a
//! segment/sidecar pairing from different saves (crash between the segment
//! commit and the sidecar write, in either order) is self-detecting at load
//! time. A second digest over the id bytes themselves makes the mapping
//! corruption-evident independent of its length. One codec shared by every
//! pack that persists a segment.
//!
//! Binary format:
//!   magic         8 bytes   b"KHVANID2"
//!   commit_digest 32 bytes  blake3 of the metadata.bin bytes at write time
//!   ids_hash      32 bytes  blake3 of the raw id bytes that follow
//!   count         8 bytes   u64 little-endian — number of UUIDs
//!   ids           16 × count bytes — raw UUID bytes

use uuid::Uuid;

const SIDECAR_MAGIC: &[u8; 8] = b"KHVANID2";
const HEADER_LEN: usize = 8 + 32 + 32 + 8;

/// Fail-closed errors from an external-id sidecar write.
#[derive(Debug, thiserror::Error)]
pub enum ExternalIdsWriteError {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{context}: {detail}")]
    InvalidPath {
        context: &'static str,
        detail: String,
    },

    #[error("open segment dir: path identity changed during canonicalization")]
    DirectoryIdentityChanged,

    #[error(
        "secure external-id sidecar writes are unsupported on {platform}: no no-follow, handle-relative directory operations are available; refusing write"
    )]
    UnsupportedPlatform { platform: &'static str },
}

impl ExternalIdsWriteError {
    #[cfg(unix)]
    fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

/// Blake3 digest of `dir/metadata.bin` — the identity of one specific segment
/// commit. `Ok(None)` when the file is absent (no committed segment).
pub fn segment_commit_digest(dir: &std::path::Path) -> Result<Option<[u8; 32]>, String> {
    match std::fs::read(dir.join("metadata.bin")) {
        Ok(bytes) => Ok(Some(*blake3::hash(&bytes).as_bytes())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read metadata.bin: {e}")),
    }
}

/// Write `ids` to `dir/external_ids.bin` using a tmp-then-rename pattern,
/// bound to the commit record identified by `commit_digest`. On unix the
/// segment directory is canonicalized, every canonical path component is
/// opened without following symlinks, and every filesystem step runs relative
/// to the resulting descriptor. Legitimate symlinked ancestors are accepted,
/// while a symlink planted at the segment dir, tmp path, or final path cannot
/// redirect the write.
pub fn write_external_ids_sidecar(
    dir: &std::path::Path,
    commit_digest: &[u8; 32],
    ids: &[Uuid],
) -> Result<(), ExternalIdsWriteError> {
    let mut id_bytes: Vec<u8> = Vec::with_capacity(ids.len() * 16);
    for id in ids {
        id_bytes.extend_from_slice(id.as_bytes());
    }
    let ids_hash = blake3::hash(&id_bytes);

    let count = ids.len() as u64;
    let mut buf: Vec<u8> = Vec::with_capacity(HEADER_LEN + id_bytes.len());
    buf.extend_from_slice(SIDECAR_MAGIC);
    buf.extend_from_slice(commit_digest);
    buf.extend_from_slice(ids_hash.as_bytes());
    buf.extend_from_slice(&count.to_le_bytes());
    buf.extend_from_slice(&id_bytes);

    write_via_dirfd(dir, &buf)
}

/// All sidecar filesystem operations run relative to a directory descriptor
/// pinned BEFORE any byte is written (mirrors `khive-db`'s `walpin` sidecar
/// write idiom). Canonicalization admits legitimate symlinked ancestors; each
/// canonical component is then opened `O_NOFOLLOW`, and the caller's original
/// final component is independently opened `O_NOFOLLOW` and identity-checked
/// before the tmp file exists. The tmp entry is unlinked (`unlinkat` never
/// follows) and re-created `O_EXCL | O_NOFOLLOW` relative to that descriptor,
/// so a symlink planted at the tmp path cannot redirect the write either.
#[cfg(unix)]
fn write_via_dirfd(dir: &std::path::Path, buf: &[u8]) -> Result<(), ExternalIdsWriteError> {
    use std::io::Write as _;
    use std::os::unix::io::{AsRawFd as _, FromRawFd as _};

    let canonical_dir = std::fs::canonicalize(dir)
        .map_err(|e| ExternalIdsWriteError::io("canonicalize segment dir", e))?;
    let dir_file = open_dir_without_symlinks(&canonical_dir)?;
    verify_original_dir_identity(dir, &dir_file)?;
    let dir_fd = dir_file.as_raw_fd();

    const TMP_NAME: &std::ffi::CStr = c"external_ids.bin.tmp";

    // SAFETY: both arguments are live for the call; `unlinkat` removes a
    // planted symlink or stale tmp as a directory entry, never through it.
    let rc = unsafe { libc::unlinkat(dir_fd, TMP_NAME.as_ptr(), 0) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ENOENT) {
            return Err(ExternalIdsWriteError::io(
                "remove stale external_ids.bin.tmp",
                err,
            ));
        }
    }

    // SAFETY: the name and `dir_fd` are live for the call; the fd returned
    // on success is uniquely owned and wrapped immediately below.
    let tmp_fd = unsafe {
        libc::openat(
            dir_fd,
            TMP_NAME.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o644 as libc::c_uint,
        )
    };
    if tmp_fd < 0 {
        return Err(ExternalIdsWriteError::io(
            "create external_ids.bin.tmp",
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: `tmp_fd` was just returned by the successful `openat` above and
    // is uniquely owned by this `File`, which closes it exactly once on drop.
    let mut f = unsafe { std::fs::File::from_raw_fd(tmp_fd) };
    f.write_all(buf)
        .map_err(|e| ExternalIdsWriteError::io("write external_ids.bin.tmp", e))?;
    f.sync_all()
        .map_err(|e| ExternalIdsWriteError::io("sync external_ids.bin.tmp", e))?;
    drop(f);

    // SAFETY: both names are NUL-terminated C strings; `dir_fd` is a live,
    // open directory descriptor for the call's duration, and the rename is
    // performed relative to it rather than a re-resolved path.
    let rc = unsafe {
        libc::renameat(
            dir_fd,
            TMP_NAME.as_ptr(),
            dir_fd,
            c"external_ids.bin".as_ptr(),
        )
    };
    if rc != 0 {
        return Err(ExternalIdsWriteError::io(
            "rename external_ids.bin.tmp -> external_ids.bin",
            std::io::Error::last_os_error(),
        ));
    }

    dir_file
        .sync_all()
        .map_err(|e| ExternalIdsWriteError::io("sync segment dir", e))
}

#[cfg(unix)]
fn verify_original_dir_identity(
    dir: &std::path::Path,
    canonical_dir: &std::fs::File,
) -> Result<(), ExternalIdsWriteError> {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::io::FromRawFd as _;

    let c_dir = std::ffi::CString::new(dir.as_os_str().as_bytes()).map_err(|e| {
        ExternalIdsWriteError::InvalidPath {
            context: "segment dir path",
            detail: e.to_string(),
        }
    })?;
    // SAFETY: `c_dir` is NUL-terminated for the call; the returned fd is
    // uniquely owned and wrapped immediately below.
    let original_fd = unsafe {
        libc::open(
            c_dir.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if original_fd < 0 {
        return Err(ExternalIdsWriteError::io(
            "open original segment dir without following final component",
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: `original_fd` was returned by the successful `open` above and
    // is uniquely owned by this `File`.
    let original_dir = unsafe { std::fs::File::from_raw_fd(original_fd) };
    let original_meta = original_dir
        .metadata()
        .map_err(|e| ExternalIdsWriteError::io("stat original segment dir", e))?;
    let canonical_meta = canonical_dir
        .metadata()
        .map_err(|e| ExternalIdsWriteError::io("stat canonical segment dir", e))?;
    if original_meta.dev() != canonical_meta.dev() || original_meta.ino() != canonical_meta.ino() {
        return Err(ExternalIdsWriteError::DirectoryIdentityChanged);
    }

    Ok(())
}

#[cfg(unix)]
fn open_dir_without_symlinks(
    dir: &std::path::Path,
) -> Result<std::fs::File, ExternalIdsWriteError> {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::io::{AsRawFd as _, FromRawFd as _};

    if dir.as_os_str().is_empty() {
        return Err(ExternalIdsWriteError::InvalidPath {
            context: "open segment dir",
            detail: "empty path".into(),
        });
    }

    let start = if dir.is_absolute() { c"/" } else { c"." };
    // SAFETY: `start` is NUL-terminated; the returned fd is uniquely owned
    // and wrapped immediately below.
    let start_fd = unsafe {
        libc::open(
            start.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if start_fd < 0 {
        return Err(ExternalIdsWriteError::io(
            "open segment dir root",
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: `start_fd` was returned by the successful `open` above and is
    // uniquely owned by this `File`.
    let mut current = unsafe { std::fs::File::from_raw_fd(start_fd) };

    for component in dir.components() {
        let name = match component {
            std::path::Component::RootDir | std::path::Component::CurDir => continue,
            std::path::Component::ParentDir | std::path::Component::Normal(_) => {
                std::ffi::CString::new(component.as_os_str().as_bytes()).map_err(|e| {
                    ExternalIdsWriteError::InvalidPath {
                        context: "segment dir path component",
                        detail: e.to_string(),
                    }
                })?
            }
            std::path::Component::Prefix(_) => {
                return Err(ExternalIdsWriteError::InvalidPath {
                    context: "open segment dir",
                    detail: "unsupported path prefix".into(),
                });
            }
        };

        // SAFETY: `name` is NUL-terminated and `current` owns a live
        // directory fd; the returned fd is wrapped immediately on success.
        let next_fd = unsafe {
            libc::openat(
                current.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if next_fd < 0 {
            return Err(ExternalIdsWriteError::io(
                format!("open segment dir component {:?}", component.as_os_str()),
                std::io::Error::last_os_error(),
            ));
        }
        // SAFETY: `next_fd` was returned by the successful `openat` above
        // and is uniquely owned by the replacement `File`.
        current = unsafe { std::fs::File::from_raw_fd(next_fd) };
    }

    Ok(current)
}

/// Non-Unix `std` cannot express the no-follow, descriptor-relative unlink,
/// create, rename, and sync sequence. Refuse before mutating the filesystem.
#[cfg(not(unix))]
fn write_via_dirfd(_dir: &std::path::Path, _buf: &[u8]) -> Result<(), ExternalIdsWriteError> {
    Err(ExternalIdsWriteError::UnsupportedPlatform {
        platform: std::env::consts::OS,
    })
}

/// Read `dir/external_ids.bin` and return `(commit_digest, ids)`.
///
/// Returns `Err` on any I/O error, wrong magic, truncated header, count/size
/// mismatch (checked arithmetic — a hostile count cannot wrap), or an ids
/// digest that does not match the id bytes.
pub fn read_external_ids_sidecar(dir: &std::path::Path) -> Result<([u8; 32], Vec<Uuid>), String> {
    let bytes = std::fs::read(dir.join("external_ids.bin"))
        .map_err(|e| format!("read external_ids.bin: {e}"))?;

    if bytes.len() < HEADER_LEN {
        return Err(format!(
            "external_ids.bin too short: {} bytes (need at least {HEADER_LEN})",
            bytes.len()
        ));
    }

    let magic = &bytes[0..8];
    if magic != SIDECAR_MAGIC {
        return Err(format!(
            "external_ids.bin bad magic: got {:?}, expected {:?}",
            magic, SIDECAR_MAGIC
        ));
    }

    let mut commit_digest = [0u8; 32];
    commit_digest.copy_from_slice(&bytes[8..40]);
    let mut ids_hash = [0u8; 32];
    ids_hash.copy_from_slice(&bytes[40..72]);

    let count = u64::from_le_bytes(bytes[72..80].try_into().unwrap());
    let count =
        usize::try_from(count).map_err(|_| "external_ids.bin count exceeds usize".to_string())?;
    let expected_len = count
        .checked_mul(16)
        .and_then(|n| n.checked_add(HEADER_LEN))
        .ok_or_else(|| format!("external_ids.bin count {count} overflows length arithmetic"))?;
    if bytes.len() != expected_len {
        return Err(format!(
            "external_ids.bin length mismatch: got {} bytes, expected {expected_len} for {count} UUIDs",
            bytes.len(),
        ));
    }

    let id_bytes = &bytes[HEADER_LEN..];
    if *blake3::hash(id_bytes).as_bytes() != ids_hash {
        return Err("external_ids.bin ids digest mismatch (corrupt or truncated mapping)".into());
    }

    let mut ids = Vec::with_capacity(count);
    for chunk in id_bytes.chunks_exact(16) {
        let raw: [u8; 16] = chunk.try_into().unwrap();
        ids.push(Uuid::from_bytes(raw));
    }

    Ok((commit_digest, ids))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        let root = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
        tempfile::tempdir_in(root).expect("tempdir")
    }

    #[test]
    fn sidecar_round_trip() {
        let dir = tempdir();
        let digest = [7u8; 32];
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        write_external_ids_sidecar(dir.path(), &digest, &ids).expect("write");
        let (read_digest, read_ids) = read_external_ids_sidecar(dir.path()).expect("read");
        assert_eq!(read_digest, digest);
        assert_eq!(read_ids, ids);
    }

    #[test]
    fn sidecar_rejects_truncation() {
        let dir = tempdir();
        let ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
        write_external_ids_sidecar(dir.path(), &[0u8; 32], &ids).expect("write");
        let path = dir.path().join("external_ids.bin");
        let bytes = std::fs::read(&path).expect("read back");
        std::fs::write(&path, &bytes[..bytes.len() - 1]).expect("truncate");
        assert!(read_external_ids_sidecar(dir.path()).is_err());
    }

    #[test]
    fn sidecar_rejects_overflowing_count() {
        let dir = tempdir();
        write_external_ids_sidecar(dir.path(), &[0u8; 32], &[]).expect("write");
        let path = dir.path().join("external_ids.bin");
        let mut bytes = std::fs::read(&path).expect("read back");
        // count = 2^60: count * 16 wraps to 0 under unchecked u64 arithmetic,
        // which would let a header-only file pass a naive length equality.
        bytes[72..80].copy_from_slice(&(1u64 << 60).to_le_bytes());
        std::fs::write(&path, &bytes).expect("write hostile count");
        let err = read_external_ids_sidecar(dir.path()).expect_err("must reject");
        assert!(
            err.contains("overflow") || err.contains("length mismatch"),
            "hostile count must fail arithmetic or length validation, got: {err}"
        );
    }

    #[test]
    fn sidecar_rejects_flipped_id_byte() {
        let dir = tempdir();
        let ids: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
        write_external_ids_sidecar(dir.path(), &[0u8; 32], &ids).expect("write");
        let path = dir.path().join("external_ids.bin");
        let mut bytes = std::fs::read(&path).expect("read back");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&path, &bytes).expect("write corrupted");
        let err = read_external_ids_sidecar(dir.path()).expect_err("must reject");
        assert!(err.contains("ids digest mismatch"), "got: {err}");
    }

    #[test]
    #[cfg(unix)]
    fn write_external_ids_sidecar_refuses_symlinked_tmp_path() {
        let segment_dir = tempdir();
        let victim_dir = tempdir();
        let victim_path = victim_dir.path().join("victim.bin");
        std::fs::write(&victim_path, b"precious").expect("write victim");

        let tmp_path = segment_dir.path().join("external_ids.bin.tmp");
        std::os::unix::fs::symlink(&victim_path, &tmp_path).expect("symlink tmp path");

        let digest = [9u8; 32];
        let ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
        write_external_ids_sidecar(segment_dir.path(), &digest, &ids)
            .expect("write must succeed despite the planted symlink");

        assert_eq!(
            std::fs::read(&victim_path).expect("victim readable"),
            b"precious",
            "the symlink target must never be written through"
        );

        let (read_digest, read_ids) =
            read_external_ids_sidecar(segment_dir.path()).expect("sidecar must be readable");
        assert_eq!(read_digest, digest);
        assert_eq!(read_ids, ids);
    }

    #[test]
    #[cfg(unix)]
    fn write_external_ids_sidecar_refuses_symlinked_segment_dir() {
        let victim_dir = tempdir();
        let link_parent = tempdir();
        let dir_link = link_parent.path().join("segment");
        std::os::unix::fs::symlink(victim_dir.path(), &dir_link).expect("symlink segment dir");

        let digest = [7u8; 32];
        let ids: Vec<Uuid> = (0..2).map(|_| Uuid::new_v4()).collect();
        let err = write_external_ids_sidecar(&dir_link, &digest, &ids)
            .expect_err("a symlinked segment dir must refuse before any byte is written");
        assert!(err.to_string().contains("segment dir"), "got: {err}");

        assert!(
            !victim_dir.path().join("external_ids.bin.tmp").exists()
                && !victim_dir.path().join("external_ids.bin").exists(),
            "nothing may be written through the segment-dir symlink"
        );
    }

    #[test]
    #[cfg(unix)]
    fn write_external_ids_sidecar_accepts_symlinked_ancestor() {
        let real_parent = tempdir();
        let real_segment = real_parent.path().join("segment");
        std::fs::create_dir(&real_segment).expect("create real segment dir");

        let link_parent = tempdir();
        let parent_link = link_parent.path().join("parent");
        std::os::unix::fs::symlink(real_parent.path(), &parent_link).expect("symlink ancestor");
        let via_ancestor = parent_link.join("segment");

        let digest = [5u8; 32];
        let ids: Vec<Uuid> = (0..2).map(|_| Uuid::new_v4()).collect();
        write_external_ids_sidecar(&via_ancestor, &digest, &ids)
            .expect("a legitimate symlinked ancestor must be canonicalized");

        let (read_digest, read_ids) =
            read_external_ids_sidecar(&real_segment).expect("sidecar lands in canonical dir");
        assert_eq!(read_digest, digest);
        assert_eq!(read_ids, ids);
    }

    #[test]
    fn segment_commit_digest_absent_and_present() {
        let dir = tempdir();
        assert_eq!(segment_commit_digest(dir.path()).expect("absent ok"), None);
        std::fs::write(dir.path().join("metadata.bin"), b"commit-bytes").expect("write");
        let digest = segment_commit_digest(dir.path())
            .expect("present ok")
            .expect("must be Some");
        assert_eq!(digest, *blake3::hash(b"commit-bytes").as_bytes());
    }
}
