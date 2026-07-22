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

    #[error("open segment dir: path identity changed during validation")]
    DirectoryIdentityChanged,

    #[error(
        "open segment dir: untrusted ancestor symlink {component:?} (symlink uid {symlink_uid}, parent uid {parent_uid}, parent mode {parent_mode:#o})"
    )]
    UntrustedAncestorSymlink {
        component: std::ffi::OsString,
        symlink_uid: u32,
        parent_uid: u32,
        parent_mode: u32,
    },

    #[error(
        "secure external-id sidecar writes are unsupported on {platform}: no no-follow, handle-relative directory operations are available; refusing write"
    )]
    UnsupportedPlatform { platform: &'static str },
}

impl ExternalIdsWriteError {
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
/// bound to the commit record identified by `commit_digest`. On Unix every
/// original path component is opened without following symlinks. Ancestor
/// symlinks are followed only when their ownership and parent directory are
/// trusted; every filesystem step then runs relative to the resulting
/// descriptor.
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
/// write idiom). The caller's original path is walked `O_NOFOLLOW`; trusted
/// ancestor symlinks are resolved from their pinned parent descriptors. The
/// original final component is independently opened `O_NOFOLLOW` and
/// identity-checked before the tmp file exists. The tmp entry is unlinked and
/// re-created `O_EXCL | O_NOFOLLOW` relative to that descriptor.
#[cfg(unix)]
fn write_via_dirfd(dir: &std::path::Path, buf: &[u8]) -> Result<(), ExternalIdsWriteError> {
    use std::io::Write as _;
    use std::os::unix::io::{AsRawFd as _, FromRawFd as _};

    let dir_file = open_dir_with_trusted_symlinks(dir)?;
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
fn open_dir_with_trusted_symlinks(
    dir: &std::path::Path,
) -> Result<std::fs::File, ExternalIdsWriteError> {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::io::AsRawFd as _;

    if dir.as_os_str().is_empty() {
        return Err(ExternalIdsWriteError::InvalidPath {
            context: "open segment dir",
            detail: "empty path".into(),
        });
    }

    let mut pending = owned_components(dir)?;
    let mut current = open_start_directory(dir.is_absolute())?;
    let effective_uid = unsafe { libc::geteuid() } as u32;
    let mut followed_symlinks = 0usize;

    while let Some(component) = pending.pop_front() {
        let name = std::ffi::CString::new(component.as_bytes()).map_err(|e| {
            ExternalIdsWriteError::InvalidPath {
                context: "segment dir path component",
                detail: e.to_string(),
            }
        })?;
        match open_directory_at(current.as_raw_fd(), &name) {
            Ok(next) => current = next,
            Err(open_error) => {
                let link_meta = metadata_at_no_follow(current.as_raw_fd(), &name)?;
                if link_meta.st_mode & libc::S_IFMT != libc::S_IFLNK {
                    return Err(ExternalIdsWriteError::io(
                        format!("open segment dir component {component:?}"),
                        open_error,
                    ));
                }
                if pending.is_empty() {
                    return Err(ExternalIdsWriteError::InvalidPath {
                        context: "open segment dir",
                        detail: "final component is a symlink".into(),
                    });
                }

                let parent_meta = current
                    .metadata()
                    .map_err(|e| ExternalIdsWriteError::io("stat symlink parent", e))?;
                let symlink_uid = link_meta.st_uid;
                let parent_uid = parent_meta.uid();
                let parent_mode = parent_meta.mode();
                if !trusted_ancestor_symlink(symlink_uid, parent_uid, parent_mode, effective_uid) {
                    return Err(ExternalIdsWriteError::UntrustedAncestorSymlink {
                        component,
                        symlink_uid,
                        parent_uid,
                        parent_mode,
                    });
                }

                followed_symlinks += 1;
                if followed_symlinks > 40 {
                    return Err(ExternalIdsWriteError::InvalidPath {
                        context: "open segment dir",
                        detail: "too many ancestor symlinks".into(),
                    });
                }
                let target = read_link_at(current.as_raw_fd(), &name, link_meta.st_size)?;
                let target_is_absolute = target.is_absolute();
                let mut target_components = owned_components(&target)?;
                target_components.append(&mut pending);
                pending = target_components;
                if target_is_absolute {
                    current = open_start_directory(true)?;
                }
            }
        }
    }

    Ok(current)
}

#[cfg(unix)]
fn owned_components(
    path: &std::path::Path,
) -> Result<std::collections::VecDeque<std::ffi::OsString>, ExternalIdsWriteError> {
    let mut components = std::collections::VecDeque::new();
    for component in path.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::ParentDir | std::path::Component::Normal(_) => {
                components.push_back(component.as_os_str().to_os_string());
            }
            std::path::Component::Prefix(_) => {
                return Err(ExternalIdsWriteError::InvalidPath {
                    context: "open segment dir",
                    detail: "unsupported path prefix".into(),
                });
            }
        }
    }
    Ok(components)
}

#[cfg(unix)]
fn open_start_directory(absolute: bool) -> Result<std::fs::File, ExternalIdsWriteError> {
    use std::os::unix::io::FromRawFd as _;

    let start = if absolute { c"/" } else { c"." };
    // SAFETY: `start` is NUL-terminated; a successful fd is uniquely owned.
    let fd = unsafe {
        libc::open(
            start.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(ExternalIdsWriteError::io(
            "open segment dir root",
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: `fd` is newly returned and transferred exactly once.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn open_directory_at(
    parent_fd: std::os::unix::io::RawFd,
    name: &std::ffi::CStr,
) -> Result<std::fs::File, std::io::Error> {
    use std::os::unix::io::FromRawFd as _;

    // SAFETY: `name` and `parent_fd` are live; a successful fd is uniquely owned.
    let fd = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is newly returned and transferred exactly once.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn metadata_at_no_follow(
    parent_fd: std::os::unix::io::RawFd,
    name: &std::ffi::CStr,
) -> Result<libc::stat, ExternalIdsWriteError> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `metadata` is writable and both lookup arguments are live.
    let rc = unsafe {
        libc::fstatat(
            parent_fd,
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(ExternalIdsWriteError::io(
            "inspect segment dir component without following symlink",
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: successful `fstatat` initialized the output structure.
    Ok(unsafe { metadata.assume_init() })
}

#[cfg(unix)]
fn read_link_at(
    parent_fd: std::os::unix::io::RawFd,
    name: &std::ffi::CStr,
    size_hint: libc::off_t,
) -> Result<std::path::PathBuf, ExternalIdsWriteError> {
    use std::os::unix::ffi::OsStringExt as _;

    let hinted = usize::try_from(size_hint).unwrap_or(0).saturating_add(1);
    let mut capacity = hinted.clamp(256, 65_536);
    loop {
        let mut bytes = vec![0u8; capacity];
        // SAFETY: the buffer is writable and the lookup arguments are live.
        let len = unsafe {
            libc::readlinkat(
                parent_fd,
                name.as_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
            )
        };
        if len < 0 {
            return Err(ExternalIdsWriteError::io(
                "read trusted ancestor symlink",
                std::io::Error::last_os_error(),
            ));
        }
        let len = len as usize;
        if len < bytes.len() {
            bytes.truncate(len);
            return Ok(std::ffi::OsString::from_vec(bytes).into());
        }
        if capacity == 65_536 {
            return Err(ExternalIdsWriteError::InvalidPath {
                context: "open segment dir",
                detail: "ancestor symlink target is too long".into(),
            });
        }
        capacity = (capacity * 2).min(65_536);
    }
}

#[cfg(unix)]
fn trusted_ancestor_symlink(
    symlink_uid: u32,
    parent_uid: u32,
    parent_mode: u32,
    effective_uid: u32,
) -> bool {
    let owner_is_trusted = |uid| uid == 0 || uid == effective_uid;
    owner_is_trusted(symlink_uid) && owner_is_trusted(parent_uid) && parent_mode & 0o022 == 0
}

#[cfg(any(not(unix), test))]
fn ensure_not_symlink_or_reparse(
    path: &std::path::Path,
    context: &'static str,
) -> Result<Option<std::fs::Metadata>, ExternalIdsWriteError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_symlink_or_reparse(&metadata) => {
            Err(ExternalIdsWriteError::InvalidPath {
                context,
                detail: "path is a symlink or reparse point".into(),
            })
        }
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ExternalIdsWriteError::io(context, error)),
    }
}

#[cfg(any(not(unix), test))]
fn metadata_is_symlink_or_reparse(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    false
}

#[cfg(any(not(unix), test))]
fn ensure_portable_ancestors_not_symlinks(
    dir: &std::path::Path,
    context: &'static str,
) -> Result<(), ExternalIdsWriteError> {
    const MAX_ANCESTORS: usize = 40;

    let ancestors: Vec<_> = dir
        .ancestors()
        .skip(1)
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .take(MAX_ANCESTORS + 1)
        .collect();
    if ancestors.len() > MAX_ANCESTORS {
        return Err(ExternalIdsWriteError::InvalidPath {
            context,
            detail: format!("path has more than {MAX_ANCESTORS} ancestor components"),
        });
    }
    for ancestor in ancestors.into_iter().rev() {
        ensure_not_symlink_or_reparse(ancestor, context)?;
    }
    Ok(())
}

/// Non-Unix `std` has no handle-relative no-follow rename, so this is a
/// best-effort path-based fallback. It rejects pre-existing symlink/reparse
/// points in the segment directory's ancestors and in the segment/sidecar
/// entries. A component can still be replaced between a check and a later
/// path-based mutation; portable `std` cannot close that check-to-use window.
#[cfg(not(unix))]
fn write_via_dirfd(dir: &std::path::Path, buf: &[u8]) -> Result<(), ExternalIdsWriteError> {
    write_via_paths(dir, buf)
}

#[cfg(any(not(unix), test))]
fn write_via_paths(dir: &std::path::Path, buf: &[u8]) -> Result<(), ExternalIdsWriteError> {
    use std::io::Write as _;

    ensure_portable_ancestors_not_symlinks(dir, "inspect segment dir ancestor")?;
    let dir_metadata =
        ensure_not_symlink_or_reparse(dir, "inspect segment dir")?.ok_or_else(|| {
            ExternalIdsWriteError::io(
                "inspect segment dir",
                std::io::Error::new(std::io::ErrorKind::NotFound, "segment dir does not exist"),
            )
        })?;
    if !dir_metadata.is_dir() {
        return Err(ExternalIdsWriteError::InvalidPath {
            context: "inspect segment dir",
            detail: "path is not a directory".into(),
        });
    }

    let tmp_path = dir.join("external_ids.bin.tmp");
    let final_path = dir.join("external_ids.bin");
    let stale_tmp = ensure_not_symlink_or_reparse(&tmp_path, "inspect external_ids.bin.tmp")?;
    ensure_not_symlink_or_reparse(&final_path, "inspect external_ids.bin")?;
    if stale_tmp.is_some() {
        std::fs::remove_file(&tmp_path)
            .map_err(|e| ExternalIdsWriteError::io("remove stale external_ids.bin.tmp", e))?;
    }

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .map_err(|e| ExternalIdsWriteError::io("create external_ids.bin.tmp", e))?;
    file.write_all(buf)
        .map_err(|e| ExternalIdsWriteError::io("write external_ids.bin.tmp", e))?;
    file.sync_all()
        .map_err(|e| ExternalIdsWriteError::io("sync external_ids.bin.tmp", e))?;
    drop(file);

    ensure_portable_ancestors_not_symlinks(dir, "reinspect segment dir ancestor")?;
    ensure_not_symlink_or_reparse(dir, "reinspect segment dir")?;
    ensure_not_symlink_or_reparse(&tmp_path, "reinspect external_ids.bin.tmp")?;
    ensure_not_symlink_or_reparse(&final_path, "reinspect external_ids.bin")?;
    #[cfg(windows)]
    match std::fs::remove_file(&final_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(ExternalIdsWriteError::io(
                "remove previous external_ids.bin",
                error,
            ));
        }
    }
    std::fs::rename(&tmp_path, &final_path).map_err(|e| {
        ExternalIdsWriteError::io("rename external_ids.bin.tmp -> external_ids.bin", e)
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
    #[cfg(unix)]
    fn write_external_ids_sidecar_refuses_symlink_in_world_writable_parent() {
        use std::os::unix::fs::PermissionsExt as _;

        let real_parent = tempdir();
        let real_segment = real_parent.path().join("segment");
        std::fs::create_dir(&real_segment).expect("create real segment dir");

        let link_parent = tempdir();
        std::fs::set_permissions(link_parent.path(), std::fs::Permissions::from_mode(0o777))
            .expect("make symlink parent world-writable");
        let parent_link = link_parent.path().join("parent");
        std::os::unix::fs::symlink(real_parent.path(), &parent_link).expect("symlink ancestor");

        let err = write_external_ids_sidecar(&parent_link.join("segment"), &[3u8; 32], &[])
            .expect_err("a symlink in a world-writable parent must be refused");
        assert!(
            err.to_string().contains("untrusted ancestor symlink"),
            "got: {err}"
        );
        assert!(
            !real_segment.join("external_ids.bin.tmp").exists()
                && !real_segment.join("external_ids.bin").exists(),
            "nothing may be written through the untrusted ancestor symlink"
        );
    }

    #[test]
    #[cfg(unix)]
    fn ancestor_symlink_trust_requires_safe_owners_and_parent_mode() {
        let effective_uid = unsafe { libc::geteuid() } as u32;
        let foreign_uid = if effective_uid == 1 { 2 } else { 1 };

        assert!(trusted_ancestor_symlink(
            effective_uid,
            effective_uid,
            0o40700,
            effective_uid
        ));
        assert!(trusted_ancestor_symlink(
            0,
            effective_uid,
            0o40755,
            effective_uid
        ));
        assert!(!trusted_ancestor_symlink(
            foreign_uid,
            effective_uid,
            0o40700,
            effective_uid
        ));
        assert!(!trusted_ancestor_symlink(
            effective_uid,
            foreign_uid,
            0o40700,
            effective_uid
        ));
        assert!(!trusted_ancestor_symlink(
            effective_uid,
            effective_uid,
            0o40722,
            effective_uid
        ));
    }

    #[test]
    #[cfg(unix)]
    fn portable_path_check_refuses_symlink() {
        let dir = tempdir();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        std::fs::write(&target, b"target").expect("write target");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        ensure_not_symlink_or_reparse(&target, "inspect target").expect("regular path must pass");
        let err = ensure_not_symlink_or_reparse(&link, "inspect link")
            .expect_err("portable check must refuse a symlink");
        assert!(matches!(err, ExternalIdsWriteError::InvalidPath { .. }));
    }

    #[test]
    fn portable_writer_round_trips_bytes() {
        let dir = tempdir();
        write_via_paths(dir.path(), b"sidecar").expect("portable write");
        assert_eq!(
            std::fs::read(dir.path().join("external_ids.bin")).expect("read sidecar"),
            b"sidecar"
        );
    }

    #[test]
    #[cfg(unix)]
    fn portable_writer_refuses_symlinked_sidecar_paths() {
        let victim_dir = tempdir();
        let victim = victim_dir.path().join("victim");
        std::fs::write(&victim, b"precious").expect("write victim");

        for sidecar_name in ["external_ids.bin.tmp", "external_ids.bin"] {
            let segment_dir = tempdir();
            std::os::unix::fs::symlink(&victim, segment_dir.path().join(sidecar_name))
                .expect("create sidecar symlink");
            let err = write_via_paths(segment_dir.path(), b"replacement")
                .expect_err("portable writer must refuse sidecar symlinks");
            assert!(matches!(err, ExternalIdsWriteError::InvalidPath { .. }));
            assert_eq!(std::fs::read(&victim).expect("read victim"), b"precious");
        }
    }

    #[test]
    #[cfg(unix)]
    fn portable_writer_refuses_symlinked_segment_dir() {
        let real_dir = tempdir();
        let link_parent = tempdir();
        let segment_link = link_parent.path().join("segment");
        std::os::unix::fs::symlink(real_dir.path(), &segment_link).expect("create segment symlink");

        let err = write_via_paths(&segment_link, b"sidecar")
            .expect_err("portable writer must refuse a symlinked segment dir");
        assert!(matches!(err, ExternalIdsWriteError::InvalidPath { .. }));
        assert!(!real_dir.path().join("external_ids.bin").exists());
    }

    #[test]
    #[cfg(unix)]
    fn portable_writer_refuses_symlinked_ancestor() {
        let real_parent = tempdir();
        let real_segment = real_parent.path().join("regular").join("segment");
        std::fs::create_dir_all(&real_segment).expect("create real segment dir");

        let link_parent = tempdir();
        let ancestor_link = link_parent.path().join("ancestor");
        std::os::unix::fs::symlink(real_parent.path(), &ancestor_link)
            .expect("create ancestor symlink");

        let err = write_via_paths(&ancestor_link.join("regular").join("segment"), b"sidecar")
            .expect_err("portable writer must refuse a symlinked ancestor");
        assert!(matches!(err, ExternalIdsWriteError::InvalidPath { .. }));
        assert!(!real_segment.join("external_ids.bin").exists());
    }

    #[test]
    fn portable_writer_accepts_regular_ancestor_chain() {
        let root = tempdir();
        let segment_dir = root.path().join("one").join("two").join("segment");
        std::fs::create_dir_all(&segment_dir).expect("create regular ancestor chain");

        write_via_paths(&segment_dir, b"sidecar").expect("portable write");
        assert_eq!(
            std::fs::read(segment_dir.join("external_ids.bin")).expect("read sidecar"),
            b"sidecar"
        );
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
