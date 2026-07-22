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
    NtCreateFile, FILE_CREATE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT,
};
use windows_sys::Win32::Foundation::{
    RtlNtStatusToDosError, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE,
    UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileAttributeTagInfo, FileDispositionInfo, FileRenameInfo,
    GetFileInformationByHandleEx, GetFinalPathNameByHandleW, SetFileInformationByHandle, DELETE,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_TAG_INFO, FILE_DISPOSITION_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED,
    FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_RENAME_INFO_0, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING, SYNCHRONIZE, VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

const TMP_NAME: &str = "external_ids.bin.tmp";
const FINAL_NAME: &str = "external_ids.bin";

pub(super) fn write_via_dir_handle(dir: &Path, buf: &[u8]) -> Result<(), ExternalIdsWriteError> {
    lexical_prefilter(dir)?;
    let expected = std::fs::canonicalize(dir)
        .map_err(|error| ExternalIdsWriteError::io("canonicalize segment dir", error))?;
    let expected_wide: Vec<u16> = expected.as_os_str().encode_wide().collect();
    lexical_prefilter(dir)?;

    let dir_file = open_directory(dir)?;
    verify_handle_kind(&dir_file, true, "inspect opened segment dir")?;
    let opened_path = final_path(&dir_file)?;
    if !windows_final_path_matches(&expected_wide, &opened_path) {
        return Err(ExternalIdsWriteError::DirectoryIdentityChanged);
    }

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

    remove_relative_if_exists(&dir_file, FINAL_NAME, "remove previous external_ids.bin")?;
    rename_relative(&tmp_file, &dir_file, FINAL_NAME).map_err(|error| {
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

fn open_directory(dir: &Path) -> Result<std::fs::File, ExternalIdsWriteError> {
    let wide = nul_terminated(dir.as_os_str(), "segment dir path")?;
    // SAFETY: `wide` is NUL-terminated and live for the call. A successful
    // handle is uniquely transferred into `File` below.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
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
    let mut wide: Vec<u16> = OsStr::new(name).encode_wide().collect();
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
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
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
            ReplaceIfExists: false,
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
    // `info` points at the initialized `total_size`-byte input buffer.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileRenameInfo,
            info.cast::<c_void>(),
            total_size as u32,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
