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
/// bound to the commit record identified by `commit_digest`. The tmp file is
/// created exclusively (no-follow on unix) and the final rename is performed
/// relative to a directory descriptor, so a symlink planted at either path
/// cannot redirect the write.
pub fn write_external_ids_sidecar(
    dir: &std::path::Path,
    commit_digest: &[u8; 32],
    ids: &[Uuid],
) -> Result<(), String> {
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

/// Exclusive no-follow tmp create, then a directory-fd-relative atomic
/// rename over the final path (mirrors `khive-db`'s `walpin` sidecar write
/// idiom). A pre-existing entry at the tmp path — a planted symlink or a
/// stale tmp left by a crashed prior write — is unlinked first; `remove_file`
/// never follows a symlink, so it cannot be redirected either.
#[cfg(unix)]
fn write_via_dirfd(dir: &std::path::Path, buf: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::io::FromRawFd as _;

    let tmp_path = dir.join("external_ids.bin.tmp");

    match std::fs::remove_file(&tmp_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("remove stale external_ids.bin.tmp: {e}")),
    }

    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp_path)
        .map_err(|e| format!("create external_ids.bin.tmp: {e}"))?;
    f.write_all(buf)
        .map_err(|e| format!("write external_ids.bin.tmp: {e}"))?;
    f.sync_all()
        .map_err(|e| format!("sync external_ids.bin.tmp: {e}"))?;
    drop(f);

    let c_dir = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .map_err(|e| format!("segment dir path: {e}"))?;
    // SAFETY: `c_dir` is NUL-terminated for the call; the returned fd is
    // uniquely owned by this call and wrapped immediately below.
    let dir_fd = unsafe {
        libc::open(
            c_dir.as_ptr(),
            libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if dir_fd < 0 {
        return Err(format!(
            "open segment dir: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `dir_fd` was just returned by the successful `open` above and
    // is uniquely owned by this `File`, which closes it exactly once on drop.
    let dir_file = unsafe { std::fs::File::from_raw_fd(dir_fd) };

    // SAFETY: both names are NUL-terminated C string literals; `dir_fd` is a
    // live, open directory descriptor for the call's duration, and the
    // rename is performed relative to it rather than a re-resolved path.
    let rc = unsafe {
        libc::renameat(
            dir_fd,
            c"external_ids.bin.tmp".as_ptr(),
            dir_fd,
            c"external_ids.bin".as_ptr(),
        )
    };
    if rc != 0 {
        return Err(format!(
            "rename external_ids.bin.tmp -> external_ids.bin: {}",
            std::io::Error::last_os_error()
        ));
    }

    dir_file
        .sync_all()
        .map_err(|e| format!("sync segment dir: {e}"))
}

/// Non-unix fallback: exclusive tmp create (no `O_NOFOLLOW` equivalent in
/// `std` off unix) then a plain path-based rename.
#[cfg(not(unix))]
fn write_via_dirfd(dir: &std::path::Path, buf: &[u8]) -> Result<(), String> {
    use std::io::Write as _;

    let tmp_path = dir.join("external_ids.bin.tmp");
    let final_path = dir.join("external_ids.bin");

    match std::fs::remove_file(&tmp_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("remove stale external_ids.bin.tmp: {e}")),
    }

    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .map_err(|e| format!("create external_ids.bin.tmp: {e}"))?;
    f.write_all(buf)
        .map_err(|e| format!("write external_ids.bin.tmp: {e}"))?;
    f.sync_all()
        .map_err(|e| format!("sync external_ids.bin.tmp: {e}"))?;
    drop(f);
    std::fs::rename(&tmp_path, &final_path)
        .map_err(|e| format!("rename external_ids.bin.tmp -> external_ids.bin: {e}"))
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

    #[test]
    fn sidecar_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let digest = [7u8; 32];
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        write_external_ids_sidecar(dir.path(), &digest, &ids).expect("write");
        let (read_digest, read_ids) = read_external_ids_sidecar(dir.path()).expect("read");
        assert_eq!(read_digest, digest);
        assert_eq!(read_ids, ids);
    }

    #[test]
    fn sidecar_rejects_truncation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
        write_external_ids_sidecar(dir.path(), &[0u8; 32], &ids).expect("write");
        let path = dir.path().join("external_ids.bin");
        let bytes = std::fs::read(&path).expect("read back");
        std::fs::write(&path, &bytes[..bytes.len() - 1]).expect("truncate");
        assert!(read_external_ids_sidecar(dir.path()).is_err());
    }

    #[test]
    fn sidecar_rejects_overflowing_count() {
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        let segment_dir = tempfile::tempdir().expect("segment tempdir");
        let victim_dir = tempfile::tempdir().expect("victim tempdir");
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
    fn segment_commit_digest_absent_and_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(segment_commit_digest(dir.path()).expect("absent ok"), None);
        std::fs::write(dir.path().join("metadata.bin"), b"commit-bytes").expect("write");
        let digest = segment_commit_digest(dir.path())
            .expect("present ok")
            .expect("must be Some");
        assert_eq!(digest, *blake3::hash(b"commit-bytes").as_bytes());
    }
}
