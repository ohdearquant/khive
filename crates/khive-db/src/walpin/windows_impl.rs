use super::{
    io_other, windows_attribute_tag_is_acceptable, windows_final_path_matches,
    windows_owner_dacl_is_restricted, windows_relative_child_name_is_safe,
};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::os::raw::c_void;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::time::SystemTime;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    NtCreateFile, FILE_CREATE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT,
};
use windows_sys::Win32::Foundation::{
    LocalFree, RtlNtStatusToDosError, ERROR_ALREADY_EXISTS, HANDLE, UNICODE_STRING,
};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    AclSizeInformation, AddAccessAllowedAceEx, EqualSid, GetAce, GetAclInformation, GetLengthSid,
    GetSecurityDescriptorControl, GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor,
    SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner, TokenUser,
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, CONTAINER_INHERIT_ACE,
    DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION, SECURITY_ATTRIBUTES,
    SECURITY_DESCRIPTOR, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, FileAttributeTagInfo, GetFileInformationByHandleEx, FILE_ALL_ACCESS,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, OPEN_EXISTING, READ_CONTROL,
    SYNCHRONIZE, VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

#[cfg(test)]
std::thread_local! {
    static OPEN_DIR_HANDLE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn open_dir_handle_call_count() -> usize {
    OPEN_DIR_HANDLE_CALLS.with(std::cell::Cell::get)
}

#[cfg(test)]
thread_local! {
    /// Runs after the target has been inspected but before its replacing
    /// rename, so a test can observe the old name at the exact seam.
    static BEFORE_TARGET_RENAME_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn set_before_target_rename_hook(hook: impl FnOnce() + 'static) {
    BEFORE_TARGET_RENAME_HOOK.with(|cell| *cell.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn take_before_target_rename_hook() -> Option<Box<dyn FnOnce()>> {
    BEFORE_TARGET_RENAME_HOOK.with(|cell| cell.borrow_mut().take())
}

fn to_wide_nul(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::from(io::ErrorKind::InvalidFilename));
    }
    wide.push(0);
    Ok(wide)
}

/// Open `path` with `FILE_FLAG_OPEN_REPARSE_POINT`, so a symlink or
/// junction planted at `path`'s own final component is opened AS that
/// reparse-point object itself, never followed. The returned `File`
/// owns the handle and closes it exactly once, on drop.
fn open_reparse_aware(
    path: &Path,
    access: u32,
    disposition: u32,
    extra_flags: u32,
) -> io::Result<fs::File> {
    let wide = to_wide_nul(path)?;
    // SAFETY: `wide` is a valid, NUL-terminated UTF-16 string for the
    // call's duration; the returned handle, on success, is uniquely
    // owned by this call and wrapped immediately below.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null_mut(),
            disposition,
            FILE_FLAG_OPEN_REPARSE_POINT | extra_flags,
            std::ptr::null_mut(),
        )
    };
    if handle == invalid_handle_value() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` was just returned by the successful `CreateFileW`
    // above; wrapping it in `File` binds its lifetime to this value so
    // it is closed exactly once, on drop.
    Ok(unsafe { fs::File::from_raw_handle(handle as RawHandle) })
}

fn verify_handle_kind(file: &fs::File, require_directory: bool) -> io::Result<()> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: the handle is live and `info` is the correctly sized output
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
        return Err(io::Error::last_os_error());
    }
    if !windows_attribute_tag_is_acceptable(info.FileAttributes, info.ReparseTag, require_directory)
    {
        return Err(io_other(
            "opened walpin sidecar handle has the wrong kind or is a reparse point",
        ));
    }
    Ok(())
}

fn final_path(file: &fs::File) -> io::Result<Vec<u16>> {
    let mut path = vec![0u16; 260];
    loop {
        // SAFETY: the handle is live and `path` exposes the supplied
        // writable buffer for the call.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle() as Handle,
                path.as_mut_ptr(),
                path.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        let length = length as usize;
        if length < path.len() {
            path.truncate(length);
            return Ok(path);
        }
        path.resize(length.saturating_add(1), 0);
    }
}

fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn ensure_ancestors_not_reparse(dir: &Path) -> io::Result<()> {
    const MAX_ANCESTORS: usize = 40;

    let parent = dir
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let ancestors: Vec<_> = parent
        .ancestors()
        .filter(|path| !path.as_os_str().is_empty())
        .take(MAX_ANCESTORS + 1)
        .collect();
    if ancestors.len() > MAX_ANCESTORS {
        return Err(io_other(format!(
            "walpin sidecar path has more than {MAX_ANCESTORS} ancestor components"
        )));
    }
    for ancestor in ancestors.into_iter().rev() {
        let metadata = fs::symlink_metadata(ancestor)?;
        if metadata.file_type().is_symlink() || metadata_is_reparse(&metadata) {
            return Err(io_other(format!(
                "walpin sidecar ancestor {ancestor:?} is a reparse point; refusing"
            )));
        }
        if !metadata.is_dir() {
            return Err(io_other(format!(
                "walpin sidecar ancestor {ancestor:?} is not a directory"
            )));
        }
    }
    Ok(())
}

fn lexical_prefilter(dir: &Path) -> io::Result<()> {
    ensure_ancestors_not_reparse(dir)?;
    let metadata = fs::symlink_metadata(dir)?;
    if metadata.file_type().is_symlink() || metadata_is_reparse(&metadata) {
        return Err(io_other(format!(
            "walpin sidecar path {dir:?} is a reparse point; refusing"
        )));
    }
    if !metadata.is_dir() {
        return Err(io_other(format!(
            "walpin sidecar path {dir:?} exists and is not a directory"
        )));
    }
    Ok(())
}

struct LocalSecurityDescriptor(*mut c_void);

impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `GetSecurityInfo` allocated this descriptor with
            // `LocalAlloc`; this guard releases it exactly once.
            unsafe { LocalFree(self.0) };
        }
    }
}

fn validate_owner_only_dacl(file: &fs::File, dir: &Path) -> io::Result<()> {
    let mut owner = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: the directory handle is live and all requested output
    // pointers remain valid for the call.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &raw mut owner,
            std::ptr::null_mut(),
            &raw mut dacl,
            std::ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let _descriptor = LocalSecurityDescriptor(descriptor);
    if owner.is_null() || dacl.is_null() || descriptor.is_null() {
        return Err(io_other(format!(
            "walpin sidecar dir {dir:?} has no owner-only DACL; refusing"
        )));
    }

    let mut acl_info = ACL_SIZE_INFORMATION::default();
    // SAFETY: `dacl` belongs to the live descriptor guard and `acl_info`
    // is the correctly sized output buffer.
    if unsafe {
        GetAclInformation(
            dacl,
            (&raw mut acl_info).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }

    let mut ace_ptr = std::ptr::null_mut();
    if acl_info.AceCount != 1
        || unsafe { GetAce(dacl, 0, &raw mut ace_ptr) } == 0
        || ace_ptr.is_null()
    {
        return Err(io_other(format!(
            "walpin sidecar dir {dir:?} grants access beyond its owner; refusing"
        )));
    }
    // SAFETY: `GetAce` returned the sole ACE in the live DACL, so its
    // common header is present. Reject every other shape before reading
    // the allowed-ACE fields.
    let header = unsafe { &*ace_ptr.cast::<ACE_HEADER>() };
    if header.AceType != 0
        || usize::from(header.AceSize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>()
    {
        return Err(io_other(format!(
            "walpin sidecar dir {dir:?} grants access beyond its owner; refusing"
        )));
    }
    // SAFETY: the header above establishes the allowed-ACE type and the
    // complete fixed prefix containing `Mask` and `SidStart`.
    let ace = unsafe { &*ace_ptr.cast::<ACCESS_ALLOWED_ACE>() };
    let ace_sid = (&raw const ace.SidStart).cast_mut().cast();
    let owner_matches = unsafe { EqualSid(owner, ace_sid) } != 0;
    let token_storage = current_token_user()?;
    // SAFETY: successful `GetTokenInformation(TokenUser)` initialized a
    // `TOKEN_USER` at the start of the aligned output buffer.
    let token_user_sid = unsafe { (*token_storage.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let owner_is_token_user = unsafe { EqualSid(owner, token_user_sid) } != 0;
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: `descriptor` remains live under `_descriptor`; both scalar
    // output buffers are valid for the call.
    if unsafe { GetSecurityDescriptorControl(descriptor, &raw mut control, &raw mut revision) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let restricted = windows_owner_dacl_is_restricted(
        acl_info.AceCount,
        ace.Header.AceType,
        ace.Header.AceFlags,
        ace.Mask,
        owner_matches,
        owner_is_token_user,
        control & SE_DACL_PROTECTED != 0,
    );
    if !restricted {
        return Err(io_other(format!(
            "walpin sidecar dir {dir:?} grants access beyond its owner; refusing"
        )));
    }
    Ok(())
}

fn open_dir_handle(dir: &Path) -> io::Result<fs::File> {
    #[cfg(test)]
    OPEN_DIR_HANDLE_CALLS.with(|calls| calls.set(calls.get() + 1));

    lexical_prefilter(dir)?;
    let expected = fs::canonicalize(dir)?;
    let expected_wide: Vec<u16> = expected.as_os_str().encode_wide().collect();
    lexical_prefilter(dir)?;

    let file = open_reparse_aware(
        dir,
        FILE_READ_ATTRIBUTES | READ_CONTROL,
        OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS,
    )?;
    verify_handle_kind(&file, true)?;
    let opened = final_path(&file)?;
    if !windows_final_path_matches(&expected_wide, &opened) {
        return Err(io_other(format!(
            "walpin sidecar path {dir:?} changed identity while it was opened; refusing"
        )));
    }
    validate_owner_only_dacl(&file, dir)?;
    Ok(file)
}

fn current_token_user() -> io::Result<Vec<usize>> {
    let mut token_handle: HANDLE = std::ptr::null_mut();
    // SAFETY: the pseudo-process handle is always valid and the output
    // handle is transferred to `OwnedHandle` immediately on success.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token_handle) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `token_handle` is newly returned and transferred once.
    let token = unsafe { OwnedHandle::from_raw_handle(token_handle as RawHandle) };

    let mut token_bytes = 0;
    // SAFETY: the first call intentionally supplies no output buffer and
    // asks Windows for its required size.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &raw mut token_bytes,
        )
    };
    if token_bytes == 0 {
        return Err(io::Error::last_os_error());
    }
    let token_words = (token_bytes as usize)
        .div_ceil(std::mem::size_of::<usize>())
        .max(1);
    let mut token_storage = vec![0usize; token_words];
    // SAFETY: the aligned storage has at least `token_bytes` writable
    // bytes and remains live while its SID is consumed below.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            token_storage.as_mut_ptr().cast(),
            token_bytes,
            &raw mut token_bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(token_storage)
}

fn create_owner_only_dir(dir: &Path) -> io::Result<()> {
    let token_storage = current_token_user()?;
    // SAFETY: successful `GetTokenInformation(TokenUser)` initialized a
    // `TOKEN_USER` at the start of the aligned output buffer.
    let owner_sid = unsafe { (*token_storage.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let sid_bytes = unsafe { GetLengthSid(owner_sid) } as usize;
    if sid_bytes == 0 {
        return Err(io::Error::last_os_error());
    }

    let acl_bytes = std::mem::size_of::<ACL>()
        .checked_add(std::mem::size_of::<ACCESS_ALLOWED_ACE>() - std::mem::size_of::<u32>())
        .and_then(|size| size.checked_add(sid_bytes))
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(|| io_other("owner-only walpin DACL size overflow"))?;
    let acl_words = (acl_bytes as usize)
        .div_ceil(std::mem::size_of::<usize>())
        .max(1);
    let mut acl_storage = vec![0usize; acl_words];
    let acl = acl_storage.as_mut_ptr().cast::<ACL>();
    // SAFETY: `acl` points to aligned writable storage of `acl_bytes`.
    if unsafe { InitializeAcl(acl, acl_bytes, ACL_REVISION) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the ACL is initialized and large enough for one full-control
    // ACE carrying the live token-user SID.
    if unsafe {
        AddAccessAllowedAceEx(
            acl,
            ACL_REVISION,
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
            FILE_ALL_ACCESS,
            owner_sid,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }

    let mut descriptor = SECURITY_DESCRIPTOR::default();
    // SAFETY: `descriptor` is writable and all referenced SID/ACL storage
    // remains live through `CreateDirectoryW` below.
    if unsafe { InitializeSecurityDescriptor((&raw mut descriptor).cast(), 1) } == 0
        || unsafe { SetSecurityDescriptorOwner((&raw mut descriptor).cast(), owner_sid, 0) } == 0
        || unsafe { SetSecurityDescriptorDacl((&raw mut descriptor).cast(), 1, acl, 0) } == 0
        || unsafe {
            SetSecurityDescriptorControl(
                (&raw mut descriptor).cast(),
                SE_DACL_PROTECTED,
                SE_DACL_PROTECTED,
            )
        } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: (&raw mut descriptor).cast(),
        bInheritHandle: 0,
    };
    let wide = to_wide_nul(dir)?;
    // SAFETY: `wide` is NUL-terminated and the security descriptor, ACL,
    // and owner SID remain live for the call.
    if unsafe { CreateDirectoryW(wide.as_ptr(), &raw const attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn open_or_create_dir_handle(dir: &Path) -> io::Result<fs::File> {
    match open_dir_handle(dir) {
        Ok(handle) => Ok(handle),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            ensure_ancestors_not_reparse(dir)?;
            if let Err(create_error) = create_owner_only_dir(dir) {
                if create_error.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
                    return Err(create_error);
                }
            }
            open_dir_handle(dir)
        }
        Err(error) => Err(error),
    }
}

pub(super) fn ensure_sidecar_dir(dir: &Path) -> io::Result<()> {
    open_or_create_dir_handle(dir).map(|_| ())
}

fn open_relative(
    dir: &fs::File,
    name: &str,
    desired_access: u32,
    create_disposition: u32,
) -> io::Result<fs::File> {
    if !windows_relative_child_name_is_safe(name) {
        return Err(io::Error::from(io::ErrorKind::InvalidFilename));
    }
    let mut wide: Vec<u16> = OsStr::new(name).encode_wide().collect();
    let byte_len = wide
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidFilename))?;
    let unicode_name = UNICODE_STRING {
        Length: byte_len,
        MaximumLength: byte_len,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: dir.as_raw_handle(),
        ObjectName: &raw const unicode_name,
        Attributes: windows_sys::Win32::Foundation::OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut io_status = IO_STATUS_BLOCK::default();
    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: every input structure and the name buffer are live; the
    // root directory handle is pinned, and a successful child handle is
    // transferred immediately below.
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
        // SAFETY: converting a returned failure status has no preconditions.
        let error = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    // SAFETY: `handle` is newly returned and transferred exactly once.
    Ok(unsafe { fs::File::from_raw_handle(handle as RawHandle) })
}

fn remove_relative_if_exists(dir: &fs::File, name: &str) -> io::Result<()> {
    let file = match open_relative(
        dir,
        name,
        DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        FILE_OPEN,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    verify_handle_kind(&file, false)?;
    delete_via_handle(&file)
}

fn inspect_relative_if_exists(dir: &fs::File, name: &str) -> io::Result<Option<fs::File>> {
    let file = match open_relative(dir, name, FILE_READ_ATTRIBUTES | SYNCHRONIZE, FILE_OPEN) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    verify_handle_kind(&file, false)?;
    Ok(Some(file))
}

fn delete_via_handle(file: &fs::File) -> io::Result<()> {
    let info = FileDispositionInfo { delete_pending: 1 };
    // SAFETY: `file`'s handle is live and was opened with `DELETE`
    // access; `info` is a valid, correctly sized input buffer for the
    // `FileDispositionInfo` class.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as Handle,
            FILE_DISPOSITION_INFO_CLASS,
            &info as *const FileDispositionInfo as *mut c_void,
            std::mem::size_of::<FileDispositionInfo>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Handle-bound rename to a full destination path with a null
/// `RootDirectory`. Both indirect forms are refused on this API
/// (measured on Windows Server 2022 CI, one round each): a non-null
/// `RootDirectory` fails with `ERROR_INVALID_PARAMETER` (87) on the
/// classic `FileRenameInfo` class AND on `FileRenameInfoEx`
/// (directory-relative `RootDirectory` renames exist only at the
/// `NtSetInformationFile` layer), and a bare relative name with a null
/// `RootDirectory` resolves against the process working directory, not
/// the file's parent — `ERROR_NOT_SAME_DEVICE` (17) when cwd and temp
/// sit on different drives. The caller therefore supplies the fully
/// qualified destination. With a null `RootDirectory`, Windows resolves
/// that destination by name at rename time, leaving a TOCTOU window
/// after directory validation: replacing a parent entry with a junction
/// or other reparse point can redirect the rename outside the directory
/// validated by `write_atomic`. The single-component target name only
/// constrains the leaf and does not close that window.
fn rename_via_handle(file: &fs::File, target_path: &Path) -> io::Result<()> {
    let wide = to_wide_nul(target_path)?;
    let name_bytes = (wide.len() - 1) * 2;
    // The real field offset, not an approximation — `FileRenameInfoEx`
    // validates the reported buffer size against this exact offset plus
    // `file_name_length`. Keep the trailing NUL in the buffer but out of
    // that length, matching the Win32 `FILE_RENAME_INFO` contract.
    let header_size = std::mem::offset_of!(FileRenameInfo, file_name);
    let total_size = header_size + name_bytes + std::mem::size_of::<u16>();
    let words = total_size.div_ceil(8).max(1);
    let mut buf: Vec<u64> = vec![0u64; words];
    // SAFETY: `buf` is 8-byte aligned (backed by `Vec<u64>`) and sized
    // to hold the header, `name_bytes` of `file_name`, and its trailing
    // NUL; the pointer arithmetic below stays within that allocation.
    unsafe {
        let header = buf.as_mut_ptr() as *mut FileRenameInfo;
        (*header).flags = FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
        (*header).root_directory = std::ptr::null_mut();
        (*header).file_name_length = name_bytes as u32;
        let name_ptr = (*header).file_name.as_mut_ptr();
        std::ptr::copy_nonoverlapping(wide.as_ptr(), name_ptr, wide.len());
    }
    let byte_ptr = buf.as_mut_ptr() as *mut c_void;
    // SAFETY: `file`'s handle is live and was opened with `DELETE`
    // access (required by the `FileRenameInfoEx` class); `byte_ptr`
    // addresses the well-formed buffer built above, sized exactly
    // `total_size`.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as Handle,
            FILE_RENAME_INFO_CLASS,
            byte_ptr,
            total_size as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn write_atomic(
    dir: &Path,
    target_name: &str,
    tmp_name: &str,
    body: &[u8],
) -> io::Result<()> {
    let dir_handle = open_or_create_dir_handle(dir)?;
    remove_relative_if_exists(&dir_handle, tmp_name)?;
    let mut tmp_file = open_relative(
        &dir_handle,
        tmp_name,
        GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        FILE_CREATE,
    )?;
    verify_handle_kind(&tmp_file, false)?;
    tmp_file.write_all(body)?;
    tmp_file.sync_all()?;
    let target_handle = inspect_relative_if_exists(&dir_handle, target_name)?;
    #[cfg(test)]
    if let Some(hook) = take_before_target_rename_hook() {
        hook();
    }
    // The inspected handle stays open through the replacing rename. Its
    // FILE_SHARE_DELETE permission still lets another writer move the
    // inspected leaf and install a different one before this path-based
    // rename; the handle does not bind the destination name to its identity.
    let result = rename_via_handle(&tmp_file, &dir.join(target_name));
    drop(target_handle);
    result
}

pub(super) fn remove_checked(dir: &Path, name: &str) -> io::Result<()> {
    let dir_handle = match open_dir_handle(dir) {
        Ok(handle) => handle,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    remove_relative_if_exists(&dir_handle, name)
}

pub(super) fn touch_mtime(dir: &Path, name: &str) -> io::Result<()> {
    let dir_handle = open_dir_handle(dir)?;
    let file = open_relative(
        &dir_handle,
        name,
        FILE_WRITE_ATTRIBUTES | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        FILE_OPEN,
    )
    .map_err(|error| {
        io_other(format!(
            "walpin sidecar entry {name:?} does not exist or could not be opened: {error}"
        ))
    })?;
    verify_handle_kind(&file, false)?;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| io_other(e.to_string()))?;
    const EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000;
    let ticks = now.as_secs() * 10_000_000 + u64::from(now.subsec_nanos()) / 100 + EPOCH_DIFF_100NS;
    let last_write = FileTime {
        dw_low_date_time: (ticks & 0xFFFF_FFFF) as u32,
        dw_high_date_time: (ticks >> 32) as u32,
    };
    // SAFETY: `file`'s handle is live; null creation/access-time
    // pointers leave those fields untouched (metadata-only mtime
    // refresh, mirroring the Unix `UTIME_OMIT` behavior); `last_write`
    // is a valid `FILETIME`-shaped value for the call's duration.
    if unsafe {
        SetFileTime(
            file.as_raw_handle() as Handle,
            std::ptr::null(),
            std::ptr::null(),
            &last_write,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

type Handle = *mut c_void;

#[repr(C)]
struct FileTime {
    dw_low_date_time: u32,
    dw_high_date_time: u32,
}

/// Mirrors Win32's `FILE_RENAME_INFO`: a `DWORD Flags` (the
/// `FileRenameInfoEx` interpretation of the leading union member —
/// the `Ex` class is used for `FILE_RENAME_FLAG_REPLACE_IF_EXISTS` and
/// `FILE_RENAME_FLAG_POSIX_SEMANTICS`; `RootDirectory` stays null on
/// BOTH classes because `SetFileInformationByHandle` rejects a non-null
/// value with `ERROR_INVALID_PARAMETER` — see [`rename_via_handle`]),
/// then a `HANDLE` (natural alignment inserts padding before it, matched
/// here by `repr(C)`), a `DWORD` length, and a flexible `WCHAR` array
/// sized by `file_name_length` bytes — the trailing `[u16; 1]` is a
/// placeholder; real instances are built in a manually sized buffer in
/// [`rename_via_handle`].
#[repr(C)]
struct FileRenameInfo {
    flags: u32,
    root_directory: Handle,
    file_name_length: u32,
    file_name: [u16; 1],
}

/// Mirrors Win32's `FILE_DISPOSITION_INFO`: a single `BOOLEAN` marking
/// the handle's object for delete-on-close.
#[repr(C)]
struct FileDispositionInfo {
    delete_pending: u8,
}

const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const STILL_ACTIVE: u32 = 259;
const GENERIC_WRITE: u32 = 0x4000_0000;
const DELETE: u32 = 0x0001_0000;
// `FileRenameInfoEx` (22), not the classic `FileRenameInfo` (3) — see
// the `FileRenameInfo` struct doc comment above.
const FILE_RENAME_INFO_CLASS: i32 = 22;
const FILE_DISPOSITION_INFO_CLASS: i32 = 4;
const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 1;
const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 2;

fn invalid_handle_value() -> Handle {
    usize::MAX as Handle
}

// `kernel32` is implicitly linked on every Windows target (same as
// `std` itself relies on); no explicit `#[link(...)]` is needed, mirroring
// how `windows-sys`/`winapi` declare these `extern "system"` blocks.
extern "system" {
    fn OpenProcess(dw_desired_access: u32, b_inherit_handle: i32, dw_process_id: u32) -> Handle;
    fn CloseHandle(h_object: Handle) -> i32;
    fn GetExitCodeProcess(h_process: Handle, lp_exit_code: *mut u32) -> i32;
    fn GetProcessTimes(
        h_process: Handle,
        lp_creation_time: *mut FileTime,
        lp_exit_time: *mut FileTime,
        lp_kernel_time: *mut FileTime,
        lp_user_time: *mut FileTime,
    ) -> i32;
    fn CreateFileW(
        lp_file_name: *const u16,
        dw_desired_access: u32,
        dw_share_mode: u32,
        lp_security_attributes: *mut c_void,
        dw_creation_disposition: u32,
        dw_flags_and_attributes: u32,
        h_template_file: Handle,
    ) -> Handle;
    fn SetFileTime(
        h_file: Handle,
        lp_creation_time: *const FileTime,
        lp_last_access_time: *const FileTime,
        lp_last_write_time: *const FileTime,
    ) -> i32;
    fn GetFinalPathNameByHandleW(
        h_file: Handle,
        lp_sz_file_path: *mut u16,
        cch_file_path: u32,
        dw_flags: u32,
    ) -> u32;
    fn SetFileInformationByHandle(
        h_file: Handle,
        file_information_class: i32,
        lp_file_information: *mut c_void,
        dw_buffer_size: u32,
    ) -> i32;
}

pub(super) fn is_process_alive(pid: u32) -> bool {
    // SAFETY: `OpenProcess` is a pure query; the handle (if non-null) is
    // closed before returning.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut exit_code: u32 = 0;
    // SAFETY: `handle` is a valid, just-opened process handle; `exit_code`
    // is a valid output buffer.
    let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    // SAFETY: `handle` was opened above and is closed exactly once here.
    unsafe { CloseHandle(handle) };
    ok != 0 && exit_code == STILL_ACTIVE
}

pub(super) fn process_start_time_secs(pid: u32) -> Option<i64> {
    // SAFETY: pure query; the handle is closed before returning.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut creation = FileTime {
        dw_low_date_time: 0,
        dw_high_date_time: 0,
    };
    let mut exit = FileTime {
        dw_low_date_time: 0,
        dw_high_date_time: 0,
    };
    let mut kernel = FileTime {
        dw_low_date_time: 0,
        dw_high_date_time: 0,
    };
    let mut user = FileTime {
        dw_low_date_time: 0,
        dw_high_date_time: 0,
    };
    // SAFETY: `handle` is valid; all four output buffers are valid
    // `FILETIME`-shaped structs for the call's duration.
    let ok = unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    // SAFETY: `handle` was opened above and closed exactly once here.
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return None;
    }
    // FILETIME: 100ns intervals since 1601-01-01 UTC. Convert to a Unix
    // epoch (1970-01-01) second count via the well-known offset between
    // the two epochs.
    let ticks = ((creation.dw_high_date_time as u64) << 32) | creation.dw_low_date_time as u64;
    const EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000;
    let unix_100ns = ticks.checked_sub(EPOCH_DIFF_100NS)?;
    Some((unix_100ns / 10_000_000) as i64)
}
