use super::{
    ensure_not_symlink_or_reparse, ensure_portable_ancestors_not_symlinks,
    windows_attribute_tag_is_acceptable, windows_final_path_matches, ExternalIdsWriteError,
};
use std::ffi::{c_void, OsStr};
use std::io::Write as _;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use std::path::Path;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FileNamesInformation, NtCreateFile, NtQueryDirectoryFile, FILE_CREATE, FILE_DIRECTORY_FILE,
    FILE_NAMES_INFORMATION, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF,
    FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
};
use windows_sys::Win32::Foundation::{
    RtlNtStatusToDosError, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    OBJ_CASE_INSENSITIVE, STATUS_NO_MORE_FILES, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileAttributeTagInfo, FileDispositionInfo, FileIdInfo, FileRenameInfoEx,
    GetFileInformationByHandleEx, GetFinalPathNameByHandleW, SetFileInformationByHandle, DELETE,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_TAG_INFO, FILE_DISPOSITION_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_LIST_DIRECTORY,
    FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_RENAME_INFO_0,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, OPEN_EXISTING,
    SYNCHRONIZE, VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::WindowsProgramming::{
    FILE_RENAME_FLAG_POSIX_SEMANTICS, FILE_RENAME_FLAG_REPLACE_IF_EXISTS,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

const TMP_NAME: &str = "external_ids.bin.tmp";
const FINAL_NAME: &str = "external_ids.bin";

#[cfg(test)]
#[path = "external_ids_windows_tests.rs"]
mod tests;

pub(super) fn write_via_dir_handle(dir: &Path, buf: &[u8]) -> Result<(), ExternalIdsWriteError> {
    write_via_dir_handle_with(dir, buf, rename_relative)
}

/// The checkpoint writer shares the sidecar's verified directory-handle
/// boundary. All later operations are relative to this pinned handle.
pub(crate) fn open_checkpoint_directory(dir: &Path) -> std::io::Result<std::fs::File> {
    open_checkpoint_directory_with_access(dir, FILE_READ_ATTRIBUTES)
}

pub(crate) fn open_checkpoint_directory_for_listing(dir: &Path) -> std::io::Result<std::fs::File> {
    open_checkpoint_directory_with_access(
        dir,
        FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY | SYNCHRONIZE,
    )
}

fn open_checkpoint_directory_with_access(
    dir: &Path,
    desired_access: u32,
) -> std::io::Result<std::fs::File> {
    ensure_portable_ancestors_not_symlinks(dir, "inspect checkpoint dir ancestor")
        .map_err(std::io::Error::other)?;
    let metadata = ensure_not_symlink_or_reparse(dir, "inspect checkpoint dir")
        .map_err(std::io::Error::other)?
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
    if !metadata.is_dir() {
        return Err(std::io::Error::from(std::io::ErrorKind::NotADirectory));
    }
    let expected = std::fs::canonicalize(dir)?;
    open_verified_directory_with(dir, &expected, desired_access, || {})
        .map_err(std::io::Error::other)
}

fn open_verified_directory_with(
    dir: &Path,
    expected: &Path,
    desired_access: u32,
    before_retained_open: impl FnOnce(),
) -> Result<std::fs::File, ExternalIdsWriteError> {
    let pinned = open_directory_component_walk(expected)?;
    let expected_identity = directory_identity(&pinned)?;
    let expected_wide: Vec<u16> = expected.as_os_str().encode_wide().collect();
    if !windows_final_path_matches(&expected_wide, &final_path(&pinned)?) {
        return Err(ExternalIdsWriteError::DirectoryIdentityChanged);
    }

    before_retained_open();
    let retained = open_directory(dir, desired_access)?;
    verify_handle_kind(&retained, true, "inspect retained checkpoint dir")?;
    // A directory replaced at the same pathname has the same final path but
    // a different native identity. Keep the checked handle alive until the
    // independently opened handle is bound to it.
    if directory_identity(&retained)? != expected_identity {
        return Err(ExternalIdsWriteError::DirectoryIdentityChanged);
    }
    if !windows_final_path_matches(&expected_wide, &final_path(&retained)?) {
        return Err(ExternalIdsWriteError::DirectoryIdentityChanged);
    }
    Ok(retained)
}

fn open_directory_component_walk(path: &Path) -> Result<std::fs::File, ExternalIdsWriteError> {
    use std::path::Component;

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(ExternalIdsWriteError::InvalidPath {
            context: "pin checkpoint root",
            detail: "canonical directory has no Windows prefix".into(),
        });
    };
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(ExternalIdsWriteError::InvalidPath {
            context: "pin checkpoint root",
            detail: "canonical directory is not rooted".into(),
        });
    }
    let mut root = std::path::PathBuf::from(prefix.as_os_str());
    root.push("\\");
    let access = FILE_READ_ATTRIBUTES | FILE_TRAVERSE | SYNCHRONIZE;
    let mut current = open_directory(&root, access)?;
    verify_handle_kind(&current, true, "inspect checkpoint root handle")?;
    for component in components {
        let Component::Normal(name) = component else {
            return Err(ExternalIdsWriteError::InvalidPath {
                context: "pin checkpoint directory component",
                detail: "canonical directory contains a non-normal component".into(),
            });
        };
        let child =
            open_relative_with_options(&current, name, access, FILE_OPEN, FILE_DIRECTORY_FILE)
                .map_err(|error| {
                    ExternalIdsWriteError::io("open checkpoint directory component", error)
                })?;
        verify_handle_kind(
            &child,
            true,
            "inspect checkpoint directory component handle",
        )?;
        current = child;
    }
    Ok(current)
}

fn directory_identity(directory: &std::fs::File) -> Result<(u64, [u8; 16]), ExternalIdsWriteError> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: the live handle and correctly sized writable FileIdInfo buffer
    // remain valid for this synchronous query.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            directory.as_raw_handle(),
            FileIdInfo,
            (&raw mut info).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(ExternalIdsWriteError::io(
            "inspect checkpoint directory file identity",
            std::io::Error::last_os_error(),
        ));
    }
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

pub(crate) fn list_checkpoint_names_bounded(
    dir: &std::fs::File,
    max_entries: usize,
) -> std::io::Result<(Vec<String>, bool)> {
    let mut names = Vec::new();
    let mut visited = 0;
    let mut restart = true;
    loop {
        // A single entry needs at most 12 header bytes plus 255 UTF-16 code
        // units; the aligned buffer also covers longer provider names.
        let mut buffer = [0u64; 512];
        let mut status_block = IO_STATUS_BLOCK::default();
        // SAFETY: the directory handle and writable output buffers remain live
        // through the synchronous query. ReturnSingleEntry limits each visit.
        let status = unsafe {
            NtQueryDirectoryFile(
                dir.as_raw_handle(),
                std::ptr::null_mut(),
                None,
                std::ptr::null(),
                &raw mut status_block,
                buffer.as_mut_ptr().cast(),
                std::mem::size_of_val(&buffer) as u32,
                FileNamesInformation,
                true,
                std::ptr::null(),
                restart,
            )
        };
        restart = false;
        if status == STATUS_NO_MORE_FILES {
            return Ok((names, false));
        }
        if status != 0 {
            // SAFETY: translating an NTSTATUS has no preconditions.
            let code = unsafe { RtlNtStatusToDosError(status) };
            return Err(std::io::Error::from_raw_os_error(code as i32));
        }
        let name_offset = std::mem::offset_of!(FILE_NAMES_INFORMATION, FileName);
        if status_block.Information < name_offset {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "checkpoint directory entry is truncated",
            ));
        }
        // SAFETY: the aligned buffer contains the completed entry header.
        let entry = unsafe { &*(buffer.as_ptr().cast::<FILE_NAMES_INFORMATION>()) };
        let name_bytes = entry.FileNameLength as usize;
        if !name_bytes.is_multiple_of(2)
            || name_bytes > status_block.Information.saturating_sub(name_offset)
            || status_block.Information > std::mem::size_of_val(&buffer)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "checkpoint directory entry has invalid name length",
            ));
        }
        // SAFETY: the name length was checked against the completed buffer.
        let units = unsafe { std::slice::from_raw_parts(entry.FileName.as_ptr(), name_bytes / 2) };
        if units == [b'.' as u16] || units == [b'.' as u16, b'.' as u16] {
            continue;
        }
        if visited == max_entries {
            return Ok((names, true));
        }
        visited += 1;
        if let Ok(name) = String::from_utf16(units) {
            names.push(name);
        }
    }
}

pub(crate) fn open_checkpoint_lock(dir: &std::fs::File) -> std::io::Result<std::fs::File> {
    let file = open_relative(
        dir,
        ".checkpoint.lock",
        GENERIC_READ | GENERIC_WRITE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        FILE_OPEN_IF,
    )?;
    verify_handle_kind(&file, false, "inspect opened checkpoint lock")
        .map_err(std::io::Error::other)?;
    Ok(file)
}

pub(crate) fn open_checkpoint_read_file(
    dir: &std::fs::File,
    name: &str,
) -> std::io::Result<Option<std::fs::File>> {
    let file = match open_relative(
        dir,
        name,
        GENERIC_READ | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        FILE_OPEN,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    verify_handle_kind(&file, false, "inspect opened checkpoint sidecar")
        .map_err(std::io::Error::other)?;
    Ok(Some(file))
}

pub(crate) fn stage_checkpoint_file(
    dir: &std::fs::File,
    name: &str,
    bytes: &[u8],
) -> std::io::Result<()> {
    remove_relative_if_exists(dir, name, "inspect stale checkpoint staging file")
        .map_err(std::io::Error::other)?;
    let mut file = open_relative(
        dir,
        name,
        GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        FILE_CREATE,
    )?;
    verify_handle_kind(&file, false, "inspect created checkpoint staging file")
        .map_err(std::io::Error::other)?;
    file.write_all(bytes)?;
    file.sync_all()
}

pub(crate) fn rename_checkpoint_file(
    dir: &std::fs::File,
    from: &str,
    to: &str,
) -> std::io::Result<()> {
    let file = open_relative(
        dir,
        from,
        DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        FILE_OPEN,
    )?;
    verify_handle_kind(
        &file,
        false,
        "inspect checkpoint staging file before rename",
    )
    .map_err(std::io::Error::other)?;
    rename_relative(&file, dir, to)
}

pub(crate) fn remove_checkpoint_file(dir: &std::fs::File, name: &str) -> std::io::Result<()> {
    remove_relative_if_exists(dir, name, "remove checkpoint sidecar").map_err(std::io::Error::other)
}

fn write_via_dir_handle_with(
    dir: &Path,
    buf: &[u8],
    rename: impl FnOnce(&std::fs::File, &std::fs::File, &str) -> std::io::Result<()>,
) -> Result<(), ExternalIdsWriteError> {
    lexical_prefilter(dir)?;
    let expected = std::fs::canonicalize(dir)
        .map_err(|error| ExternalIdsWriteError::io("canonicalize segment dir", error))?;
    lexical_prefilter(dir)?;
    let dir_file = open_verified_directory_with(dir, &expected, FILE_READ_ATTRIBUTES, || {})?;

    remove_relative_if_exists(&dir_file, TMP_NAME, "remove stale external_ids.bin.tmp")?;
    let mut tmp_file = open_relative(
        &dir_file,
        TMP_NAME,
        GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        FILE_CREATE,
    )
    .map_err(|error| ExternalIdsWriteError::io("create external_ids.bin.tmp", error))?;
    verify_handle_kind(&tmp_file, false, "inspect created external_ids.bin.tmp")?;
    tmp_file
        .write_all(buf)
        .map_err(|error| ExternalIdsWriteError::io("write external_ids.bin.tmp", error))?;
    tmp_file
        .sync_all()
        .map_err(|error| ExternalIdsWriteError::io("sync external_ids.bin.tmp", error))?;

    rename(&tmp_file, &dir_file, FINAL_NAME).map_err(|error| {
        ExternalIdsWriteError::io("rename external_ids.bin.tmp -> external_ids.bin", error)
    })
}

fn lexical_prefilter(dir: &Path) -> Result<(), ExternalIdsWriteError> {
    ensure_portable_ancestors_not_symlinks(dir, "inspect segment dir ancestor")?;
    let metadata = ensure_not_symlink_or_reparse(dir, "inspect segment dir")?.ok_or_else(|| {
        ExternalIdsWriteError::io(
            "inspect segment dir",
            std::io::Error::new(std::io::ErrorKind::NotFound, "segment dir does not exist"),
        )
    })?;
    if !metadata.is_dir() {
        return Err(ExternalIdsWriteError::InvalidPath {
            context: "inspect segment dir",
            detail: "path is not a directory".into(),
        });
    }
    ensure_not_symlink_or_reparse(&dir.join(TMP_NAME), "inspect external_ids.bin.tmp")?;
    ensure_not_symlink_or_reparse(&dir.join(FINAL_NAME), "inspect external_ids.bin")?;
    Ok(())
}

fn open_directory(dir: &Path, desired_access: u32) -> Result<std::fs::File, ExternalIdsWriteError> {
    let wide = nul_terminated(dir.as_os_str(), "segment dir path")?;
    // SAFETY: `wide` is NUL-terminated and live for the call. A successful
    // handle is uniquely transferred into `File` below.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            desired_access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(ExternalIdsWriteError::io(
            "open segment dir without following reparse point",
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: `handle` is newly returned and transferred exactly once.
    Ok(unsafe { std::fs::File::from_raw_handle(handle) })
}

fn nul_terminated(value: &OsStr, context: &'static str) -> Result<Vec<u16>, ExternalIdsWriteError> {
    let mut wide: Vec<u16> = value.encode_wide().collect();
    if wide.contains(&0) {
        return Err(ExternalIdsWriteError::InvalidPath {
            context,
            detail: "path contains an interior NUL".into(),
        });
    }
    wide.push(0);
    Ok(wide)
}

fn verify_handle_kind(
    file: &std::fs::File,
    require_directory: bool,
    context: &'static str,
) -> Result<(), ExternalIdsWriteError> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: the handle is live and `info` is a correctly sized writable
    // buffer for `FileAttributeTagInfo`.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&raw mut info).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(ExternalIdsWriteError::io(
            context,
            std::io::Error::last_os_error(),
        ));
    }
    if !windows_attribute_tag_is_acceptable(info.FileAttributes, info.ReparseTag, require_directory)
    {
        return Err(ExternalIdsWriteError::InvalidPath {
            context,
            detail: "opened handle has the wrong kind or is a reparse point".into(),
        });
    }
    Ok(())
}

fn final_path(file: &std::fs::File) -> Result<Vec<u16>, ExternalIdsWriteError> {
    let mut path = vec![0u16; 260];
    loop {
        // SAFETY: the handle is live and `path` exposes a writable buffer of
        // the supplied length.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                path.as_mut_ptr(),
                path.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
        if length == 0 {
            return Err(ExternalIdsWriteError::io(
                "resolve opened segment dir",
                std::io::Error::last_os_error(),
            ));
        }
        let length = length as usize;
        if length < path.len() {
            path.truncate(length);
            return Ok(path);
        }
        path.resize(length.saturating_add(1), 0);
    }
}

fn open_relative(
    dir: &std::fs::File,
    name: &str,
    desired_access: u32,
    create_disposition: u32,
) -> std::io::Result<std::fs::File> {
    open_relative_with_options(
        dir,
        OsStr::new(name),
        desired_access,
        create_disposition,
        FILE_NON_DIRECTORY_FILE,
    )
}

fn open_relative_with_options(
    dir: &std::fs::File,
    name: &OsStr,
    desired_access: u32,
    create_disposition: u32,
    kind_option: u32,
) -> std::io::Result<std::fs::File> {
    let mut wide: Vec<u16> = name.encode_wide().collect();
    let byte_len = wide
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidFilename))?;
    let unicode_name = UNICODE_STRING {
        Length: byte_len,
        MaximumLength: byte_len,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: dir.as_raw_handle(),
        ObjectName: &raw const unicode_name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut io_status = IO_STATUS_BLOCK::default();
    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: every input structure and name buffer is live for the call; the
    // root handle is live, and a successful child handle is transferred below.
    let status = unsafe {
        NtCreateFile(
            &raw mut handle,
            desired_access,
            &raw const attributes,
            &raw mut io_status,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            create_disposition,
            kind_option | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        // SAFETY: converting the returned failure status has no preconditions.
        let error = unsafe { RtlNtStatusToDosError(status) };
        return Err(std::io::Error::from_raw_os_error(error as i32));
    }
    // SAFETY: `handle` is newly returned and transferred exactly once.
    Ok(unsafe { std::fs::File::from_raw_handle(handle) })
}

fn remove_relative_if_exists(
    dir: &std::fs::File,
    name: &str,
    context: &'static str,
) -> Result<(), ExternalIdsWriteError> {
    let file = match open_relative(
        dir,
        name,
        DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        FILE_OPEN,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(ExternalIdsWriteError::io(context, error)),
    };
    verify_handle_kind(&file, false, context)?;
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the handle was opened with `DELETE`, and `disposition` is the
    // correctly sized input for `FileDispositionInfo`.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&raw const disposition).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(ExternalIdsWriteError::io(
            context,
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn rename_relative(
    file: &std::fs::File,
    dir: &std::fs::File,
    target_name: &str,
) -> std::io::Result<()> {
    let wide: Vec<u16> = OsStr::new(target_name).encode_wide().collect();
    let name_bytes = wide
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|length| u32::try_from(length).ok())
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidFilename))?;
    let header_size = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let total_size = header_size
        .checked_add(name_bytes as usize)
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::FileTooLarge))?;
    let mut storage = vec![0usize; total_size.div_ceil(std::mem::size_of::<usize>())];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: `storage` has the structure's alignment and enough bytes for
    // the fixed header plus the complete relative target name.
    unsafe {
        (*info).Anonymous = FILE_RENAME_INFO_0 {
            Flags: FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS,
        };
        (*info).RootDirectory = dir.as_raw_handle();
        (*info).FileNameLength = name_bytes;
        std::ptr::copy_nonoverlapping(
            wide.as_ptr(),
            (&raw mut (*info).FileName).cast::<u16>(),
            wide.len(),
        );
    }
    // SAFETY: both handles are live, the source was opened with `DELETE`, and
    // `info` points at the initialized `total_size`-byte replacement input.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileRenameInfoEx,
            info.cast::<c_void>(),
            total_size as u32,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
