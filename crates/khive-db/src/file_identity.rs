//! Physical identity of a database file, including SQLite's opened Windows handle.

use std::io;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DatabaseFileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial: u64,
    #[cfg(windows)]
    file_id: [u8; 16],
}

#[cfg(unix)]
impl DatabaseFileIdentity {
    pub(crate) fn unix_parts(self) -> (u64, u64) {
        (self.device, self.inode)
    }
}

#[cfg(unix)]
pub fn database_file_identity(path: &Path) -> io::Result<DatabaseFileIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::metadata(path)?;
    Ok(DatabaseFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
pub fn database_file_identity(path: &Path) -> io::Result<DatabaseFileIdentity> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let file = std::fs::OpenOptions::new()
        .read(true)
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)?;
    identity_from_handle(file.as_raw_handle())
}

#[cfg(windows)]
fn identity_from_handle(
    handle: std::os::windows::io::RawHandle,
) -> io::Result<DatabaseFileIdentity> {
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    let mut info = FILE_ID_INFO::default();
    // SAFETY: the caller retains a live handle; `info` is a writable buffer
    // of the exact size required by FileIdInfo. No ownership is transferred.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&raw mut info).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(DatabaseFileIdentity {
        volume_serial: info.VolumeSerialNumber,
        file_id: info.FileId.Identifier,
    })
}

#[cfg(windows)]
pub fn sqlite_opened_file_identity(
    conn: &rusqlite::Connection,
) -> Result<DatabaseFileIdentity, crate::error::SqliteError> {
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};

    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: the connection remains live for this call and the writable
    // out-parameter has the native HANDLE representation SQLite expects.
    let result = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            conn.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
            (&raw mut handle).cast(),
        )
    };
    if result != rusqlite::ffi::SQLITE_OK || handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(crate::error::SqliteError::InvalidData(format!(
            "cannot inspect opened SQLite database handle (file control {result})"
        )));
    }
    Ok(identity_from_handle(handle)?)
}
