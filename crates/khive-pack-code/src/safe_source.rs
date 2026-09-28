use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceReadError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Refused(String),
}

pub(crate) fn open_contained_file(
    canonical_root: &Path,
    source_path: &Path,
) -> Result<File, SourceReadError> {
    #[cfg(unix)]
    let opened = {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt as _;

        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(source_path)
    };
    #[cfg(not(unix))]
    let opened = File::open(source_path);
    let source = opened.map_err(|error| {
        SourceReadError::Refused(format!(
            "cannot open candidate source {}: {error}",
            source_path.display()
        ))
    })?;
    let metadata = source.metadata().map_err(|error| {
        SourceReadError::Refused(format!(
            "cannot verify opened source type {}: {error}",
            source_path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(SourceReadError::Refused(format!(
            "opened source is not a regular file: {}",
            source_path.display()
        )));
    }
    let opened_path = opened_file_path(&source).map_err(|error| {
        SourceReadError::Refused(format!(
            "cannot verify opened source {}: {error}",
            source_path.display()
        ))
    })?;
    if !opened_path.starts_with(canonical_root) {
        return Err(SourceReadError::Refused(format!(
            "opened source escapes the canonical ingest root: {} -> {}",
            source_path.display(),
            opened_path.display()
        )));
    }
    Ok(source)
}

pub(crate) fn read_contained_to_string(
    canonical_root: &Path,
    source_path: &Path,
) -> Result<String, SourceReadError> {
    let mut source = open_contained_file(canonical_root, source_path)?;
    let mut text = String::new();
    source.read_to_string(&mut text)?;
    Ok(text)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn opened_file_path(file: &File) -> io::Result<PathBuf> {
    use std::os::fd::AsRawFd as _;

    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn opened_file_path(file: &File) -> io::Result<PathBuf> {
    use std::ffi::OsStr;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    let mut bytes = vec![0_u8; libc::PATH_MAX as usize];
    // SAFETY: `file` owns a live descriptor; fcntl writes into this buffer
    // during the call and does not retain its pointer.
    let result = unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_GETPATH,
            bytes.as_mut_ptr().cast::<libc::c_void>(),
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    let length = bytes.iter().position(|byte| *byte == 0).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "F_GETPATH returned no NUL terminator",
        )
    })?;
    Ok(PathBuf::from(OsStr::from_bytes(&bytes[..length])))
}

#[cfg(windows)]
fn opened_file_path(file: &File) -> io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED, VOLUME_NAME_DOS,
    };

    let mut path = vec![0_u16; 260];
    loop {
        // SAFETY: `file` owns a live handle and the buffer is writable for
        // the duration of this call.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
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
            return Ok(PathBuf::from(OsString::from_wide(&path)));
        }
        path.resize(length.saturating_add(1), 0);
    }
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
fn opened_file_path(_file: &File) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure opened-source path resolution is unsupported on this Unix target",
    ))
}

#[cfg(not(any(unix, windows)))]
fn opened_file_path(_file: &File) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure opened-source path resolution is unsupported on this target",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    #[test]
    fn source_swapped_to_outside_symlink_after_containment_check_is_refused() {
        let fixture = TempDir::new().expect("fixture");
        let root = fixture.path().join("root");
        fs::create_dir(&root).expect("root");
        let path = root.join("lib.rs");
        fs::write(&path, "pub fn inside() {}\n").expect("inside source");
        let outside = fixture.path().join("outside.rs");
        fs::write(&outside, "pub fn outside() {}\n").expect("outside source");

        let canonical_root = root.canonicalize().expect("canonical root");
        assert!(path
            .canonicalize()
            .expect("checked path")
            .starts_with(&canonical_root));
        fs::remove_file(&path).expect("remove checked source");
        symlink(&outside, &path).expect("swap to outside symlink");

        assert!(matches!(
            read_contained_to_string(&canonical_root, &path),
            Err(SourceReadError::Refused(_))
        ));
    }

    #[test]
    fn parent_directory_swapped_to_outside_symlink_is_refused() {
        let fixture = TempDir::new().expect("fixture");
        let root = fixture.path().join("root");
        let dir = root.join("src");
        fs::create_dir_all(&dir).expect("source directory");
        let path = dir.join("lib.rs");
        fs::write(&path, "pub fn inside() {}\n").expect("inside source");
        let outside = fixture.path().join("outside");
        fs::create_dir(&outside).expect("outside directory");
        fs::write(outside.join("lib.rs"), "pub fn outside() {}\n").expect("outside source");

        let canonical_root = root.canonicalize().expect("canonical root");
        assert!(path
            .canonicalize()
            .expect("checked path")
            .starts_with(&canonical_root));
        fs::rename(&dir, root.join("saved_src")).expect("move checked directory");
        symlink(&outside, &dir).expect("swap parent to outside symlink");

        assert!(matches!(
            read_contained_to_string(&canonical_root, &path),
            Err(SourceReadError::Refused(_))
        ));
    }

    #[test]
    fn regular_file_and_in_root_alias_are_read() {
        let fixture = TempDir::new().expect("fixture");
        let root = fixture.path();
        let path = root.join("lib.rs");
        fs::write(&path, "pub fn inside() {}\n").expect("inside source");
        let alias = root.join("alias.rs");
        symlink(&path, &alias).expect("in-root alias");
        let canonical_root = root.canonicalize().expect("canonical root");

        assert_eq!(
            read_contained_to_string(&canonical_root, &path).expect("regular file"),
            "pub fn inside() {}\n"
        );
        assert_eq!(
            read_contained_to_string(&canonical_root, &alias).expect("in-root alias"),
            "pub fn inside() {}\n"
        );
    }
}
