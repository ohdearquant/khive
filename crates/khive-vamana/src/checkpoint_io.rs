//! Handle-relative checkpoint publication. A writable checkpoint directory is
//! not a trusted source of staging paths: never follow a planted link while
//! opening the lock or a temporary segment.

use std::{fs::File, io, path::Path};

pub(crate) struct CheckpointDirectory {
    #[cfg(any(unix, windows))]
    dir: File,
}

impl CheckpointDirectory {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let dir = crate::external_ids::open_dir_with_trusted_symlinks(path)
                .map_err(io::Error::other)?;
            crate::external_ids::verify_original_dir_identity(path, &dir)
                .map_err(io::Error::other)?;
            Ok(Self { dir })
        }
        #[cfg(windows)]
        {
            let dir = crate::external_ids::windows::open_checkpoint_directory(path)?;
            Ok(Self { dir })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure checkpoint publication requires handle-relative filesystem operations",
            ))
        }
    }

    pub(crate) fn open_lock(&self) -> io::Result<File> {
        #[cfg(unix)]
        {
            use std::os::fd::{AsRawFd as _, FromRawFd as _};
            // SAFETY: the directory descriptor and static NUL-terminated name
            // live through this call; a successful descriptor is uniquely owned.
            let fd = unsafe {
                libc::openat(
                    self.dir.as_raw_fd(),
                    c".checkpoint.lock".as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_NOFOLLOW
                        | libc::O_NONBLOCK
                        | libc::O_CLOEXEC,
                    0o644 as libc::c_uint,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` was freshly returned by openat and has one owner.
            let file = unsafe { File::from_raw_fd(fd) };
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "checkpoint lock is not a regular file",
                ));
            }
            Ok(file)
        }
        #[cfg(windows)]
        {
            crate::external_ids::windows::open_checkpoint_lock(&self.dir)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure checkpoint lock unsupported",
            ))
        }
    }

    pub(crate) fn stage(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::{
                ffi::CString,
                io::Write as _,
                os::fd::{AsRawFd as _, FromRawFd as _},
            };
            let name = component_name(name)?;
            let name =
                CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            let dir_fd = self.dir.as_raw_fd();
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: pointers and descriptor are live. AT_SYMLINK_NOFOLLOW
            // inspects the directory entry itself rather than its target.
            let rc = unsafe {
                libc::fstatat(
                    dir_fd,
                    name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if rc == 0 {
                // SAFETY: fstatat succeeded and initialized the structure.
                let stat = unsafe { stat.assume_init() };
                if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "checkpoint staging entry is not a regular file",
                    ));
                }
                // A stale regular staging inode is safe to replace. An entry
                // swapped in after fstatat is only unlinked, never followed.
                // SAFETY: the name and descriptor remain live.
                if unsafe { libc::unlinkat(dir_fd, name.as_ptr(), 0) } != 0 {
                    return Err(io::Error::last_os_error());
                }
            } else {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::NotFound {
                    return Err(error);
                }
            }
            // SAFETY: O_EXCL and O_NOFOLLOW prevent a concurrent planted link
            // from being opened. The returned descriptor has one owner.
            let fd = unsafe {
                libc::openat(
                    dir_fd,
                    name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o644 as libc::c_uint,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fd was freshly returned by openat.
            let mut file = unsafe { File::from_raw_fd(fd) };
            file.write_all(bytes)?;
            file.sync_all()
        }
        #[cfg(windows)]
        {
            component_name(name)?;
            crate::external_ids::windows::stage_checkpoint_file(&self.dir, name, bytes)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (name, bytes);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure checkpoint staging unsupported",
            ))
        }
    }

    pub(crate) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        component_name(from)?;
        component_name(to)?;
        #[cfg(unix)]
        {
            use std::{ffi::CString, os::fd::AsRawFd as _};
            let from =
                CString::new(from).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            let to = CString::new(to).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            let dir_fd = self.dir.as_raw_fd();
            // SAFETY: both names and the pinned directory descriptor are live.
            if unsafe { libc::renameat(dir_fd, from.as_ptr(), dir_fd, to.as_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            crate::external_ids::windows::rename_checkpoint_file(&self.dir, from, to)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (from, to);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure checkpoint rename unsupported",
            ))
        }
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        #[cfg(any(unix, windows))]
        {
            self.dir.sync_all()
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure checkpoint sync unsupported",
            ))
        }
    }
}

fn component_name(name: &str) -> io::Result<&str> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    Ok(name)
}
