use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::raw::c_int;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::FileExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use rusqlite::ffi;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FileIdExtdDirectoryInformation, FileStatInformation, NtCreateFile, NtQueryDirectoryFile,
    NtQueryInformationByName, FILE_CREATE, FILE_DIRECTORY_FILE, FILE_ID_EXTD_DIR_INFORMATION,
    FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT,
    FILE_STAT_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
};
use windows_sys::Win32::Foundation::{
    RtlNtStatusToDosError, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION, GENERIC_READ,
    GENERIC_WRITE, HANDLE, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, STATUS_NO_MORE_FILES,
    STATUS_NO_SUCH_FILE, STATUS_OBJECT_NAME_NOT_FOUND, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileAttributeTagInfo, FileDispositionInfo, FileIdInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx, LockFileEx, SetFileInformationByHandle, UnlockFileEx,
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_DISPOSITION_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, LOCKFILE_EXCLUSIVE_LOCK,
    LOCKFILE_FAIL_IMMEDIATELY, OPEN_EXISTING, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::{IO_STATUS_BLOCK, OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};

use super::{Observed, OpenAccess};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Identity {
    volume: u64,
    file_id: [u8; 16],
}

// Match SQLite's Win32 rollback VFS byte ranges. The lock-byte page is
// deliberately omitted from database content by SQLite's pager.
const PENDING_BYTE: u64 = 0x4000_0000;
const RESERVED_BYTE: u64 = PENDING_BYTE + 1;
const SHARED_FIRST: u64 = PENDING_BYTE + 2;
const SHARED_SIZE: u32 = 510;

#[derive(Clone, Copy, Default)]
struct LocalLocks {
    level: c_int,
    shared: bool,
    reserved: bool,
    pending: bool,
    exclusive: bool,
    poisoned: bool,
}

type LockOwners = HashMap<u64, LocalLocks>;

fn local_conflict(
    owners: &LockOwners,
    current_owner: u64,
    predicate: fn(&LocalLocks) -> bool,
) -> bool {
    owners
        .iter()
        .any(|(&owner, locks)| owner != current_owner && predicate(locks))
}

fn rollback_owners() -> &'static Mutex<HashMap<Identity, LockOwners>> {
    static OWNERS: OnceLock<Mutex<HashMap<Identity, LockOwners>>> = OnceLock::new();
    OWNERS.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_LOCK_OWNER: AtomicU64 = AtomicU64::new(1);

fn overlapped_at(offset: u64) -> OVERLAPPED {
    let mut overlapped = OVERLAPPED::default();
    overlapped.Anonymous = OVERLAPPED_0 {
        Anonymous: OVERLAPPED_0_0 {
            Offset: offset as u32,
            OffsetHigh: (offset >> 32) as u32,
        },
    };
    overlapped
}

fn lock_range(file: &File, offset: u64, count: u32, exclusive: bool) -> io::Result<()> {
    let mut overlapped = overlapped_at(offset);
    let flags = LOCKFILE_FAIL_IMMEDIATELY
        | if exclusive {
            LOCKFILE_EXCLUSIVE_LOCK
        } else {
            0
        };
    // SAFETY: file is live; OVERLAPPED remains live for the synchronous,
    // fail-immediately byte-range lock call. The handle was opened for
    // synchronous I/O by the native guarded opener.
    if unsafe {
        LockFileEx(
            file.as_raw_handle(),
            flags,
            0,
            count,
            0,
            &raw mut overlapped,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn unlock_range(file: &File, offset: u64, count: u32) -> io::Result<()> {
    let mut overlapped = overlapped_at(offset);
    // SAFETY: file is live, and the offset/count identify a range acquired
    // through this same handle by lock_range.
    if unsafe { UnlockFileEx(file.as_raw_handle(), 0, count, 0, &raw mut overlapped) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn lock_error(error: &io::Error) -> c_int {
    match error.raw_os_error() {
        Some(code)
            if code == ERROR_LOCK_VIOLATION as i32 || code == ERROR_SHARING_VIOLATION as i32 =>
        {
            ffi::SQLITE_BUSY
        }
        _ => ffi::SQLITE_IOERR_LOCK,
    }
}

/// Rollback-journal locking for one guarded main-database handle. All native
/// operations and same-process ownership changes run under one registry
/// mutex, keyed by the identity attested from the opened handle. Windows
/// locks are attached to a handle; the registry closes same-process windows
/// between a local check and LockFileEx on a different handle.
pub(super) struct RollbackLock {
    identity: Identity,
    owner: u64,
    held: LocalLocks,
}

impl RollbackLock {
    pub(super) fn new(identity: Identity) -> Self {
        Self {
            identity,
            owner: NEXT_LOCK_OWNER.fetch_add(1, Ordering::Relaxed),
            held: LocalLocks::default(),
        }
    }

    pub(super) fn lock(&mut self, file: &File, level: c_int) -> c_int {
        if self.held.poisoned {
            return ffi::SQLITE_IOERR_LOCK;
        }
        if !matches!(
            level,
            ffi::SQLITE_LOCK_SHARED | ffi::SQLITE_LOCK_RESERVED | ffi::SQLITE_LOCK_EXCLUSIVE
        ) {
            return ffi::SQLITE_IOERR_LOCK;
        }
        if level <= self.held.level {
            return ffi::SQLITE_OK;
        }
        let Ok(mut registry) = rollback_owners().lock() else {
            return ffi::SQLITE_IOERR_LOCK;
        };
        let owners = registry.entry(self.identity).or_default();
        match level {
            ffi::SQLITE_LOCK_SHARED => {
                if self.held.level != ffi::SQLITE_LOCK_NONE
                    || local_conflict(owners, self.owner, |locks| {
                        locks.pending || locks.exclusive || locks.poisoned
                    })
                {
                    return ffi::SQLITE_BUSY;
                }
                if let Err(error) = lock_range(file, PENDING_BYTE, 1, false) {
                    return lock_error(&error);
                }
                let shared = lock_range(file, SHARED_FIRST, SHARED_SIZE, false);
                let pending_release = unlock_range(file, PENDING_BYTE, 1);
                if pending_release.is_err() {
                    self.held.pending = true;
                    self.held.poisoned = true;
                }
                match shared {
                    Ok(()) => {
                        self.held.shared = true;
                        self.held.level = ffi::SQLITE_LOCK_SHARED;
                        owners.insert(self.owner, self.held);
                        if self.held.poisoned {
                            ffi::SQLITE_IOERR_LOCK
                        } else {
                            ffi::SQLITE_OK
                        }
                    }
                    Err(error) => {
                        if self.held.poisoned {
                            owners.insert(self.owner, self.held);
                            ffi::SQLITE_IOERR_LOCK
                        } else {
                            lock_error(&error)
                        }
                    }
                }
            }
            ffi::SQLITE_LOCK_RESERVED => {
                if self.held.level != ffi::SQLITE_LOCK_SHARED || !self.held.shared {
                    return ffi::SQLITE_IOERR_LOCK;
                }
                if local_conflict(owners, self.owner, |locks| {
                    locks.reserved || locks.pending || locks.exclusive || locks.poisoned
                }) {
                    return ffi::SQLITE_BUSY;
                }
                if let Err(error) = lock_range(file, RESERVED_BYTE, 1, true) {
                    return lock_error(&error);
                }
                self.held.reserved = true;
                self.held.level = ffi::SQLITE_LOCK_RESERVED;
                owners.insert(self.owner, self.held);
                ffi::SQLITE_OK
            }
            ffi::SQLITE_LOCK_EXCLUSIVE => {
                if self.held.level < ffi::SQLITE_LOCK_SHARED || !self.held.shared {
                    return ffi::SQLITE_IOERR_LOCK;
                }
                if !self.held.pending {
                    if local_conflict(owners, self.owner, |locks| {
                        locks.pending || locks.exclusive || locks.poisoned
                    }) {
                        return ffi::SQLITE_BUSY;
                    }
                    if let Err(error) = lock_range(file, PENDING_BYTE, 1, true) {
                        return lock_error(&error);
                    }
                    self.held.pending = true;
                    self.held.level = ffi::SQLITE_LOCK_PENDING;
                    owners.insert(self.owner, self.held);
                }
                // Retain PENDING after a failed upgrade, just as SQLite's
                // Win32 VFS does. It blocks new readers while old readers
                // drain and the pager retries xLock(EXCLUSIVE).
                if local_conflict(owners, self.owner, |locks| locks.shared || locks.poisoned) {
                    return ffi::SQLITE_BUSY;
                }
                if unlock_range(file, SHARED_FIRST, SHARED_SIZE).is_err() {
                    self.held.poisoned = true;
                    owners.insert(self.owner, self.held);
                    return ffi::SQLITE_IOERR_LOCK;
                }
                self.held.shared = false;
                owners.insert(self.owner, self.held);
                match lock_range(file, SHARED_FIRST, SHARED_SIZE, true) {
                    Ok(()) => {
                        self.held.exclusive = true;
                        self.held.level = ffi::SQLITE_LOCK_EXCLUSIVE;
                        owners.insert(self.owner, self.held);
                        ffi::SQLITE_OK
                    }
                    Err(error) => {
                        if lock_range(file, SHARED_FIRST, SHARED_SIZE, false).is_err() {
                            self.held.poisoned = true;
                            owners.insert(self.owner, self.held);
                            return ffi::SQLITE_IOERR_LOCK;
                        }
                        self.held.shared = true;
                        owners.insert(self.owner, self.held);
                        lock_error(&error)
                    }
                }
            }
            _ => ffi::SQLITE_IOERR_LOCK,
        }
    }

    pub(super) fn unlock(&mut self, file: &File, level: c_int) -> c_int {
        if !matches!(level, ffi::SQLITE_LOCK_NONE | ffi::SQLITE_LOCK_SHARED) {
            return ffi::SQLITE_IOERR_UNLOCK;
        }
        if self.held.poisoned {
            return ffi::SQLITE_IOERR_UNLOCK;
        }
        if self.held.level <= level {
            return ffi::SQLITE_OK;
        }
        let Ok(mut registry) = rollback_owners().lock() else {
            return ffi::SQLITE_IOERR_UNLOCK;
        };
        let owners = registry.entry(self.identity).or_default();
        if self.held.exclusive {
            if unlock_range(file, SHARED_FIRST, SHARED_SIZE).is_err() {
                self.held.poisoned = true;
                owners.insert(self.owner, self.held);
                return ffi::SQLITE_IOERR_UNLOCK;
            }
            self.held.exclusive = false;
            if level == ffi::SQLITE_LOCK_SHARED {
                if lock_range(file, SHARED_FIRST, SHARED_SIZE, false).is_err() {
                    self.held.poisoned = true;
                    owners.insert(self.owner, self.held);
                    return ffi::SQLITE_IOERR_UNLOCK;
                }
                self.held.shared = true;
            }
        }
        if self.held.reserved {
            if unlock_range(file, RESERVED_BYTE, 1).is_err() {
                self.held.poisoned = true;
                owners.insert(self.owner, self.held);
                return ffi::SQLITE_IOERR_UNLOCK;
            }
            self.held.reserved = false;
        }
        if level == ffi::SQLITE_LOCK_NONE && self.held.shared {
            if unlock_range(file, SHARED_FIRST, SHARED_SIZE).is_err() {
                self.held.poisoned = true;
                owners.insert(self.owner, self.held);
                return ffi::SQLITE_IOERR_UNLOCK;
            }
            self.held.shared = false;
        }
        if self.held.pending {
            if unlock_range(file, PENDING_BYTE, 1).is_err() {
                self.held.poisoned = true;
                owners.insert(self.owner, self.held);
                return ffi::SQLITE_IOERR_UNLOCK;
            }
            self.held.pending = false;
        }
        self.held.level = level;
        if level == ffi::SQLITE_LOCK_NONE {
            owners.remove(&self.owner);
            if owners.is_empty() {
                registry.remove(&self.identity);
            }
        } else {
            owners.insert(self.owner, self.held);
        }
        ffi::SQLITE_OK
    }

    pub(super) fn check_reserved(&self, file: &File, result: &mut c_int) -> c_int {
        *result = 1;
        if self.held.poisoned {
            return ffi::SQLITE_IOERR_CHECKRESERVEDLOCK;
        }
        let Ok(registry) = rollback_owners().lock() else {
            return ffi::SQLITE_IOERR_CHECKRESERVEDLOCK;
        };
        if self.held.level >= ffi::SQLITE_LOCK_RESERVED
            || registry.get(&self.identity).is_some_and(|owners| {
                owners.iter().any(|(&owner, locks)| {
                    owner != self.owner
                        && (locks.reserved || locks.pending || locks.exclusive || locks.poisoned)
                })
            })
        {
            return ffi::SQLITE_OK;
        }
        match lock_range(file, RESERVED_BYTE, 1, false) {
            Ok(()) => {
                if unlock_range(file, RESERVED_BYTE, 1).is_err() {
                    return ffi::SQLITE_IOERR_CHECKRESERVEDLOCK;
                }
                *result = 0;
                ffi::SQLITE_OK
            }
            Err(error) if lock_error(&error) == ffi::SQLITE_BUSY => ffi::SQLITE_OK,
            Err(_) => ffi::SQLITE_IOERR_CHECKRESERVEDLOCK,
        }
    }

    pub(super) fn close(self, file: File) -> c_int {
        // Keep the registry mutex through CloseHandle: another guarded
        // handle must not pass a local-owner check while this handle still
        // owns kernel byte-range locks.
        let Ok(mut registry) = rollback_owners().lock() else {
            drop(file);
            return ffi::SQLITE_IOERR_CLOSE;
        };
        drop(file);
        if let Some(owners) = registry.get_mut(&self.identity) {
            owners.remove(&self.owner);
            if owners.is_empty() {
                registry.remove(&self.identity);
            }
        }
        ffi::SQLITE_OK
    }
}

pub(super) struct PinnedParent {
    path: PathBuf,
    directory: File,
    identity: Identity,
}

fn invalid_path() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "non-plain code-map path")
}

fn invalid_member() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "code-map member is not a regular file or is a reparse point",
    )
}

fn wide_name(name: &OsStr) -> io::Result<Vec<u16>> {
    let wide: Vec<u16> = name.encode_wide().collect();
    // A colon addresses an alternate data stream rather than a file leaf.
    // Reject the other reserved Windows name characters here as well, before
    // any relative native open or name-based metadata query.
    const RESERVED: &[u8] = b"<>:\"/\\|?*";
    if wide.is_empty()
        || wide
            .iter()
            .any(|&unit| unit < 32 || RESERVED.iter().any(|&byte| unit == u16::from(byte)))
    {
        return Err(invalid_path());
    }
    Ok(wide)
}

#[cfg(test)]
mod tests {
    use super::wide_name;
    use std::ffi::OsStr;

    #[test]
    fn relative_leaf_refuses_alternate_stream_and_reserved_names() {
        for name in ["map.db:stream", "map?.db", "map|db", "map\u{0001}db"] {
            assert!(wide_name(OsStr::new(name)).is_err(), "accepted {name:?}");
        }
        assert!(wide_name(OsStr::new("code-map.db")).is_ok());
    }
}

fn unicode_name(wide: &mut [u16]) -> io::Result<UNICODE_STRING> {
    let length = wide
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(invalid_path)?;
    Ok(UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: wide.as_mut_ptr(),
    })
}

fn nt_error(status: i32) -> io::Error {
    // SAFETY: mapping an NTSTATUS to a Win32 error has no pointer preconditions.
    io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
}

fn open_relative(
    parent: &File,
    name: &OsStr,
    desired_access: u32,
    disposition: u32,
    directory: bool,
) -> io::Result<File> {
    open_relative_shared(
        parent,
        name,
        desired_access,
        disposition,
        directory,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )
}

fn open_relative_shared(
    parent: &File,
    name: &OsStr,
    desired_access: u32,
    disposition: u32,
    directory: bool,
    share_mode: u32,
) -> io::Result<File> {
    let mut wide = wide_name(name)?;
    let name = unicode_name(&mut wide)?;
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &raw const name,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut io_status = IO_STATUS_BLOCK::default();
    let mut handle: HANDLE = std::ptr::null_mut();
    let options = if directory {
        FILE_DIRECTORY_FILE
    } else {
        FILE_NON_DIRECTORY_FILE
    } | FILE_OPEN_REPARSE_POINT
        | FILE_SYNCHRONOUS_IO_NONALERT;
    // SAFETY: all input structures and the name buffer remain live; the
    // pinned parent is a live directory handle; a returned handle is owned
    // immediately below.
    let status = unsafe {
        NtCreateFile(
            &raw mut handle,
            desired_access,
            &raw const attributes,
            &raw mut io_status,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            share_mode,
            disposition,
            options,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(nt_error(status));
    }
    // SAFETY: NtCreateFile returned a newly owned handle.
    Ok(unsafe { File::from_raw_handle(handle) })
}

fn identity(file: &File) -> io::Result<Identity> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: file is live; info is the correctly sized output buffer.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&raw mut info).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if info.FileId.Identifier == [0; 16] {
        return Err(invalid_member());
    }
    Ok(Identity {
        volume: info.VolumeSerialNumber,
        file_id: info.FileId.Identifier,
    })
}

fn verify_kind(file: &File, directory: bool) -> io::Result<()> {
    let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: file is live; tag is the correctly sized output buffer.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&raw mut tag).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let is_directory = tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || tag.ReparseTag != 0
        || is_directory != directory
    {
        return Err(invalid_member());
    }
    Ok(())
}

impl PinnedParent {
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(invalid_path());
        }
        let mut components = path.components();
        let Component::Prefix(prefix) = components.next().ok_or_else(invalid_path)? else {
            return Err(invalid_path());
        };
        if !matches!(components.next(), Some(Component::RootDir)) {
            return Err(invalid_path());
        }
        let mut root = OsString::from(prefix.as_os_str());
        root.push("\\");
        let mut root_wide: Vec<u16> = root.encode_wide().collect();
        root_wide.push(0);
        // SAFETY: root_wide is a live NUL-terminated root path; the returned
        // handle is owned immediately below.
        let root_handle = unsafe {
            CreateFileW(
                root_wide.as_ptr(),
                GENERIC_READ | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null(),
            )
        };
        if root_handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateFileW returned a newly owned handle.
        let mut directory = unsafe { File::from_raw_handle(root_handle) };
        verify_kind(&directory, true)?;
        for component in components {
            let Component::Normal(name) = component else {
                return Err(invalid_path());
            };
            directory = open_relative(
                &directory,
                name,
                GENERIC_READ | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                FILE_OPEN,
                true,
            )?;
            verify_kind(&directory, true)?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            identity: identity(&directory)?,
            directory,
        })
    }

    pub(super) fn identity(&self) -> Identity {
        self.identity
    }

    pub(super) fn stat_child(&self, name: &OsStr) -> io::Result<Option<Observed>> {
        // NtQueryDirectoryFile captures its name filter on the first query
        // against a directory file object. Reopen the no-follow parent for
        // each child name, and reject a path replacement before querying it.
        let query_parent = Self::open(&self.path)?;
        if query_parent.identity != self.identity {
            return Err(invalid_member());
        }
        let mut wide = wide_name(name)?;
        let filter = unicode_name(&mut wide)?;
        let mut io_status = IO_STATUS_BLOCK::default();
        // A directory query obtains ID, kind, and reparse information
        // without opening and closing the child sidecar.
        let mut buffer = [0u64; 1024];
        // SAFETY: the parent is a live directory handle; the output buffer
        // and filter are live and correctly sized throughout the query.
        let status = unsafe {
            NtQueryDirectoryFile(
                query_parent.directory.as_raw_handle(),
                std::ptr::null_mut(),
                None,
                std::ptr::null(),
                &raw mut io_status,
                buffer.as_mut_ptr().cast(),
                std::mem::size_of_val(&buffer) as u32,
                FileIdExtdDirectoryInformation,
                true,
                &raw const filter,
                true,
            )
        };
        if matches!(
            status,
            STATUS_NO_MORE_FILES | STATUS_NO_SUCH_FILE | STATUS_OBJECT_NAME_NOT_FOUND
        ) {
            return Ok(None);
        }
        if status < 0 {
            return Err(nt_error(status));
        }
        // SAFETY: a successful query populated at least the fixed portion
        // of one FILE_ID_EXTD_DIR_INFORMATION entry in this aligned buffer.
        let entry = unsafe { &*(buffer.as_ptr().cast::<FILE_ID_EXTD_DIR_INFORMATION>()) };
        let returned_name_length = entry.FileNameLength as usize / std::mem::size_of::<u16>();
        let fixed = std::mem::offset_of!(FILE_ID_EXTD_DIR_INFORMATION, FileName);
        if entry.FileNameLength as usize % 2 != 0
            || fixed + entry.FileNameLength as usize > io_status.Information
            || fixed + entry.FileNameLength as usize > std::mem::size_of_val(&buffer)
        {
            return Err(invalid_member());
        }
        // SAFETY: the length was checked against the query's output buffer.
        let returned_name =
            unsafe { std::slice::from_raw_parts(entry.FileName.as_ptr(), returned_name_length) };
        if returned_name != wide {
            return Err(invalid_member());
        }
        if entry.FileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
            || entry.ReparsePointTag != 0
            || entry.FileId.Identifier == [0; 16]
        {
            return Err(invalid_member());
        }
        let name = unicode_name(&mut wide)?;
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: self.directory.as_raw_handle(),
            ObjectName: &raw const name,
            Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut stat = FILE_STAT_INFORMATION::default();
        let mut stat_status = IO_STATUS_BLOCK::default();
        // Unlike an open/close of the sidecar, this name query obtains its
        // link count without creating a child file object or touching SQLite.
        // An unsupported information class/filesystem fails closed.
        // SAFETY: the pinned directory, name, attributes and output buffers
        // remain live throughout the native metadata query.
        let status = unsafe {
            NtQueryInformationByName(
                &raw const attributes,
                &raw mut stat_status,
                (&raw mut stat).cast(),
                std::mem::size_of::<FILE_STAT_INFORMATION>() as u32,
                FileStatInformation,
            )
        };
        if status < 0 {
            return Err(nt_error(status));
        }
        let file_id = stat.FileId.to_le_bytes();
        // FileStatInformation supplies only a 64-bit file ID. Accept it only
        // when the directory's 128-bit ID has a zero upper half and agrees
        // with that low half; otherwise the two observations cannot prove
        // that the link count belongs to the same child.
        if entry.FileId.Identifier[..8] != file_id
            || entry.FileId.Identifier[8..] != [0; 8]
            || stat.FileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
            || stat.ReparseTag != 0
            || stat.NumberOfLinks == 0
        {
            return Err(invalid_member());
        }
        Ok(Some(Observed {
            identity: Identity {
                volume: self.identity.volume,
                file_id: entry.FileId.Identifier,
            },
            links: u64::from(stat.NumberOfLinks),
        }))
    }

    pub(super) fn open_child(&self, name: &OsStr, access: OpenAccess) -> io::Result<File> {
        let (rights, disposition) = match access {
            OpenAccess::ReadOnly => (GENERIC_READ, FILE_OPEN),
            OpenAccess::ReadWrite => (GENERIC_READ | GENERIC_WRITE, FILE_OPEN),
            OpenAccess::Create => (GENERIC_READ | GENERIC_WRITE, FILE_OPEN_IF),
            OpenAccess::CreateNew => (GENERIC_READ | GENERIC_WRITE, FILE_CREATE),
        };
        let file = open_relative(
            &self.directory,
            name,
            rights | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            disposition,
            false,
        )?;
        // A reparse-object handle is not a protected regular-file alias;
        // refuse it without spending a quarantine slot.
        verify_kind(&file, false)?;
        Ok(file)
    }

    pub(super) fn open_delete_child(&self, name: &OsStr) -> io::Result<Option<File>> {
        let file = match open_relative_shared(
            &self.directory,
            name,
            DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_OPEN,
            false,
            // Prevent another opener from acquiring DELETE access to rename
            // this leaf between the handle proof and disposition.
            FILE_SHARE_READ | FILE_SHARE_WRITE,
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        verify_kind(&file, false)?;
        Ok(Some(file))
    }

    pub(super) fn delete_opened_child(&self, file: File, _sync_parent: bool) -> io::Result<()> {
        let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: file is live; disposition is the correctly sized input.
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                (&raw mut disposition).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        drop(file);
        // SQLite's Win32 VFS also ignores xDelete's syncDir argument: a
        // Windows directory HANDLE does not provide a portable fsync.
        Ok(())
    }
}

pub(super) fn observe_file(file: &File) -> io::Result<Observed> {
    verify_kind(file, false)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file is live; info is the correctly sized output buffer.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.nNumberOfLinks == 0 {
        return Err(invalid_member());
    }
    Ok(Observed {
        identity: identity(file)?,
        links: u64::from(info.nNumberOfLinks),
    })
}

pub(super) fn sqlite_header_mode(file: &File) -> io::Result<Option<(u8, u8)>> {
    let size = file.metadata()?.len();
    if size == 0 {
        return Ok(None);
    }
    if size < 20 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "existing SQLite main is shorter than its header",
        ));
    }
    let mut header = [0u8; 20];
    let mut read = 0;
    while read < header.len() {
        let count = file.seek_read(&mut header[read..], read as u64)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "SQLite header changed during admission",
            ));
        }
        read += count;
    }
    if &header[..16] != b"SQLite format 3\0" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "existing code-map main is not a SQLite database",
        ));
    }
    Ok(Some((header[18], header[19])))
}

pub(super) fn close_unlocked(file: File, _identity: Identity) {
    drop(file);
}
