//! ADR-091 Amendment 2 Plank B: cross-process WAL-pin attribution sidecar.
//!
//! Every `kkernel mcp` process (daemon or session, any supported platform)
//! that observes its own `tx_registry` oldest span exceed `KHIVE_TX_WARN_SECS`
//! writes a per-PID heartbeat file under `<db-file>.walpin/<pid>.json`. On a
//! TRUNCATE no-progress event, the daemon enumerates this directory and
//! applies a three-test liveness gate (PID alive, `started_at` matches the
//! OS-reported process start time, `updated_at` fresh) to attribute the WAL
//! pin to a specific process rather than only naming its own in-process
//! registry.
//!
//! Filesystem trust boundary (binding): the sidecar
//! directory is created mode 0700 and validated as owned by the current user
//! before any use — a non-compliant existing directory is refused, never
//! chmod/chown'd into compliance. Heartbeat writes go through exclusive
//! create with `O_NOFOLLOW` semantics to a temp file, then atomic rename over
//! the target. Enumeration refuses symlinks and validates per-entry ownership
//! before reading or deleting anything.
//!
//! **Platform split.** Only the write path
//! (`ensure_sidecar_dir`/`write_heartbeat`/`write_beacon`/`remove_heartbeat`/
//! `touch_beacon`) and the identity primitives (`is_process_alive`/
//! `process_start_time_secs`) need to run on every platform — a Windows
//! session still needs to report itself into the sidecar. Directory
//! enumeration (`enumerate_live`/`housekeep_live`, and the OS-derived holder
//! census they anchor to) is Unix-only: its sole caller is the daemon's checkpoint task,
//! and daemon mode itself requires Unix (`khive-mcp/src/serve.rs` refuses
//! `--daemon` on non-Unix). The Unix write path is additionally
//! **handle-bound at every path component**: reaching the sidecar directory
//! walks each component of its parent path with
//! `openat(.., O_DIRECTORY | O_NOFOLLOW)` relative to the previous
//! descriptor (never a single `open()` on the parent's full path, which
//! only refuses a symlink at the parent's own final component and silently
//! follows every component before it), the directory itself is then
//! validated on the resulting file descriptor, and every
//! create/rename/unlink/enumeration read is performed `*at()`-relative to
//! it — no path is ever re-resolved per operation. The final path
//! component (the sidecar directory's own name, derived from the database
//! file name) is converted to its `openat` argument byte-exact, never via a
//! lossy UTF-8 conversion, since this project supports non-UTF-8 database
//! paths and a lossy conversion could collide two distinct database names
//! onto one sidecar directory. Windows uses a backup-semantics, no-follow
//! directory handle whose `FileAttributeTagInfo` and final resolved path are
//! verified before use. Every child open/create is then rooted at that handle
//! through `NtCreateFile`; rename and deletion remain handle-relative. New
//! directories receive a protected DACL containing one inheritable full-
//! control ACE for their owner, and existing directories with broader ACLs
//! are refused rather than repaired.

#[cfg(any(unix, test))]
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;
#[cfg(any(unix, test))]
use std::time::{SystemTime, UNIX_EPOCH};

/// Allowed drift between a heartbeat's recorded `started_at` and the
/// OS-reported process start time queried fresh at enumeration — both are
/// whole-second values sourced from different clocks (the writer's own
/// `SystemTime::now()` vs. `proc_pidinfo`/`/proc/<pid>/stat`), so this is
/// rounding slack, not a real identity ambiguity window.
#[cfg(unix)]
const START_TIME_EPSILON_SECS: u64 = 2;

mod types;

use types::io_other;
#[cfg(unix)]
use types::{producer_temp_identity, ProducerTempKind};
pub use types::{
    sidecar_dir_for, sidecar_enabled, LiveWalpinEntry, WalpinBeacon, WalpinHeartbeat,
    WalpinPidHealth, WalpinReport,
};
#[cfg(any(windows, test))]
use types::{
    windows_attribute_tag_is_acceptable, windows_final_path_matches,
    windows_owner_dacl_is_restricted, windows_relative_child_name_is_safe,
};

/// Unix sidecar internals (ADR-091 Amendment 2: handle-bound
/// filesystem operations). The sidecar directory is opened exactly once per
/// call with `O_DIRECTORY | O_NOFOLLOW`, validated (type/mode/owner) on that
/// descriptor, and every create/rename/unlink/read is `*at()`-relative to it
/// — the path is never re-resolved between validation and use.
#[cfg(unix)]
mod unix_impl;

/// Windows sidecar internals. The directory is opened without following its
/// final component, checked through handle metadata and final-path identity,
/// and retained as the root for all child operations. New directories receive
/// a protected owner-only DACL before they become visible.
#[cfg(windows)]
mod windows_impl {
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
        AclSizeInformation, AddAccessAllowedAceEx, EqualSid, GetAce, GetAclInformation,
        GetLengthSid, GetSecurityDescriptorControl, GetTokenInformation, InitializeAcl,
        InitializeSecurityDescriptor, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
        SetSecurityDescriptorOwner, TokenUser, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION,
        ACL_SIZE_INFORMATION, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
        TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateDirectoryW, FileAttributeTagInfo, GetFileInformationByHandleEx, FILE_ALL_ACCESS,
        FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, OPEN_EXISTING,
        READ_CONTROL, SYNCHRONIZE, VOLUME_NAME_DOS,
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
        if !windows_attribute_tag_is_acceptable(
            info.FileAttributes,
            info.ReparseTag,
            require_directory,
        ) {
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
        if unsafe { GetSecurityDescriptorControl(descriptor, &raw mut control, &raw mut revision) }
            == 0
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
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token_handle) } == 0
        {
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
            || unsafe { SetSecurityDescriptorOwner((&raw mut descriptor).cast(), owner_sid, 0) }
                == 0
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
        let ticks =
            now.as_secs() * 10_000_000 + u64::from(now.subsec_nanos()) / 100 + EPOCH_DIFF_100NS;
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
        fn OpenProcess(dw_desired_access: u32, b_inherit_handle: i32, dw_process_id: u32)
            -> Handle;
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
        let ok =
            unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
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
}

/// Outcome of one OS-derived holder census pass (ADR-091 Amendment 2,
/// item a). A PID the census positively determined does NOT hold the
/// database file is simply absent from `holders` — that is a normal,
/// complete result. A PID whose inspection FAILED (permission denied, or a
/// races-away process) instead of succeeding-with-a-negative-answer is
/// recorded in `uninspectable_pids`: the census as a whole is then
/// INCOMPLETE, and callers must treat that exactly like an `unknown` sidecar
/// PID — inconclusive, never silently folded into "no unregistered holder."
///
/// `truncated` is a second, independent incompleteness signal: set when the enumeration walk itself has positive evidence it did
/// not see the full live-process universe even though no single PID's
/// inspection outright failed — a `/proc` directory-iterator error or a
/// PID-namespace mismatch on Linux, or a libproc buffer whose returned byte
/// count still equalled its negotiated capacity after bounded retries on
/// macOS. `is_complete()` folds both signals together.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CensusResult {
    pub holders: std::collections::HashSet<u32>,
    pub uninspectable_pids: Vec<u32>,
    pub truncated: bool,
    /// Set when the walk stopped because its wall-clock budget was spent.
    /// This also sets `truncated`, so `is_complete()` needs no knowledge of
    /// it; the two are kept apart because they have opposite operational
    /// meanings. A budget stop is a configurable trade the caller asked for
    /// and says nothing about the health of the machine. Every other
    /// truncation is positive evidence that process enumeration itself
    /// misbehaved. A report that folded them together would describe a
    /// healthy bounded census in the same words as a broken one, on every
    /// call, on every busy box — which is how a real signal gets ignored.
    pub budget_exhausted: bool,
}

#[cfg(target_os = "linux")]
fn census_visible_self_pid() -> Option<u32> {
    fs::read_link("/proc/self")
        .ok()?
        .file_name()?
        .to_str()?
        .parse()
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn census_visible_self_pid() -> Option<u32> {
    Some(std::process::id())
}

impl CensusResult {
    /// Every discovered PID was either confirmed as a holder or positively
    /// ruled out (no PID's inspection failed outright), AND the walk itself
    /// carries no positive evidence that it missed part of the live-process
    /// universe.
    pub fn is_complete(&self) -> bool {
        self.uninspectable_pids.is_empty() && !self.truncated
    }

    /// ADR-091 Amendment 2 self-canary: the checkpoint census caller always
    /// runs inside the process whose
    /// own SQLite connection pool holds `db_path` open, so a correct,
    /// complete census must find the process identity visible through the
    /// scanner's process source. On Linux that is `/proc/self`, which can
    /// differ from `std::process::id()` when procfs and the caller occupy
    /// different PID namespaces. Not finding the scanner-visible identity
    /// is positive proof the walk missed at least one live holder. This is
    /// necessary but not sufficient, so every platform applies this on top
    /// of its own per-step incompleteness markers.
    #[cfg(unix)]
    fn apply_self_canary(&mut self) {
        self.apply_self_canary_for(census_visible_self_pid());
    }

    fn apply_self_canary_for(&mut self, expected_self: Option<u32>) {
        if expected_self.is_none_or(|pid| !self.holders.contains(&pid)) {
            self.truncated = true;
        }
    }
}

/// macOS: classify a `proc_pidinfo`/`proc_pidfdinfo` failure by errno.
/// `ESRCH` means the target process exited between `proc_listpids` and this
/// call — a genuine "positively gone" race, safe to skip. Any other errno
/// (most commonly `EPERM`/`EACCES`, inspecting another user's open files)
/// means the inspection itself failed: the census cannot say whether this
/// PID holds the database file, so it must be reported as uninspectable
/// rather than silently excluded.
#[cfg(target_os = "macos")]
fn macos_pid_genuinely_gone(errno: Option<i32>) -> bool {
    errno == Some(libc::ESRCH)
}

/// macOS: classify a `proc_pidfdinfo` return against the expected struct
/// size. Only an exact match is a successful inspection. A positive but
/// short byte count (ADR-091 Amendment 2) means the kernel wrote a
/// truncated/partial struct rather than the full `VnodeFdInfoWithPath` —
/// that is an inspection failure exactly like a non-positive return, not a
/// successful call that merely returned less data than expected.
#[cfg(target_os = "macos")]
fn proc_pidfdinfo_returned_expected_size(returned_bytes: i32, expected_size: usize) -> bool {
    returned_bytes > 0 && returned_bytes as usize == expected_size
}

/// Bounded attempts for the macOS buffer-size negotiation below — enough to
/// absorb the live-set growing between the sizing call and the data call a
/// couple of times without looping forever on a pathologically fast-growing
/// process/fd table.
#[cfg(target_os = "macos")]
const CENSUS_BUFFER_NEGOTIATION_ATTEMPTS: usize = 4;

/// Bounded buffer-size negotiation shared by `proc_listpids` and
/// `proc_pidinfo(PROC_PIDLISTFDS)` (ADR-091 Amendment 2,
/// item c — the fixed 8192-PID/4096-FD buffers used to truncate silently).
/// Both libproc calls return the needed byte count when handed a null
/// buffer (`size_call`); this allocates with headroom and re-invokes
/// (`data_call`). The set being listed (all live PIDs, or one PID's open
/// fds) can grow between the two calls, so this retries a bounded number of
/// times; if the returned byte count still equals the buffer's capacity on
/// the final attempt, the true set may be larger than what was captured —
/// the second return value is `true` and the caller must not trust the
/// result as complete.
#[cfg(target_os = "macos")]
fn negotiate_buffer<T: Default + Clone>(
    size_call: impl Fn() -> std::os::raw::c_int,
    data_call: impl Fn(*mut std::os::raw::c_void, std::os::raw::c_int) -> std::os::raw::c_int,
    should_stop: &impl Fn() -> bool,
) -> io::Result<(Vec<T>, bool)> {
    let item_size = std::mem::size_of::<T>();
    for attempt in 0..CENSUS_BUFFER_NEGOTIATION_ATTEMPTS {
        if should_stop() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "WAL holder census cancelled",
            ));
        }
        let needed = size_call();
        if needed <= 0 {
            return Err(io::Error::last_os_error());
        }
        let needed_items = needed as usize / item_size + 1;
        let item_count = needed_items + needed_items / 4 + 8;
        let mut buf: Vec<T> = vec![T::default(); item_count];
        let cap_bytes = (buf.len() * item_size) as std::os::raw::c_int;
        let bytes = data_call(buf.as_mut_ptr() as *mut std::os::raw::c_void, cap_bytes);
        if bytes <= 0 {
            return Err(io::Error::last_os_error());
        }
        let filled_capacity = bytes as usize >= cap_bytes as usize;
        let is_last_attempt = attempt + 1 == CENSUS_BUFFER_NEGOTIATION_ATTEMPTS;
        if filled_capacity && !is_last_attempt {
            // The live set grew to fill (or exceed) our snapshot — retry
            // with a freshly sized buffer rather than trust a possibly
            // partial one.
            continue;
        }
        let count = (bytes as usize / item_size).min(buf.len());
        buf.truncate(count);
        return Ok((buf, filled_capacity));
    }
    unreachable!("loop always returns or errors within CENSUS_BUFFER_NEGOTIATION_ATTEMPTS")
}

/// macOS OS-derived census (ADR-091 Amendment 2): every PID
/// on the system that currently holds `db_path` open, via `libproc`'s
/// `PROC_PIDLISTFDS`/`PROC_PIDFDVNODEPATHINFO` — never the sidecar directory
/// listing, which only sees PIDs that already wrote something there.
#[cfg(target_os = "macos")]
pub fn census_holders(db_path: &Path) -> io::Result<CensusResult> {
    census_holders_until(db_path, || false)
}

#[cfg(target_os = "macos")]
pub(crate) fn census_holders_until<C>(db_path: &Path, should_stop: C) -> io::Result<CensusResult>
where
    C: Fn() -> bool,
{
    census_holders_inner(db_path, should_stop, None)
}

/// Walk the holder census under a wall-clock budget, returning what was seen
/// so far rather than an error when the budget is spent.
///
/// This is deliberately NOT expressible through the crate-internal
/// `census_holders_until`. That function's stop closure is a CANCELLATION: every one of its check sites
/// returns `Err(Interrupted)`, which is right for a caller that no longer
/// wants the answer (a shutting-down background worker) and wrong for a caller
/// that wants a fast, honest one. An interactive diagnostic asking "is anything
/// stalled" must not be told "cancelled" because the machine had many processes
/// to walk.
///
/// A budget stop therefore sets [`CensusResult::truncated`] and
/// [`CensusResult::budget_exhausted`] and returns `Ok`, exactly as an
/// unreadable PID sets `uninspectable_pids` and continues. Both are
/// incompleteness, not failure, and [`CensusResult::is_complete`] already
/// folds them together — so a caller that does not branch on it reads a
/// bounded census as if it were whole, which is why the field is the
/// load-bearing part of this change rather than the bound.
#[cfg(target_os = "macos")]
pub fn census_holders_until_within<C>(
    db_path: &Path,
    should_stop: C,
    budget: Duration,
) -> io::Result<CensusResult>
where
    C: Fn() -> bool,
{
    census_holders_inner(db_path, should_stop, Some(Instant::now() + budget))
}

#[cfg(target_os = "macos")]
fn census_holders_inner<C>(
    db_path: &Path,
    should_stop: C,
    deadline: Option<Instant>,
) -> io::Result<CensusResult>
where
    C: Fn() -> bool,
{
    let budget_spent = || deadline.is_some_and(|d| Instant::now() >= d);
    use std::os::raw::{c_int, c_void};
    use std::os::unix::fs::MetadataExt;

    const PROC_ALL_PIDS: u32 = 1;
    const PROC_PIDLISTFDS: c_int = 1;
    const PROC_PIDFDVNODEPATHINFO: c_int = 2;
    const PROX_FDTYPE_VNODE: u32 = 1;
    const MAXPATHLEN: usize = 1024;

    if should_stop() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "WAL holder census cancelled",
        ));
    }

    #[repr(C)]
    #[derive(Clone, Default)]
    struct ProcFdInfo {
        proc_fd: i32,
        proc_fdtype: u32,
    }
    #[repr(C)]
    struct ProcFileInfo {
        fi_openflags: u32,
        fi_status: u32,
        fi_offset: i64,
        fi_type: i32,
        fi_guardflags: u32,
    }
    #[repr(C)]
    struct FsId {
        val: [i32; 2],
    }
    #[repr(C)]
    struct VinfoStat {
        vst_dev: u32,
        vst_mode: u16,
        vst_nlink: u16,
        vst_ino: u64,
        vst_uid: u32,
        vst_gid: u32,
        vst_atime: i64,
        vst_atimensec: i64,
        vst_mtime: i64,
        vst_mtimensec: i64,
        vst_ctime: i64,
        vst_ctimensec: i64,
        vst_birthtime: i64,
        vst_birthtimensec: i64,
        vst_size: i64,
        vst_blocks: i64,
        vst_blksize: i32,
        vst_flags: u32,
        vst_gen: u32,
        vst_rdev: u32,
        vst_qspare: [i64; 2],
    }
    #[repr(C)]
    struct VnodeInfo {
        vi_stat: VinfoStat,
        vi_type: i32,
        vi_pad: i32,
        vi_fsid: FsId,
    }
    #[repr(C)]
    struct VnodeInfoPath {
        vip_vi: VnodeInfo,
        vip_path: [u8; MAXPATHLEN],
    }
    #[repr(C)]
    struct VnodeFdInfoWithPath {
        pfi: ProcFileInfo,
        pvip: VnodeInfoPath,
    }

    #[link(name = "proc")]
    extern "C" {
        fn proc_listpids(kind: u32, typeinfo: u32, buffer: *mut c_void, buffersize: c_int)
            -> c_int;
        fn proc_pidinfo(
            pid: c_int,
            flavor: c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
        fn proc_pidfdinfo(
            pid: c_int,
            fd: c_int,
            flavor: c_int,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
    }

    // File-identity target, not a path target: holders are matched on
    // (device, inode) so a process that opened the database through a hard
    // link (or any alternate name for the same file) is still discovered.
    let target_meta = fs::metadata(db_path)?;
    let target_ident = (target_meta.dev() as u32, target_meta.ino());

    // SAFETY: `negotiate_buffer` hands `proc_listpids` a buffer sized from
    // its own reported byte count, growing on retry; the extern call writes
    // at most the byte capacity passed to it.
    let (pid_buf, pid_list_truncated): (Vec<i32>, bool) = negotiate_buffer(
        || unsafe { proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) },
        |buf_ptr, buf_bytes| unsafe { proc_listpids(PROC_ALL_PIDS, 0, buf_ptr, buf_bytes) },
        &should_stop,
    )?;

    let mut holders = std::collections::HashSet::new();
    let mut uninspectable: Vec<u32> = Vec::new();
    let mut budget_exhausted = false;
    'pids: for &pid in &pid_buf {
        if should_stop() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "WAL holder census cancelled",
            ));
        }
        if budget_spent() {
            budget_exhausted = true;
            break 'pids;
        }
        if pid <= 0 {
            continue;
        }
        // SAFETY: `negotiate_buffer` hands `proc_pidinfo` a buffer sized
        // from its own reported byte count, growing on retry.
        let (fd_buf, fd_list_truncated): (Vec<ProcFdInfo>, bool) = match negotiate_buffer(
            || unsafe { proc_pidinfo(pid, PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) },
            |buf_ptr, buf_bytes| unsafe {
                proc_pidinfo(pid, PROC_PIDLISTFDS, 0, buf_ptr, buf_bytes)
            },
            &should_stop,
        ) {
            Ok(v) => v,
            Err(e) => {
                if e.kind() == io::ErrorKind::Interrupted {
                    return Err(e);
                }
                // A failed sizing/listing call means either the PID exited
                // between `proc_listpids` and here (ESRCH — positively
                // gone, safe to skip) or the inspection itself failed (most
                // commonly permission denied to list another user's fds).
                // Only the former is excluded cleanly; the latter means we
                // could not determine whether this PID holds the db, so it
                // marks the whole census incomplete rather than being
                // silently treated as "not a holder."
                if !macos_pid_genuinely_gone(e.raw_os_error()) {
                    uninspectable.push(pid as u32);
                }
                continue;
            }
        };
        if fd_list_truncated {
            // This PID's fd table may be larger than what fit even after
            // bounded retries — its inspection cannot be trusted complete.
            uninspectable.push(pid as u32);
        }
        for fdinfo in &fd_buf {
            if should_stop() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "WAL holder census cancelled",
                ));
            }
            if budget_spent() {
                budget_exhausted = true;
                break 'pids;
            }
            if fdinfo.proc_fdtype != PROX_FDTYPE_VNODE {
                continue;
            }
            let mut vinfo: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
            // SAFETY: `vinfo` is a valid, zeroed, appropriately-sized buffer.
            let vsize = unsafe {
                proc_pidfdinfo(
                    pid,
                    fdinfo.proc_fd,
                    PROC_PIDFDVNODEPATHINFO,
                    &mut vinfo as *mut _ as *mut c_void,
                    std::mem::size_of::<VnodeFdInfoWithPath>() as c_int,
                )
            };
            if !proc_pidfdinfo_returned_expected_size(
                vsize,
                std::mem::size_of::<VnodeFdInfoWithPath>(),
            ) {
                // A non-positive return is a failed inspection call for
                // this fd: ESRCH-equivalent (the fd/process raced away) is
                // genuinely gone, safe to skip; any other errno means we
                // could not determine whether THIS fd is our target, so the
                // PID's census is incomplete rather than a clean negative.
                if vsize <= 0 {
                    let errno = io::Error::last_os_error().raw_os_error();
                    if !macos_pid_genuinely_gone(errno) {
                        uninspectable.push(pid as u32);
                    }
                } else {
                    // Positive but short: the call itself succeeded (no
                    // errno to classify), it just wrote less data than the
                    // struct requires — an inspection failure regardless.
                    uninspectable.push(pid as u32);
                }
                continue;
            }
            // Identity comparison on the kernel-reported (device, inode)
            // rather than the vnode's path string: a holder that opened the
            // database through a hard link (or any alternate name for the
            // same file) reports a different path, and a path comparison
            // would silently omit it without marking the census incomplete.
            let vstat = &vinfo.pvip.vip_vi.vi_stat;
            if (vstat.vst_dev, vstat.vst_ino) == target_ident {
                holders.insert(pid as u32);
                break;
            }
        }
    }
    uninspectable.sort_unstable();
    uninspectable.dedup();
    let mut census = CensusResult {
        holders,
        uninspectable_pids: uninspectable,
        truncated: pid_list_truncated || budget_exhausted,
        budget_exhausted,
    };
    census.apply_self_canary();
    Ok(census)
}

/// Linux: classify a `/proc/<pid>/fd` open failure. `NotFound` means the
/// process exited between the `/proc` directory listing and this call — a
/// genuine "positively gone" race, safe to skip. Any other error (most
/// commonly `PermissionDenied`, inspecting another user's fds) means the
/// inspection itself failed and the PID must be reported as uninspectable.
#[cfg(target_os = "linux")]
fn linux_proc_gone(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotFound
}

/// The fixed inode number the kernel assigns to the *init* PID namespace —
/// the one namespace that exists for the lifetime of the machine, created
/// at boot before any container/unshare call can create another (Linux
/// `include/linux/proc_ns.h`, `PROC_PID_INIT_INO`). Every non-init PID
/// namespace — including a container's own, self-consistent one — gets a
/// dynamically allocated inode instead, so this exact value is a positive,
/// unspoofable proof that `/proc/self/ns/pid` refers to the host's own
/// root namespace (ADR-091 Amendment 2: a same-namespace readlink
/// comparison against `/proc/1/ns/pid` cannot tell "the host" apart from
/// "a container that is its own root," because both are internally
/// self-consistent).
#[cfg(target_os = "linux")]
const PROC_PID_INIT_INO: u64 = 0xEFFFFFFC;

/// Linux: classify a `/proc/self/ns/pid` inode against
/// [`PROC_PID_INIT_INO`]. Only an exact match is positive proof this
/// process shares the host's own (init) PID namespace; anything else means
/// external holders outside this namespace may be invisible to the
/// `/proc` walk below, so the census must be marked incomplete rather than
/// trusted as global.
#[cfg(target_os = "linux")]
fn pid_ns_is_init(ino: u64) -> bool {
    ino == PROC_PID_INIT_INO
}

/// Linux: classify a single procfs mount-options string (either the
/// per-mount options field or the super-options field of a
/// `/proc/self/mountinfo` line) as restricting per-PID visibility.
///
/// The init-PID-namespace inode check (`pid_ns_is_init`) only rules out
/// one way the `/proc` walk can miss processes. A host `/proc` mounted with
/// `hidepid=1`/`hidepid=2` (or the symbolic `hidepid=noaccess` /
/// `hidepid=invisible` / `hidepid=ptraceable` forms) or `subset=pid` hides
/// other users' `/proc/<pid>` directories from `readdir` entirely — no
/// per-PID error surfaces, `/proc/self` stays visible, and the self-canary
/// still passes, so that path alone cannot detect the restriction. Only
/// `hidepid=0` (or the symbolic `hidepid=off`) and the absence of `subset`
/// are compatible with treating the walk as global.
#[cfg(target_os = "linux")]
fn proc_mount_restricts_visibility(options: &str) -> bool {
    options.split(',').map(str::trim).any(|opt| {
        if let Some(value) = opt.strip_prefix("hidepid=") {
            !matches!(value, "0" | "off")
        } else {
            opt == "hidepid" || opt == "subset" || opt.starts_with("subset=")
        }
    })
}

/// Linux: locate every procfs mount backing `/proc` in
/// `/proc/self/mountinfo` and classify whether any of them restricts
/// per-PID visibility. Mounts stack: a later `/proc` mount shadows an
/// earlier one while both records remain in mountinfo, and picking a single
/// record would let a clean shadowed mount mask a restricted visible one.
/// Selection is therefore ANY-restrictive across every matching record —
/// ordering-independent and fail-closed against stacking. Returns `None`
/// when the mountinfo file can't be read or no `/proc` entry with
/// `fstype proc` is found — the caller treats `None` the same as
/// "restricted": an unparsable mountinfo carries no positive proof the
/// walk saw every host PID either, so it fails closed rather than assuming
/// a clean mount.
#[cfg(target_os = "linux")]
fn proc_mount_is_visibility_restricted() -> Option<bool> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").ok()?;
    proc_mounts_restricted_in(&mountinfo)
}

/// Pure classification over mountinfo content, split out so the
/// any-restrictive selection is testable without a live `/proc`.
#[cfg(target_os = "linux")]
fn proc_mounts_restricted_in(mountinfo: &str) -> Option<bool> {
    let mut found_any = false;
    for line in mountinfo.lines() {
        // mountinfo line shape:
        //   <id> <parent-id> <major:minor> <root> <mount-point>
        //   <mount-options> <optional-fields...> - <fs-type> <mount-source>
        //   <super-options>
        let Some((fields_part, super_part)) = line.split_once(" - ") else {
            continue;
        };
        let fields: Vec<&str> = fields_part.split(' ').collect();
        if fields.len() < 6 || fields[4] != "/proc" {
            continue;
        }
        let mount_options = fields[5];
        let super_fields: Vec<&str> = super_part.split(' ').collect();
        if super_fields.first().copied() != Some("proc") {
            continue;
        }
        let super_options = super_fields.get(2).copied().unwrap_or("");
        found_any = true;
        if proc_mount_restricts_visibility(mount_options)
            || proc_mount_restricts_visibility(super_options)
        {
            return Some(true);
        }
    }
    if found_any {
        Some(false)
    } else {
        None
    }
}

/// Linux OS-derived census (ADR-091 Amendment 2): scan
/// `/proc/<pid>/fd/*` for every live PID and stat each fd through its proc
/// magic link, comparing `(device, inode)` identity against `db_path`'s. A
/// PID whose `fd` directory
/// cannot be opened at all (most commonly permission denied) is reported as
/// uninspectable rather than silently excluded — only a PID confirmed gone
/// (`NotFound`, a listing/inspection race) is skipped cleanly.
///
/// Before trusting the walk as a GLOBAL census (ADR-091 Amendment 2):
/// `hidepid` mounts, restricted `/proc`, and non-init PID namespaces can all
/// make `read_dir("/proc")` succeed while silently showing only a subset of
/// the host's live PIDs — with no per-entry error to catch. Three checks
/// widen the net rather than trust a clean-looking iteration outright: (1)
/// a positive proof that this process itself is running in the *host's*
/// init PID namespace — see `pid_ns_is_init`; a container's own procfs is
/// internally self-consistent (its `/proc/1` resolves to its own init), so
/// merely comparing `/proc/1/ns/pid` against `/proc/self/ns/pid` cannot
/// distinguish "the host" from "a container that is its own root," and was
/// replaced with this inode check (ADR-091 Amendment 2). (2) a positive
/// proof the procfs mount backing `/proc` carries no `hidepid`/`subset`
/// restriction — see `proc_mount_is_visibility_restricted`; a
/// `hidepid`-restricted mount hides other users' `/proc/<pid>` directories
/// from `readdir` with no per-entry error, so the init-namespace check
/// alone (self stays visible, self-canary passes) cannot detect it. (3) any error surfacing from the `/proc` or per-PID `fd`
/// directory ITERATORS themselves (not a single entry's own error) marks
/// the walk incomplete rather than being dropped via `.flatten()`.
#[cfg(target_os = "linux")]
pub fn census_holders(db_path: &Path) -> io::Result<CensusResult> {
    census_holders_until(db_path, || false)
}

#[cfg(target_os = "linux")]
pub(crate) fn census_holders_until<C>(db_path: &Path, should_stop: C) -> io::Result<CensusResult>
where
    C: Fn() -> bool,
{
    census_holders_inner(db_path, should_stop, None)
}

/// Walk the holder census under a wall-clock budget, returning what was seen
/// so far rather than an error when the budget is spent.
///
/// This is deliberately NOT expressible through the crate-internal
/// `census_holders_until`. That function's stop closure is a CANCELLATION: every one of its check sites
/// returns `Err(Interrupted)`, which is right for a caller that no longer
/// wants the answer (a shutting-down background worker) and wrong for a caller
/// that wants a fast, honest one. An interactive diagnostic asking "is anything
/// stalled" must not be told "cancelled" because the machine had many processes
/// to walk.
///
/// A budget stop therefore sets [`CensusResult::truncated`] and
/// [`CensusResult::budget_exhausted`] and returns `Ok`, exactly as an
/// unreadable PID sets `uninspectable_pids` and continues. Both are
/// incompleteness, not failure, and [`CensusResult::is_complete`] already
/// folds them together — so a caller that does not branch on it reads a
/// bounded census as if it were whole, which is why the field is the
/// load-bearing part of this change rather than the bound.
#[cfg(target_os = "linux")]
pub fn census_holders_until_within<C>(
    db_path: &Path,
    should_stop: C,
    budget: Duration,
) -> io::Result<CensusResult>
where
    C: Fn() -> bool,
{
    census_holders_inner(db_path, should_stop, Some(Instant::now() + budget))
}

#[cfg(target_os = "linux")]
fn census_holders_inner<C>(
    db_path: &Path,
    should_stop: C,
    deadline: Option<Instant>,
) -> io::Result<CensusResult>
where
    C: Fn() -> bool,
{
    let budget_spent = || deadline.is_some_and(|d| Instant::now() >= d);
    use std::os::unix::fs::MetadataExt;

    if should_stop() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "WAL holder census cancelled",
        ));
    }

    // File-identity target, not a path target: holders are matched on
    // (device, inode) so a process that opened the database through a hard
    // link or a bind-mounted alternate path is still discovered.
    let target_meta = fs::metadata(db_path)?;
    let target_ident = (target_meta.dev(), target_meta.ino());
    let mut holders = std::collections::HashSet::new();
    let mut uninspectable: Vec<u32> = Vec::new();
    let mut truncated = false;

    match fs::metadata("/proc/self/ns/pid") {
        Ok(meta) if pid_ns_is_init(meta.ino()) => {}
        _ => truncated = true,
    }

    match proc_mount_is_visibility_restricted() {
        Some(false) => {}
        Some(true) | None => truncated = true,
    }

    let mut budget_exhausted = false;
    let proc_dir = fs::read_dir("/proc")?;
    'pids: for entry_result in proc_dir {
        if should_stop() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "WAL holder census cancelled",
            ));
        }
        if budget_spent() {
            truncated = true;
            budget_exhausted = true;
            break 'pids;
        }
        let proc_entry = match entry_result {
            Ok(e) => e,
            Err(_) => {
                // The directory iterator itself failed mid-walk (not one
                // entry's own error) — the walk is no longer provably a
                // complete enumeration of live PIDs.
                truncated = true;
                continue;
            }
        };
        let Some(pid) = proc_entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let fd_dir = proc_entry.path().join("fd");
        let fds = match fs::read_dir(&fd_dir) {
            Ok(fds) => fds,
            Err(e) if linux_proc_gone(&e) => continue,
            Err(_) => {
                uninspectable.push(pid);
                continue;
            }
        };
        for fd_result in fds {
            if should_stop() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "WAL holder census cancelled",
                ));
            }
            if budget_spent() {
                truncated = true;
                budget_exhausted = true;
                break 'pids;
            }
            let fd_entry = match fd_result {
                Ok(e) => e,
                Err(_) => {
                    // The fd-directory iterator failed on this PID mid-walk
                    // — its set of open fds cannot be trusted complete, so
                    // this PID's census is incomplete rather than "no
                    // match found."
                    uninspectable.push(pid);
                    continue;
                }
            };
            // Identity comparison via a stat *through* the proc fd magic
            // link — it resolves to the open file itself, so the match is
            // on (device, inode) rather than a readlink'd path string. A
            // holder that opened the database through a hard link or a
            // bind-mounted alternate path reports a different path, and a
            // path comparison would silently omit it without marking the
            // census incomplete. Non-file fd targets (sockets, pipes,
            // anon inodes) stat fine and simply never match the target.
            match fs::metadata(fd_entry.path()) {
                Ok(meta) => {
                    if (meta.dev(), meta.ino()) == target_ident {
                        holders.insert(pid);
                        break;
                    }
                }
                // The fd itself closed between listing and this stat — a
                // genuine "positively gone" race, safe to skip.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(_) => uninspectable.push(pid),
            }
        }
    }
    uninspectable.sort_unstable();
    uninspectable.dedup();
    let mut census = CensusResult {
        holders,
        uninspectable_pids: uninspectable,
        truncated,
        budget_exhausted,
    };
    census.apply_self_canary();
    Ok(census)
}

/// Any other Unix (khive ships macOS/Linux/Windows only; this is a
/// documented-gap fallback for a hypothetical build on anything else, not a
/// real deployment target) has no holder-enumeration implementation here.
/// An error (never a silently-empty `Ok`) so the caller treats it as a
/// census failure — the same "cannot rule out an unregistered holder"
/// posture as a real enumeration error, not false reassurance.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
pub fn census_holders(_db_path: &Path) -> io::Result<CensusResult> {
    Err(io_other(
        "OS-derived holder census has no implementation on this Unix target",
    ))
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
pub(crate) fn census_holders_until<C>(_db_path: &Path, should_stop: C) -> io::Result<CensusResult>
where
    C: Fn() -> bool,
{
    if should_stop() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "WAL holder census cancelled",
        ));
    }
    Err(io_other(
        "OS-derived holder census has no implementation on this Unix target",
    ))
}

/// A budget cannot make an absent implementation partial: there is no walk to
/// stop early, so this stays the same census failure the unbounded form is.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
pub fn census_holders_until_within<C>(
    db_path: &Path,
    should_stop: C,
    _budget: Duration,
) -> io::Result<CensusResult>
where
    C: Fn() -> bool,
{
    census_holders_until(db_path, should_stop)
}

/// Ensure `dir` exists and is trustworthy: a real directory (never a
/// symlink or reparse-point component), and accessible only to its owner:
/// Unix mode `0700`, or a protected owner-only DACL on Windows. Refuses —
/// never chmod/chown/re-ACLs — a non-compliant existing directory.
pub fn ensure_sidecar_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        unix_impl::SidecarDirHandle::open_or_create(dir)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        windows_impl::ensure_sidecar_dir(dir)
    }
}

/// Write (or refresh) this process's heartbeat file. Exclusive-create a temp
/// file (`O_NOFOLLOW` on Unix), then atomically rename it over the target —
/// never an in-place open of a possibly attacker-placed path.
pub fn write_heartbeat(dir: &Path, heartbeat: &WalpinHeartbeat) -> io::Result<()> {
    let body =
        serde_json::to_vec(heartbeat).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let target = format!("{}.json", heartbeat.pid);
    let tmp = format!(".{}.json.tmp", heartbeat.pid);
    #[cfg(unix)]
    {
        let handle = unix_impl::SidecarDirHandle::open_or_create(dir)?;
        handle.write_atomic(&target, &tmp, &body)
    }
    #[cfg(windows)]
    {
        windows_impl::write_atomic(dir, &target, &tmp, &body)
    }
}

/// ADR-091 Amendment 3 Plank F1: a metadata-only mtime touch of this
/// process's already-written heartbeat — no data write, mirroring
/// [`touch_beacon`]'s mechanism and opened-directory-descriptor discipline
/// exactly. Must run on every sweep tick where the warn condition persists
/// and the heartbeat's content has not changed; a content change still
/// goes through [`write_heartbeat`]. Callers must not assume the target
/// exists — enumeration can delete a slow writer's heartbeat while its
/// span is still live — and must recreate via [`write_heartbeat`] on any
/// touch failure, never treat the record as gone for good.
pub fn touch_heartbeat(dir: &Path, pid: u32) -> io::Result<()> {
    let name = format!("{pid}.json");
    #[cfg(unix)]
    {
        let handle = unix_impl::SidecarDirHandle::open_or_create(dir)?;
        handle.touch_mtime(&name)
    }
    #[cfg(windows)]
    {
        windows_impl::touch_mtime(dir, &name)
    }
}

/// Remove this process's registration beacon, if present (fail-closed
/// escalation for a failing heartbeat write path — see the sidecar
/// `observe` logic in `khive-db`'s checkpoint module). Never follows a
/// symlink at the target path. A missing sidecar directory is a no-op — it
/// must NOT be created as a side effect of a removal.
pub fn remove_beacon(dir: &Path, pid: u32) -> io::Result<()> {
    let target = format!("{pid}.beacon");
    #[cfg(unix)]
    {
        match unix_impl::SidecarDirHandle::open_if_exists(dir)? {
            Some(handle) => handle.remove_checked(&target),
            None => Ok(()),
        }
    }
    #[cfg(windows)]
    {
        windows_impl::remove_checked(dir, &target)
    }
}

/// Remove this process's heartbeat file, if present. Never follows a
/// symlink at the target path. A missing sidecar directory is a no-op — it
/// must NOT be created as a side effect of a removal.
pub fn remove_heartbeat(dir: &Path, pid: u32) -> io::Result<()> {
    let target = format!("{pid}.json");
    #[cfg(unix)]
    {
        match unix_impl::SidecarDirHandle::open_if_exists(dir)? {
            Some(handle) => handle.remove_checked(&target),
            None => Ok(()),
        }
    }
    #[cfg(windows)]
    {
        windows_impl::remove_checked(dir, &target)
    }
}

/// Write this process's one-time registration beacon (ADR-091 Amendment 2
/// sidecar-health attribution). Written once at sidecar initialization; see
/// [`touch_beacon`] for the required per-tick freshness refresh — a beacon
/// that is never refreshed again classifies as stale, never
/// `registered-silent` (beacon refresh rule).
pub fn write_beacon(dir: &Path, beacon: &WalpinBeacon) -> io::Result<()> {
    let body =
        serde_json::to_vec(beacon).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let target = format!("{}.beacon", beacon.pid);
    let tmp = format!(".{}.beacon.tmp", beacon.pid);
    #[cfg(unix)]
    {
        let handle = unix_impl::SidecarDirHandle::open_or_create(dir)?;
        handle.write_atomic(&target, &tmp, &body)
    }
    #[cfg(windows)]
    {
        windows_impl::write_atomic(dir, &target, &tmp, &body)
    }
}

/// ADR-091 Amendment 2 beacon refresh rule: a metadata-only mtime touch of
/// this process's already-written beacon — no data write, preserving the
/// zero-steady-state-data-traffic property. Must run on every sweep tick
/// while the beacon exists: `registered-silent` classification requires the
/// refresh timestamp (not just the original write) to stay within the
/// freshness window.
pub fn touch_beacon(dir: &Path, pid: u32) -> io::Result<()> {
    let name = format!("{pid}.beacon");
    #[cfg(unix)]
    {
        let handle = unix_impl::SidecarDirHandle::open_or_create(dir)?;
        handle.touch_mtime(&name)
    }
    #[cfg(windows)]
    {
        windows_impl::touch_mtime(dir, &name)
    }
}

/// Path of `pid`'s one-time registration beacon under `dir`.
pub fn beacon_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}.beacon"))
}

/// Is `pid` alive (right now)? On Unix, `kill(pid, 0)` is a pure
/// existence/permission probe with no side effects (`EPERM` — a live PID
/// owned by someone else — still counts as alive). On Windows,
/// `OpenProcess` + `GetExitCodeProcess` checking for `STILL_ACTIVE`.
pub fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unix_impl::is_process_alive(pid)
    }
    #[cfg(windows)]
    {
        windows_impl::is_process_alive(pid)
    }
}

/// PID spelling used by the local process census.
pub fn reporting_pid() -> u32 {
    #[cfg(target_os = "linux")]
    {
        census_visible_self_pid().unwrap_or_else(std::process::id)
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::id()
    }
}

/// Coarsest uncertainty of the process start-time value returned below.
#[cfg(target_os = "macos")]
pub fn start_time_resolution_secs() -> Option<u64> {
    Some(1)
}

#[cfg(target_os = "linux")]
/// Coarsest uncertainty of the Linux process start-time value.
pub fn start_time_resolution_secs() -> Option<u64> {
    Some(2)
}

#[cfg(windows)]
/// Coarsest uncertainty of the Windows process start-time value.
pub fn start_time_resolution_secs() -> Option<u64> {
    Some(1)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
/// Process start-time values are unavailable on this platform.
pub fn start_time_resolution_secs() -> Option<u64> {
    None
}

/// The OS-reported start time of `pid`, in epoch seconds, or `None` if it
/// cannot be determined (dead PID, permission denied, or an unsupported
/// platform). Used as the required identity check in [`enumerate_live`] —
/// `None` is treated as "cannot verify," which fails the gate rather than
/// passing it.
#[cfg(target_os = "macos")]
pub fn process_start_time_secs(pid: u32) -> Option<i64> {
    use std::os::raw::{c_int, c_void};

    const PROC_PIDTBSDINFO: c_int = 3;
    const MAXCOMLEN: usize = 16;

    // Mirrors Darwin's `struct proc_bsdinfo` (`<sys/proc_info.h>`), a stable
    // public ABI used by `libproc`'s `proc_pidinfo`. Only the layout up to
    // and including `pbi_start_tvsec`/`pbi_start_tvusec` matters here.
    #[repr(C)]
    struct ProcBsdInfo {
        pbi_flags: u32,
        pbi_status: u32,
        pbi_xstatus: u32,
        pbi_pid: u32,
        pbi_ppid: u32,
        pbi_uid: u32,
        pbi_gid: u32,
        pbi_ruid: u32,
        pbi_rgid: u32,
        pbi_svuid: u32,
        pbi_svgid: u32,
        rfu_1: u32,
        pbi_comm: [u8; MAXCOMLEN],
        pbi_name: [u8; 2 * MAXCOMLEN],
        pbi_nfiles: u32,
        pbi_pgid: u32,
        pbi_pjobc: u32,
        e_tdev: u32,
        e_tpgid: u32,
        pbi_nice: i32,
        pbi_start_tvsec: u64,
        pbi_start_tvusec: u64,
    }

    #[link(name = "proc")]
    extern "C" {
        fn proc_pidinfo(
            pid: c_int,
            flavor: c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
    }

    let pid_i32 = i32::try_from(pid).ok()?;
    let mut info: ProcBsdInfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<ProcBsdInfo>() as c_int;
    // SAFETY: `info` is a valid, zeroed, appropriately-sized buffer for the
    // duration of this call; `proc_pidinfo` writes at most `size` bytes.
    let ret = unsafe {
        proc_pidinfo(
            pid_i32,
            PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut c_void,
            size,
        )
    };
    if ret != size {
        return None;
    }
    i64::try_from(info.pbi_start_tvsec).ok()
}

/// Linux: derive process start time from `/proc/<pid>/stat` field 22
/// (`starttime`, in clock ticks since boot) plus `/proc/stat`'s `btime`
/// (system boot time, epoch seconds).
#[cfg(target_os = "linux")]
pub fn process_start_time_secs(pid: u32) -> Option<i64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `comm` (field 2) is parenthesized and may itself contain spaces or
    // parens, so locate fields from the LAST ')' rather than splitting naively.
    let rparen = stat.rfind(')')?;
    let rest = stat.get(rparen + 1..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // `rest` starts at field 3 (state); field 22 (starttime) is index 22-3=19.
    let starttime_ticks: u64 = fields.get(19)?.parse().ok()?;

    // SAFETY: `_SC_CLK_TCK` is a pure query with no side effects.
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if clk_tck <= 0 {
        return None;
    }
    let secs_since_boot = starttime_ticks / clk_tck as u64;

    let stat_all = fs::read_to_string("/proc/stat").ok()?;
    let btime = stat_all.lines().find_map(|line| {
        line.strip_prefix("btime ")
            .and_then(|v| v.trim().parse::<i64>().ok())
    })?;
    Some(btime + secs_since_boot as i64)
}

/// Windows: `OpenProcess` + `GetProcessTimes`' creation-time `FILETIME`,
/// converted from 100ns-since-1601 to Unix epoch seconds.
#[cfg(windows)]
pub fn process_start_time_secs(pid: u32) -> Option<i64> {
    windows_impl::process_start_time_secs(pid)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn process_start_time_secs(_pid: u32) -> Option<i64> {
    None
}

#[cfg(any(unix, test))]
fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Staleness window for a producer sweeping at `interval`: three missed
/// ticks of a cadence floored at one second (ADR-091 Amendment 3 Plank F1's
/// `3 x max(interval, 1000ms)`) — a sub-second interval must not collapse
/// the window below what mtime resolution can distinguish, which would make
/// any timestamp other than the current wall-clock second appear stale.
#[cfg(unix)]
fn stale_window_from(interval: Duration) -> i64 {
    // ADR-091 Amendment 3 Plank F1's determinate form is `3 x
    // max(declared cadence, 1000ms)` — clamp the interval to the
    // mtime-resolution floor FIRST, then multiply by three, so a
    // sub-second cadence floors the effective window at three seconds
    // rather than merely at one. `max(3*interval, 1s)` (multiplying
    // first) would under-floor any cadence below ~333ms.
    interval
        .max(Duration::from_secs(1))
        .saturating_mul(3)
        .as_secs() as i64
}

/// Per-record staleness window: the producer's own recorded cadence wins;
/// `0` (a record written before `sweep_interval_ms` existed) falls back to
/// the enumerator's window.
#[cfg(unix)]
fn stale_window_secs(producer_interval_ms: u64, fallback_secs: i64) -> i64 {
    if producer_interval_ms == 0 {
        fallback_secs
    } else {
        stale_window_from(Duration::from_millis(producer_interval_ms))
    }
}

/// Absolute difference of two epoch-second stamps without overflow.
/// Persisted `started_at`/`updated_at` fields deserialize as unrestricted
/// i64, and plain `(a - b).abs()` wraps on extreme values in release
/// builds — a wrapped difference can land inside a freshness window and
/// classify a malformed entry as fresh. Saturating to `u64::MAX` on
/// overflow keeps any extreme stamp outside every window, failing toward
/// `Unknown` rather than exoneration.
#[cfg(unix)]
fn epoch_abs_diff(a: i64, b: i64) -> u64 {
    a.checked_sub(b)
        .map(|d| d.unsigned_abs())
        .unwrap_or(u64::MAX)
}

/// Enumerate the sidecar directory, applying the three-test liveness gate
/// to every heartbeat/beacon entry found and
/// classifying each PID's sidecar health three ways (ADR-091 Amendment 2
/// "Sidecar-health attribution"): [`WalpinPidHealth::Reporting`] (live,
/// identity-matched, fresh heartbeat), [`WalpinPidHealth::RegisteredSilent`]
/// (live, identity-matched, FRESHLY-REFRESHED beacon, no live heartbeat), or
/// [`WalpinPidHealth::Unknown`] (an entry exists but the trust-boundary check
/// refused it, failed to parse, or went stale — sidecar health for that PID
/// is unestablished).
///
/// Trust boundary (binding): the directory itself is
/// validated (type/owner/mode) BEFORE any entry is read — a non-compliant
/// directory returns `Err`, a health *failure*, never a partial/empty
/// result that could otherwise masquerade as "no live entries." Per entry,
/// symlinks and non-owned files are refused BEFORE their contents are read
/// (contributing an `Unknown` classification, not silently skipped). At
/// most `MAX_SIDECAR_ENTRIES` entries are listed and read per enumeration
/// — the bound applies at the `readdir` loop itself — and a directory
/// holding more contributes one sentinel `Unknown` marker (PID 0) so the
/// truncation is never silent.
///
/// Beacon refresh rule (ADR-091 Amendment 2): registration at
/// initialization alone never licenses `RegisteredSilent` — a beacon (or
/// heartbeat) that fails the identity gate (dead PID, reused PID) is genuine
/// absence (deleted, no entry at all: there is no evidence of THIS process),
/// but one that passes identity and STILL goes stale (its refresh mtime
/// falls outside the freshness window) is a wedged sidecar: classified
/// `Unknown`, deleted, and — critically — that PID is barred from later
/// resolving to `RegisteredSilent` off a co-existing beacon/heartbeat, per
/// "a PID whose heartbeat was deleted as stale classifies as unknown, never
/// registered-silent."
///
/// This function is Unix-only: its sole caller is the daemon's checkpoint
/// task, and daemon mode itself requires Unix. A missing directory (sidecar
/// never used yet) is `Ok` with an empty report, distinct from an
/// existing-but-untrustworthy one.
#[cfg(unix)]
pub fn enumerate_live(dir: &Path, sweep_interval: Duration) -> io::Result<WalpinReport> {
    enumerate_live_bounded(
        dir,
        sweep_interval,
        MAX_SIDECAR_ENTRIES,
        EnumerationPurpose::Attribution,
    )
}

/// Read-only sidecar classification for operator diagnostics. It shares the
/// attribution path's handle-bound trust checks and work bounds but never
/// unlinks a regular entry or producer temp, even when the evidence proves it
/// stale. The returned report states what housekeeping would reap.
#[cfg(unix)]
pub(crate) fn inspect_live(dir: &Path, sweep_interval: Duration) -> io::Result<WalpinReport> {
    enumerate_live_bounded(
        dir,
        sweep_interval,
        MAX_SIDECAR_ENTRIES,
        EnumerationPurpose::Diagnostics,
    )
}

/// Run the ordinary-tick, bounded sidecar housekeeping pass.
///
/// This uses the same trust checks, liveness classification, and
/// `MAX_SIDECAR_ENTRIES` work bound as [`enumerate_live`], but removes only
/// residue whose producer is positively dead or whose PID has been reused.
/// Malformed, uninspectable, and live-but-stale records remain on disk so a
/// later TRUNCATE-no-progress attribution pass can consume their `Unknown`
/// evidence instead of observing a falsely clean directory.
#[cfg(unix)]
pub(crate) fn housekeep_live(
    dir: &Path,
    legacy_sweep_interval: Duration,
) -> io::Result<WalpinReport> {
    enumerate_live_bounded(
        dir,
        legacy_sweep_interval,
        MAX_SIDECAR_ENTRIES,
        EnumerationPurpose::Housekeeping,
    )
}

/// Ceiling on sidecar entries listed and read per enumeration. After ADR-091
/// Amendment 5 there is no checkpoint writer guard on this path. For daemon
/// checkpoint callers, the cap instead bounds the per-tick filesystem work
/// admitted to the awaited blocking worker, the latency attributable to that
/// work, and memory retained by the returned report — the entry-count sibling
/// of the per-entry `MAX_SIDECAR_ENTRY_BYTES` bound. A real population is one
/// heartbeat/beacon pair per live process; a directory holding more than this
/// contributes one `CAP_SENTINEL_PID` `Unknown` marker (fail-closed:
/// unenumerated entries make the census inconclusive, never exonerated).
#[cfg(unix)]
const MAX_SIDECAR_ENTRIES: usize = 512;

/// Sentinel PID carried by the `Unknown` marker for entries past the
/// enumeration cap: those entries were never listed, so no real PID is
/// available. PID 0 is the kernel scheduler on every supported Unix and can
/// never be a sidecar producer.
#[cfg(unix)]
const CAP_SENTINEL_PID: u32 = 0;

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum EnumerationPurpose {
    /// Consume a fresh classification for a TRUNCATE-no-progress report.
    /// Unknown trusted residue is retained in this pass's report and removed
    /// from disk so it cannot accumulate indefinitely.
    Attribution,
    /// Ordinary healthy-tick collection. Only positively dead/reused-PID
    /// residue may be removed; uncertain evidence stays available for a later
    /// attribution pass.
    Housekeeping,
    /// Operator diagnostics: classify and reconcile without deleting any
    /// sidecar evidence.
    Diagnostics,
}

#[cfg(unix)]
impl EnumerationPurpose {
    fn removes_uncertain_evidence(self) -> bool {
        self == Self::Attribution
    }

    fn removes_dead_or_reused_evidence(self) -> bool {
        self != Self::Diagnostics
    }

    fn removes_orphan_temps(self) -> bool {
        self != Self::Diagnostics
    }
}

/// The outcome of examining one producer-temp candidate against its recorded
/// identity. Liveness alone never licenses a reap: a malformed or mismatched
/// dead-PID temp is exactly the evidence a later TRUNCATE-no-progress
/// attribution pass needs, so it must survive cleanup as `Untrusted`, never
/// fall through to `Reap`.
#[cfg(unix)]
enum OrphanTempVerdict {
    /// Not old enough yet, not owned by us, or a live producer still holding
    /// a matching identity — no report, ordinary in-flight state.
    Skip,
    /// Confirmed dead-PID or PID-reused evidence, identity verified against
    /// the filename.
    Reap(unix_impl::CheckedEntry),
    /// Old enough to act on, but the body does not parse for its recorded
    /// kind or its recorded identity does not match the filename — retained
    /// and reported regardless of whether the named PID is alive or dead.
    Untrusted(&'static str),
}

// A live producer's `proc_pidinfo`/`/proc` lookup can fail for reasons that
// have nothing to do with the temp's trustworthiness (a permission boundary
// on a shared host, a `/proc` mount restriction) — forcing that outcome from
// a portable test isn't practical, so this thread-local one-shot override is
// the seam.
#[cfg(all(unix, test))]
thread_local! {
    static STALE_ORPHAN_TEMP_START_TIME_OVERRIDE: std::cell::Cell<Option<Option<i64>>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(all(unix, test))]
fn set_stale_orphan_temp_start_time_override(value: Option<i64>) {
    STALE_ORPHAN_TEMP_START_TIME_OVERRIDE.with(|cell| cell.set(Some(value)));
}

#[cfg(unix)]
fn stale_orphan_temp_actual_start(pid: u32) -> Option<i64> {
    #[cfg(test)]
    if let Some(overridden) = STALE_ORPHAN_TEMP_START_TIME_OVERRIDE.with(|cell| cell.take()) {
        return overridden;
    }
    process_start_time_secs(pid)
}

#[cfg(unix)]
fn stale_orphan_temp(
    handle: &unix_impl::SidecarDirHandle,
    name: &str,
    pid: u32,
    kind: ProducerTempKind,
    now: i64,
    stale_after_secs: i64,
) -> io::Result<OrphanTempVerdict> {
    let entry = match handle.read_checked_entry(name) {
        Ok(Some(entry)) => entry,
        Ok(None) => return Ok(OrphanTempVerdict::Skip),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            return Ok(OrphanTempVerdict::Untrusted(
                "refused: producer temp not owned by current user",
            ));
        }
        Err(e) => return Err(e),
    };
    let modified_at = entry
        .mtime
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    if now.saturating_sub(modified_at) <= stale_after_secs {
        return Ok(OrphanTempVerdict::Skip);
    }

    let recorded_identity = match kind {
        ProducerTempKind::Heartbeat => serde_json::from_slice::<WalpinHeartbeat>(&entry.body)
            .ok()
            .map(|record| (record.pid, record.started_at)),
        ProducerTempKind::Beacon => serde_json::from_slice::<WalpinBeacon>(&entry.body)
            .ok()
            .map(|record| (record.pid, record.started_at)),
    };
    let Some((recorded_pid, recorded_start)) = recorded_identity else {
        return Ok(OrphanTempVerdict::Untrusted(
            "refused: producer temp body does not parse as its recorded kind",
        ));
    };
    if recorded_pid != pid {
        return Ok(OrphanTempVerdict::Untrusted(
            "refused: producer temp identity does not match its filename",
        ));
    }

    if !is_process_alive(pid) {
        return Ok(OrphanTempVerdict::Reap(entry));
    }
    let Some(actual_start) = stale_orphan_temp_actual_start(pid) else {
        return Ok(OrphanTempVerdict::Untrusted(
            "refused: producer temp process start time unavailable",
        ));
    };
    if epoch_abs_diff(actual_start, recorded_start) > START_TIME_EPSILON_SECS {
        return Ok(OrphanTempVerdict::Reap(entry));
    }
    Ok(OrphanTempVerdict::Skip)
}

#[cfg(unix)]
fn enumerate_live_bounded(
    dir: &Path,
    sweep_interval: Duration,
    max_entries: usize,
    purpose: EnumerationPurpose,
) -> io::Result<WalpinReport> {
    let handle = match unix_impl::SidecarDirHandle::open_if_exists(dir) {
        Ok(Some(h)) => h,
        Ok(None) => return Ok(WalpinReport::default()),
        Err(e) => return Err(e),
    };

    let now = now_epoch_secs();
    // Fallback window for records that predate the `sweep_interval_ms`
    // field — records carrying their producer's own cadence are judged
    // against it instead (see `stale_window_secs`), so a session sweeping
    // on an independently slower configured interval is not misread as
    // stale by a faster-ticking daemon.
    let fallback_window_secs = stale_window_from(sweep_interval);

    let mut heartbeats: std::collections::HashMap<u32, WalpinHeartbeat> = Default::default();
    let mut beacon_pids: std::collections::HashSet<u32> = Default::default();
    let mut unknown: Vec<(u32, &'static str)> = Vec::new();
    // PIDs whose heartbeat or beacon passed the identity gate but failed
    // freshness — these are wedged, not absent, and must never resolve to
    // `RegisteredSilent` off a co-existing entry (item b).
    let mut wedged: std::collections::HashSet<u32> = Default::default();

    // Entry-count bound: listing itself stops at the cap (see
    // `list_names`), so neither the readdir loop, the names allocation,
    // nor this processing loop scales with directory content. A truncated
    // listing contributes one sentinel `Unknown` marker below — the
    // unlisted entries were never read, and the census stays inconclusive
    // rather than exonerating.
    let (names, producer_temps, truncated) = handle.list_names(max_entries)?;
    if truncated {
        unknown.push((
            CAP_SENTINEL_PID,
            "refused: sidecar entry count exceeds enumeration cap",
        ));
    }
    let mut cleanup_would_reap = 0usize;
    let mut orphan_temps_reaped = 0usize;
    for name in producer_temps {
        let Some((pid, kind)) = producer_temp_identity(&name) else {
            continue;
        };
        match stale_orphan_temp(&handle, &name, pid, kind, now, fallback_window_secs) {
            Ok(OrphanTempVerdict::Reap(entry)) => {
                cleanup_would_reap = cleanup_would_reap.saturating_add(1);
                if purpose.removes_orphan_temps() {
                    match handle.remove_if_same(&name, &entry) {
                        Ok(true) => orphan_temps_reaped = orphan_temps_reaped.saturating_add(1),
                        Ok(false) => unknown.push((
                            pid,
                            "producer temp changed while orphan cleanup was in progress",
                        )),
                        Err(_) => unknown
                            .push((pid, "refused: producer temp changed to an untrusted entry")),
                    }
                }
            }
            Ok(OrphanTempVerdict::Skip) => {}
            Ok(OrphanTempVerdict::Untrusted(reason)) => unknown.push((pid, reason)),
            Err(_) => unknown.push((
                pid,
                "refused: untrusted producer temp (symlink, non-regular, or oversized)",
            )),
        }
    }
    for name in names {
        let is_heartbeat = name.ends_with(".json");
        let is_beacon = name.ends_with(".beacon");
        if !is_heartbeat && !is_beacon {
            continue;
        }
        let Some(pid) = name
            .rsplit_once('.')
            .and_then(|(stem, _)| stem.parse::<u32>().ok())
        else {
            continue;
        };

        // Trust boundary: symlink/ownership refusal happens BEFORE any
        // content read, and contributes `Unknown` rather than being
        // silently dropped — the entry's health is unestablished, not
        // exonerating.
        let (body, mtime) = match handle.read_checked(&name) {
            Ok(Some(v)) => v,
            Ok(None) => continue, // raced away between listing and reading
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                unknown.push((pid, "refused: sidecar entry not owned by current user"));
                continue;
            }
            Err(_) => {
                unknown.push((
                    pid,
                    "refused: untrusted sidecar entry (symlink, non-regular, or oversized)",
                ));
                continue;
            }
        };
        let mtime_secs = mtime
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        if is_heartbeat {
            let heartbeat: WalpinHeartbeat = match serde_json::from_slice(&body) {
                Ok(hb) => hb,
                Err(_) => {
                    if purpose.removes_uncertain_evidence()
                        || (purpose.removes_dead_or_reused_evidence() && !is_process_alive(pid))
                    {
                        let _ = handle.unlink_tolerant(&name);
                    }
                    wedged.insert(pid);
                    unknown.push((pid, "malformed walpin heartbeat entry"));
                    continue;
                }
            };
            if heartbeat.pid != pid {
                if purpose.removes_uncertain_evidence() {
                    let _ = handle.unlink_tolerant(&name);
                }
                wedged.insert(pid);
                unknown.push((pid, "walpin heartbeat PID does not match its entry name"));
                continue;
            }
            let alive = is_process_alive(heartbeat.pid);
            let actual_start = if alive {
                process_start_time_secs(heartbeat.pid)
            } else {
                None
            };
            let identity_ok = actual_start
                .map(|actual| {
                    epoch_abs_diff(actual, heartbeat.started_at) <= START_TIME_EPSILON_SECS
                })
                .unwrap_or(false);
            if !identity_ok {
                let positively_dead_or_reused = !alive
                    || actual_start.is_some_and(|actual| {
                        epoch_abs_diff(actual, heartbeat.started_at) > START_TIME_EPSILON_SECS
                    });
                if purpose.removes_uncertain_evidence()
                    || (purpose.removes_dead_or_reused_evidence() && positively_dead_or_reused)
                {
                    let _ = handle.unlink_tolerant(&name);
                } else {
                    wedged.insert(pid);
                    unknown.push((pid, "walpin heartbeat identity could not be verified"));
                }
                continue;
            }
            // ADR-091 Amendment 3 Plank F1: a record carrying
            // `oldest_tx_started_at` is new-style — its body is only
            // rewritten on content change, so freshness is judged against
            // the entry's mtime (advanced by a metadata-only touch every
            // tick), never the possibly-stale `updated_at` body field. A
            // record without it predates this amendment and is read
            // exactly as before: `updated_at` is its own freshness field.
            // Either way the window is the PRODUCER's recorded cadence,
            // not the enumerator's — the mixed-version rule (readers accept
            // both generations; see the amendment) depends on this branch.
            let window = stale_window_secs(heartbeat.sweep_interval_ms, fallback_window_secs);
            let hb_fresh = if heartbeat.oldest_tx_started_at.is_some() {
                epoch_abs_diff(now, mtime_secs) <= window as u64
            } else {
                epoch_abs_diff(now, heartbeat.updated_at) <= window as u64
            };
            if !hb_fresh {
                if purpose.removes_uncertain_evidence() {
                    let _ = handle.unlink_tolerant(&name);
                }
                wedged.insert(pid);
                unknown.push((pid, "stale walpin heartbeat"));
                continue;
            }
            heartbeats.insert(heartbeat.pid, heartbeat);
        } else {
            let beacon: WalpinBeacon = match serde_json::from_slice(&body) {
                Ok(b) => b,
                Err(_) => {
                    if purpose.removes_uncertain_evidence()
                        || (purpose.removes_dead_or_reused_evidence() && !is_process_alive(pid))
                    {
                        let _ = handle.unlink_tolerant(&name);
                    }
                    wedged.insert(pid);
                    unknown.push((pid, "malformed walpin beacon entry"));
                    continue;
                }
            };
            if beacon.pid != pid {
                if purpose.removes_uncertain_evidence() {
                    let _ = handle.unlink_tolerant(&name);
                }
                wedged.insert(pid);
                unknown.push((pid, "walpin beacon PID does not match its entry name"));
                continue;
            }
            let alive = is_process_alive(beacon.pid);
            let actual_start = if alive {
                process_start_time_secs(beacon.pid)
            } else {
                None
            };
            let identity_ok = actual_start
                .map(|actual| epoch_abs_diff(actual, beacon.started_at) <= START_TIME_EPSILON_SECS)
                .unwrap_or(false);
            if !identity_ok {
                let positively_dead_or_reused = !alive
                    || actual_start.is_some_and(|actual| {
                        epoch_abs_diff(actual, beacon.started_at) > START_TIME_EPSILON_SECS
                    });
                if purpose.removes_uncertain_evidence()
                    || (purpose.removes_dead_or_reused_evidence() && positively_dead_or_reused)
                {
                    let _ = handle.unlink_tolerant(&name);
                } else {
                    wedged.insert(pid);
                    unknown.push((pid, "walpin beacon identity could not be verified"));
                }
                continue;
            }
            // Beacon refresh rule: freshness is the entry's mtime (the
            // metadata-only touch), not any JSON field — the beacon's body
            // is written once and never refreshed. The window is the
            // producer's recorded cadence, not the enumerator's.
            let window = stale_window_secs(beacon.sweep_interval_ms, fallback_window_secs);
            let fresh = epoch_abs_diff(now, mtime_secs) <= window as u64;
            if !fresh {
                if purpose.removes_uncertain_evidence() {
                    let _ = handle.unlink_tolerant(&name);
                }
                wedged.insert(pid);
                unknown.push((pid, "stale walpin beacon"));
                continue;
            }
            beacon_pids.insert(beacon.pid);
        }
    }

    for (pid, _) in &unknown {
        wedged.insert(*pid);
    }

    let mut entries: Vec<WalpinPidHealth> = Vec::new();
    for (pid, hb) in heartbeats {
        if !wedged.contains(&pid) {
            entries.push(WalpinPidHealth::Reporting(hb));
        }
        beacon_pids.remove(&pid);
    }
    for pid in beacon_pids {
        if wedged.contains(&pid) {
            continue; // already carried as `Unknown` via `unknown` above
        }
        entries.push(WalpinPidHealth::RegisteredSilent { pid });
    }
    for (pid, reason) in unknown {
        entries.push(WalpinPidHealth::Unknown { pid, reason });
    }

    Ok(WalpinReport {
        entries,
        sidecar_listing_truncated: truncated,
        cleanup_would_reap,
        orphan_temps_reaped,
    })
}

/// Restore an environment setting within its exact isolated test child,
/// including when an assertion panics. Worker-owning fixtures instead set
/// their fixed configuration before child startup, so this guard is only
/// used by configuration fixtures that do not create background workers.
#[cfg(test)]
pub(crate) struct EnvVarGuard {
    key: &'static str,
    saved: Option<String>,
}
#[cfg(test)]
impl EnvVarGuard {
    pub(crate) fn capture(key: &'static str) -> Self {
        Self {
            key,
            saved: std::env::var(key).ok(),
        }
    }
}
#[cfg(test)]
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.saved {
            Some(v) => crate::test_process::set_var(self.key, v),
            None => crate::test_process::remove_var(self.key),
        }
    }
}

#[cfg(test)]
#[path = "walpin_tests.rs"]
mod tests;
