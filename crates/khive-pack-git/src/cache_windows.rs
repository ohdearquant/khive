//! Windows command-path pins for an already-owned git cache slot (#2149).
//!
//! Git accepts a pathname, not a directory handle. Keep every directory in
//! that pathname open without delete sharing until the last child exits.
//! This prevents a validated slot or `.git` child from being renamed away
//! and replaced by a symlink; it does not make repository contents immutable.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::{
    FileIdInfo, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_INFO, FILE_NAME_NORMALIZED, FILE_SHARE_READ, FILE_SHARE_WRITE, VOLUME_NAME_DOS,
};

pub(super) struct PinnedSlot {
    git_dir: PathBuf,
    // Includes the initially resolved slot, every canonical ancestor, `.git`,
    // and the ownership marker. Never release before a command's wait ends.
    _pins: Vec<File>,
}

impl PinnedSlot {
    pub(super) fn open(repo: &Path) -> io::Result<Self> {
        // Open the final slot component itself, not a reparse-point target.
        // Existing aliases ABOVE the slot may resolve here; commands below
        // use only the resolved path, so those aliases are never re-used.
        let initial = open_pin(repo, true)?;
        let expected_identity = identity(&initial)?;
        let resolved = final_path(&initial)?;
        if !resolved.is_absolute() {
            return Err(invalid("slot handle did not resolve to an absolute path"));
        }

        let mut pins = vec![initial];
        // A final-path string alone is another TOCTOU. Pin root-to-leaf so
        // each next pathname resolves through parents already held open.
        // Verify the last handle still denotes the initial slot: an ancestor
        // may have moved between final_path() and opening this chain.
        for ancestor in resolved.ancestors().collect::<Vec<_>>().into_iter().rev() {
            pins.push(open_pin(ancestor, true)?);
        }
        let pinned_slot = pins
            .last()
            .expect("absolute path has a directory component");
        if identity(pinned_slot)? != expected_identity {
            return Err(invalid(
                "slot changed while its command path was being pinned",
            ));
        }

        let git_dir = resolved.join(".git");
        pins.push(open_pin(&git_dir, true)?);
        pins.push(open_pin(&resolved.join(super::MARKER_FILE), false)?);
        Ok(Self {
            git_dir,
            _pins: pins,
        })
    }

    pub(super) fn git_dir(&self) -> &Path {
        &self.git_dir
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn open_pin(path: &Path, directory: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        // Windows delete access includes rename. An already-open DELETE
        // handle also makes this open fail, rather than weakening the pin.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    // Check all reparse tags, including junctions, not only symlink_dir.
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(invalid(
            "cache ownership component has the wrong kind or is a reparse point",
        ));
    }
    Ok(file)
}

fn identity(file: &File) -> io::Result<(u64, [u8; 16])> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: file owns a live handle and info is a correctly sized writable
    // FILE_ID_INFO buffer for FileIdInfo. No handle ownership is transferred.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&raw mut info).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

fn final_path(file: &File) -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 260];
    loop {
        // SAFETY: file remains live; buffer exposes the stated writable
        // UTF-16 capacity for this call. No returned handle needs closing.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        let length = length as usize;
        if length < buffer.len() {
            buffer.truncate(length);
            return Ok(PathBuf::from(OsString::from_wide(&buffer)));
        }
        buffer.resize(length.saturating_add(1), 0);
    }
}
