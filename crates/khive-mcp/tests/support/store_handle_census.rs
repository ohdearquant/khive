use std::io;
use std::path::{Path, PathBuf};

/// A process-local snapshot, including directory handles and unlinked Linux files.
/// An unsupported or incomplete census is an error, never an empty result.
pub fn open_store_handles(root: &Path) -> io::Result<Vec<PathBuf>> {
    let root = root.canonicalize()?;
    let mut paths = process_file_paths()?
        .into_iter()
        .filter(|path| path.starts_with(&root))
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[cfg(target_os = "linux")]
fn process_file_paths() -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir("/proc/self/fd")? {
        match std::fs::read_link(entry?.path()) {
            Ok(path) => paths.push(path),
            // Another task may close its descriptor between enumeration and readlink.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(paths)
}

#[cfg(target_os = "macos")]
fn process_file_paths() -> io::Result<Vec<PathBuf>> {
    use std::ffi::{c_int, c_void, CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct ProcFdInfo {
        descriptor: c_int,
        kind: u32,
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

    const PID_LIST_FDS: c_int = 1;
    const VNODE: u32 = 1;
    let pid = std::process::id() as c_int;
    let item_size = std::mem::size_of::<ProcFdInfo>();
    for _ in 0..4 {
        // SAFETY: a null output buffer requests the required byte count.
        let needed = unsafe { proc_pidinfo(pid, PID_LIST_FDS, 0, std::ptr::null_mut(), 0) };
        if needed <= 0 {
            return Err(io::Error::last_os_error());
        }
        let count = needed as usize / item_size + 64;
        let bytes = c_int::try_from(count * item_size)
            .map_err(|_| io::Error::other("descriptor census exceeds the libproc buffer limit"))?;
        let mut entries = vec![ProcFdInfo::default(); count];
        // SAFETY: entries is initialized and its byte capacity is exactly bytes.
        let returned =
            unsafe { proc_pidinfo(pid, PID_LIST_FDS, 0, entries.as_mut_ptr().cast(), bytes) };
        if returned <= 0 {
            return Err(io::Error::last_os_error());
        }
        if returned >= bytes {
            continue;
        }
        if !(returned as usize).is_multiple_of(item_size) {
            return Err(io::Error::other("partial descriptor census entry"));
        }
        entries.truncate(returned as usize / item_size);
        let mut paths = Vec::new();
        for entry in entries.into_iter().filter(|entry| entry.kind == VNODE) {
            let mut buffer = [0 as libc::c_char; libc::PATH_MAX as usize];
            // SAFETY: F_GETPATH writes at most PATH_MAX bytes to this live buffer.
            let result =
                unsafe { libc::fcntl(entry.descriptor, libc::F_GETPATH, buffer.as_mut_ptr()) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EBADF) {
                    continue;
                }
                return Err(error);
            }
            if !buffer.contains(&0) {
                return Err(io::Error::other("unterminated descriptor path"));
            }
            // SAFETY: the buffer is live and a terminator was checked above.
            let path = unsafe { CStr::from_ptr(buffer.as_ptr()) };
            paths.push(PathBuf::from(OsStr::from_bytes(path.to_bytes())));
        }
        return Ok(paths);
    }
    Err(io::Error::other("descriptor census remained truncated"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_file_paths() -> io::Result<Vec<PathBuf>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "runtime close measurement needs a process file-handle census on this platform",
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn census_detects_live_file_and_directory_handles() {
    let root = tempfile::tempdir().expect("private census root");
    let path = root.path().join("held.data");
    let file = std::fs::File::create(&path).expect("held file");
    let directory = std::fs::File::open(root.path()).expect("held directory");
    let paths = open_store_handles(root.path()).expect("complete positive census");
    assert!(paths.contains(&path.canonicalize().unwrap()), "{paths:?}");
    assert!(
        paths.contains(&root.path().canonicalize().unwrap()),
        "{paths:?}"
    );
    drop(file);
    drop(directory);
    assert!(open_store_handles(root.path())
        .expect("complete negative census")
        .is_empty());
}
