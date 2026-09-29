//! Output capture (tail-preserving, byte-counting) and run-directory walks.

use std::collections::VecDeque;
use std::io::Read;

use tokio::io::{AsyncRead, AsyncReadExt};

/// Keeps the last `cap` bytes of a stream and counts everything produced.
#[derive(Debug)]
pub struct Tail {
    cap: usize,
    produced: u64,
    buf: VecDeque<u8>,
}

impl Tail {
    pub fn new(cap: u64) -> Self {
        Self {
            cap: cap as usize,
            produced: 0,
            buf: VecDeque::with_capacity(cap.min(1 << 20) as usize),
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.produced += chunk.len() as u64;
        if self.cap == 0 {
            return;
        }
        if chunk.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&chunk[chunk.len() - self.cap..]);
            return;
        }
        let overflow = (self.buf.len() + chunk.len()).saturating_sub(self.cap);
        for _ in 0..overflow {
            self.buf.pop_front();
        }
        self.buf.extend(chunk);
    }

    pub fn produced(&self) -> u64 {
        self.produced
    }

    pub fn retained(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }

    pub fn complete(&self) -> bool {
        self.produced as usize <= self.cap
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainStop {
    Eof,
    Deadline,
    ReadError,
}

pub struct Drained {
    pub tail: Tail,
    pub stop: DrainStop,
}

/// Drain `reader` into a tail buffer until EOF or the run deadline.
pub async fn drain_until<R: AsyncRead + Unpin>(
    mut reader: R,
    cap: u64,
    deadline: tokio::time::Instant,
) -> Drained {
    let mut tail = Tail::new(cap);
    let mut chunk = vec![0u8; 64 * 1024];
    let stop = loop {
        if tokio::time::Instant::now() >= deadline {
            break DrainStop::Deadline;
        }
        match tokio::time::timeout_at(deadline, reader.read(&mut chunk)).await {
            Ok(Ok(0)) => break DrainStop::Eof,
            Ok(Ok(n)) => tail.push(&chunk[..n]),
            Ok(Err(_)) => break DrainStop::ReadError,
            Err(_) => break DrainStop::Deadline,
        }
    };
    Drained { tail, stop }
}

/// One bounded file capture, with its content hash accumulated while reading.
#[derive(Debug)]
pub struct CapturedContent {
    pub bytes: Vec<u8>,
    pub digest: String,
}

#[derive(Debug)]
pub enum CaptureRead {
    Complete(CapturedContent),
    TooLarge { observed_at_least: u64 },
}

fn read_regular_bounded(
    mut reader: impl Read,
    advertised_len: u64,
    max_bytes: u64,
) -> std::io::Result<CaptureRead> {
    // Sparse files are refused from opened-file metadata before allocating
    // or reading them. Recheck the actual stream because a tool can grow a
    // file between metadata inspection and EOF.
    if advertised_len > max_bytes {
        return Ok(CaptureRead::TooLarge {
            observed_at_least: advertised_len,
        });
    }
    let mut bytes = Vec::new();
    let mut hasher = blake3::Hasher::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        let observed = (bytes.len() as u64).saturating_add(n as u64);
        if observed > max_bytes {
            return Ok(CaptureRead::TooLarge {
                observed_at_least: observed,
            });
        }
        hasher.update(&chunk[..n]);
        bytes.extend_from_slice(&chunk[..n]);
    }
    Ok(CaptureRead::Complete(CapturedContent {
        bytes,
        digest: hasher.finalize().to_hex().to_string(),
    }))
}

#[cfg(unix)]
mod platform {
    use super::{read_regular_bounded, CaptureRead, CapturedContent};
    use std::collections::BTreeMap;
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;
    use std::sync::Arc;

    #[derive(Debug)]
    pub struct CaptureRoot {
        directory: Arc<File>,
    }

    impl CaptureRoot {
        pub fn open(path: &Path) -> io::Result<Self> {
            let directory = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)?;
            Ok(Self {
                directory: Arc::new(directory),
            })
        }
    }

    #[derive(Debug, Clone)]
    pub struct Found {
        // Reopen checked ancestors at read time instead of pinning one fd per directory.
        root: Arc<File>,
        parent_path: Arc<Vec<DirectoryComponent>>,
        name: OsString,
        identity: Identity,
        pub mode: u32,
    }

    #[derive(Debug, Clone)]
    struct DirectoryComponent {
        name: OsString,
        identity: Identity,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Identity {
        dev: libc::dev_t,
        ino: libc::ino_t,
        kind: libc::mode_t,
    }

    impl From<&libc::stat> for Identity {
        fn from(stat: &libc::stat) -> Self {
            Self {
                dev: stat.st_dev,
                ino: stat.st_ino,
                kind: stat.st_mode & libc::S_IFMT,
            }
        }
    }

    fn c_name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in capture name"))
    }

    fn stat_fd(file: &File) -> io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::uninit();
        if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { stat.assume_init() })
    }

    fn stat_at(parent: &File, name: &OsStr) -> io::Result<libc::stat> {
        let name = c_name(name)?;
        let mut stat = std::mem::MaybeUninit::uninit();
        if unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { stat.assume_init() })
    }

    fn changed() -> io::Error {
        io::Error::other("capture entry changed after inspection")
    }

    fn open_at(parent: &File, name: &OsStr, directory: bool) -> io::Result<File> {
        let name = c_name(name)?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 };
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn open_checked(
        parent: &File,
        name: &OsStr,
        expected: Identity,
    ) -> io::Result<(File, libc::stat)> {
        let directory = expected.kind == libc::S_IFDIR;
        let file = match open_at(parent, name, directory) {
            Ok(file) => file,
            Err(error) => {
                if stat_at(parent, name).is_ok_and(|current| Identity::from(&current) != expected) {
                    return Err(changed());
                }
                return Err(error);
            }
        };
        let stat = stat_fd(&file)?;
        if Identity::from(&stat) != expected {
            return Err(changed());
        }
        Ok((file, stat))
    }

    /// Re-resolve from the pre-launch root, refusing changed or linked ancestors.
    fn open_directory_path(root: &File, path: &[DirectoryComponent]) -> io::Result<Option<File>> {
        let mut opened = None;
        for component in path {
            let parent = opened.as_ref().unwrap_or(root);
            let (child, _) = open_checked(parent, &component.name, component.identity)?;
            opened = Some(child);
        }
        Ok(opened)
    }

    struct DirStream(*mut libc::DIR);

    impl Drop for DirStream {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.0) };
        }
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "freebsd"
    ))]
    fn errno_location() -> *mut libc::c_int {
        unsafe { libc::__error() }
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "emscripten",
        target_os = "redox",
        target_os = "hurd"
    ))]
    fn errno_location() -> *mut libc::c_int {
        unsafe { libc::__errno_location() }
    }

    #[cfg(any(
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "android",
        target_os = "cygwin",
        target_os = "nuttx"
    ))]
    fn errno_location() -> *mut libc::c_int {
        unsafe { libc::__errno() }
    }

    #[cfg(any(target_os = "solaris", target_os = "illumos"))]
    fn errno_location() -> *mut libc::c_int {
        unsafe { libc::___errno() }
    }

    #[cfg(target_os = "aix")]
    fn errno_location() -> *mut libc::c_int {
        unsafe { libc::_Errno() }
    }

    #[cfg(target_os = "haiku")]
    fn errno_location() -> *mut libc::c_int {
        unsafe { libc::_errnop() }
    }

    fn names(directory: &File) -> io::Result<Vec<OsString>> {
        let fd = open_at(directory, OsStr::new("."), true)?.into_raw_fd();
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(error);
        }
        let stream = DirStream(stream);
        let mut result = Vec::new();
        loop {
            unsafe { *errno_location() = 0 };
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(0) {
                    return Err(error);
                }
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                result.push(OsString::from_vec(name.to_vec()));
            }
        }
        result.sort();
        Ok(result)
    }

    fn read_link_at(parent: &File, name: &OsStr, max_bytes: u64) -> io::Result<CaptureRead> {
        let name = c_name(name)?;
        let ceiling = max_bytes.saturating_add(1).min(usize::MAX as u64) as usize;
        let mut capacity = ceiling.clamp(1, 256);
        loop {
            let mut bytes = vec![0u8; capacity];
            let len = unsafe {
                libc::readlinkat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                )
            };
            if len < 0 {
                return Err(io::Error::last_os_error());
            }
            if len as usize == capacity {
                if capacity == ceiling {
                    return Ok(CaptureRead::TooLarge {
                        observed_at_least: capacity as u64,
                    });
                }
                capacity = capacity.saturating_mul(2).min(ceiling);
                continue;
            }
            bytes.truncate(len as usize);
            return Ok(CaptureRead::Complete(CapturedContent {
                digest: blake3::hash(&bytes).to_hex().to_string(),
                bytes,
            }));
        }
    }

    impl Found {
        pub fn read_content_bounded(&self, max_bytes: u64) -> io::Result<CaptureRead> {
            let opened_parent = open_directory_path(&self.root, &self.parent_path)?;
            let parent = opened_parent.as_ref().unwrap_or(&self.root);
            if self.mode == 120000 {
                let checked = stat_at(parent, &self.name)?;
                if Identity::from(&checked) != self.identity {
                    return Err(changed());
                }
                let content = read_link_at(parent, &self.name, max_bytes)?;
                if Identity::from(&stat_at(parent, &self.name)?) != self.identity {
                    return Err(changed());
                }
                Ok(content)
            } else {
                let (file, stat) = open_checked(parent, &self.name, self.identity)?;
                read_regular_bounded(file, stat.st_size.max(0) as u64, max_bytes)
            }
        }
    }

    /// Walk from the descriptor opened before launch. Files and symlinks
    /// become entries; special files are skipped.
    pub fn walk(root: &CaptureRoot) -> io::Result<(BTreeMap<String, Found>, Vec<String>)> {
        let mut files = BTreeMap::new();
        let mut skipped = Vec::new();
        let mut stack = vec![(Arc::new(Vec::new()), String::new())];
        while let Some((path, rel)) = stack.pop() {
            let opened_directory =
                open_directory_path(&root.directory, &path).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("read capture directory {rel:?}: {error}"),
                    )
                })?;
            let directory = opened_directory.as_ref().unwrap_or(&root.directory);
            let entries = names(directory).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("read capture directory {rel:?}: {error}"),
                )
            })?;
            for name in entries {
                let child_rel = if rel.is_empty() {
                    name.to_string_lossy().into_owned()
                } else {
                    format!("{rel}/{}", name.to_string_lossy())
                };
                let stat = stat_at(directory, &name).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("stat capture entry {child_rel:?}: {error}"),
                    )
                })?;
                match stat.st_mode & libc::S_IFMT {
                    libc::S_IFDIR => {
                        let mut child_path = path.as_ref().clone();
                        child_path.push(DirectoryComponent {
                            name,
                            identity: Identity::from(&stat),
                        });
                        stack.push((Arc::new(child_path), child_rel));
                    }
                    libc::S_IFREG | libc::S_IFLNK => {
                        let mode = if stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
                            120000
                        } else if stat.st_mode & 0o111 != 0 {
                            755
                        } else {
                            644
                        };
                        files.insert(
                            child_rel,
                            Found {
                                root: Arc::clone(&root.directory),
                                parent_path: Arc::clone(&path),
                                name,
                                identity: Identity::from(&stat),
                                mode,
                            },
                        );
                    }
                    _ => skipped.push(child_rel),
                }
            }
        }
        Ok((files, skipped))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn capture_open_does_not_follow_symlink_in_run_directory() {
            let run = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("outside"), b"outside bytes").unwrap();
            std::os::unix::fs::symlink(outside.path().join("outside"), run.path().join("link"))
                .unwrap();
            let root = CaptureRoot::open(run.path()).unwrap();
            assert!(open_at(&root.directory, OsStr::new("link"), false).is_err());
        }
    }
}

#[cfg(unix)]
pub use platform::{walk, CaptureRoot};

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn captured_bytes(found: &platform::Found) -> Vec<u8> {
        match found.read_content_bounded(1024).unwrap() {
            CaptureRead::Complete(content) => content.bytes,
            CaptureRead::TooLarge { .. } => panic!("small fixture exceeded capture cap"),
        }
    }

    #[test]
    fn walk_captures_many_directories_under_256_fd_limit() {
        const CHILD_MARKER: &str = "KHIVE_EXEC_CAPTURE_LOW_NOFILE_CHILD";
        if std::env::var_os(CHILD_MARKER).is_none() {
            let marker_dir = tempfile::tempdir().unwrap();
            let marker = marker_dir.path().join("completed");
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "capture::tests::walk_captures_many_directories_under_256_fd_limit",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD_MARKER, &marker)
                .output()
                .expect("spawn isolated low-fd test process");
            assert!(
                output.status.success(),
                "low-fd capture child failed: status={}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                std::fs::read(&marker).expect("the filtered child test must run"),
                b"completed"
            );
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        for i in 0..400 {
            let child = dir.path().join(format!("sub-{i:03}"));
            std::fs::create_dir(&child).unwrap();
            std::fs::write(child.join("output"), b"captured").unwrap();
        }

        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        assert!(
            limit.rlim_max >= 256,
            "test requires a hard fd limit >= 256"
        );
        limit.rlim_cur = 256;
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);

        let root = CaptureRoot::open(dir.path()).unwrap();
        let (files, skipped) = walk(&root).expect("capture must not retain one fd per directory");
        assert!(skipped.is_empty());
        assert_eq!(files.len(), 400);
        for i in 0..400 {
            assert_eq!(
                captured_bytes(&files[&format!("sub-{i:03}/output")]),
                b"captured"
            );
        }
        std::fs::write(
            std::path::PathBuf::from(std::env::var_os(CHILD_MARKER).unwrap()),
            b"completed",
        )
        .unwrap();
    }

    #[test]
    fn tail_keeps_last_bytes_and_counts_all() {
        let mut t = Tail::new(128);
        t.push(&[b'A'; 512]);
        t.push(b"OUT-END");
        assert_eq!(t.produced(), 519);
        let r = t.retained();
        assert_eq!(r.len(), 128);
        assert!(r.ends_with(b"OUT-END"));
        assert!(!t.complete());
        let mut small = Tail::new(128);
        small.push(b"ok\n");
        assert!(small.complete());
        assert_eq!(small.retained(), b"ok\n");
    }

    #[test]
    fn sparse_output_is_refused_from_metadata_before_a_whole_file_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sparse-output");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(1024 * 1024 * 1024).unwrap();
        let root = CaptureRoot::open(dir.path()).unwrap();
        let (files, _) = walk(&root).unwrap();
        assert!(matches!(
            files["sparse-output"].read_content_bounded(1024).unwrap(),
            CaptureRead::TooLarge { observed_at_least } if observed_at_least == 1024 * 1024 * 1024
        ));
    }

    #[test]
    fn actual_capture_bytes_are_bounded_even_if_metadata_underreports() {
        let bytes = vec![b'x'; 2048];
        let result = read_regular_bounded(std::io::Cursor::new(bytes), 0, 1024).unwrap();
        assert!(matches!(
            result,
            CaptureRead::TooLarge { observed_at_least } if observed_at_least > 1024
        ));
        let result = read_regular_bounded(std::io::Cursor::new(b"complete"), 0, 1024).unwrap();
        let CaptureRead::Complete(content) = result else {
            panic!("small content should be captured");
        };
        assert_eq!(content.bytes, b"complete");
        assert_eq!(
            content.digest,
            blake3::hash(b"complete").to_hex().to_string()
        );
    }

    #[test]
    fn walk_reports_missing_root_instead_of_returning_an_empty_tree() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing-run");
        let error = CaptureRoot::open(&missing).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn walk_reports_unreadable_descendant_instead_of_omitting_its_files() {
        use std::os::unix::fs::PermissionsExt;

        assert_ne!(unsafe { libc::geteuid() }, 0, "run as a non-root user");
        let dir = tempfile::tempdir().unwrap();
        let sealed = dir.path().join("sealed");
        std::fs::create_dir(&sealed).unwrap();
        std::fs::write(sealed.join("input"), b"still present").unwrap();
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o000)).unwrap();
        let root = CaptureRoot::open(dir.path()).unwrap();
        let result = walk(&root);
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("sealed"));
        assert_eq!(
            std::fs::read(sealed.join("input")).unwrap(),
            b"still present"
        );
    }

    #[cfg(unix)]
    #[test]
    fn walk_captures_file_directory_and_dangling_symlinks_without_following() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"1").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b"), b"2").unwrap();
        for (name, target) in [
            ("file-link", "a"),
            ("dir-link", "sub"),
            ("dangling-link", "missing"),
            ("escape-link", "/etc/passwd"),
        ] {
            std::os::unix::fs::symlink(target, dir.path().join(name)).unwrap();
        }
        let root = CaptureRoot::open(dir.path()).unwrap();
        let (files, skipped) = walk(&root).unwrap();
        assert_eq!(
            files.keys().cloned().collect::<Vec<_>>(),
            vec![
                "a",
                "dangling-link",
                "dir-link",
                "escape-link",
                "file-link",
                "sub/b"
            ]
        );
        assert!(skipped.is_empty());
        assert_eq!(captured_bytes(&files["a"]), b"1");
        assert_eq!(captured_bytes(&files["sub/b"]), b"2");
        for (name, target) in [
            ("file-link", "a"),
            ("dir-link", "sub"),
            ("dangling-link", "missing"),
            ("escape-link", "/etc/passwd"),
        ] {
            assert_eq!(files[name].mode, 120000);
            assert_eq!(captured_bytes(&files[name]), target.as_bytes());
        }
    }

    #[cfg(unix)]
    #[test]
    fn walk_preserves_non_utf8_and_unnormalized_symlink_target_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let dir = tempfile::tempdir().unwrap();
        let target = b"../missing//\xff/./target\n";
        std::os::unix::fs::symlink(
            std::ffi::OsString::from_vec(target.to_vec()),
            dir.path().join("link"),
        )
        .unwrap();
        let root = CaptureRoot::open(dir.path()).unwrap();
        let (files, skipped) = walk(&root).unwrap();
        assert!(skipped.is_empty());
        assert_eq!(files["link"].mode, 120000);
        assert_eq!(captured_bytes(&files["link"]), target);
    }

    #[cfg(unix)]
    #[test]
    fn walk_still_skips_sockets_and_fifos() {
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(dir.path().join("socket")).unwrap();
        let fifo = std::ffi::CString::new(dir.path().join("fifo").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let root = CaptureRoot::open(dir.path()).unwrap();
        let (files, mut skipped) = walk(&root).unwrap();
        assert!(files.is_empty());
        skipped.sort();
        assert_eq!(skipped, vec!["fifo", "socket"]);
    }

    #[test]
    fn capture_refuses_symlink_in_run_directory() {
        let run = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside");
        std::fs::write(&outside_file, b"outside bytes").unwrap();
        let path = run.path().join("output");
        std::fs::write(&path, b"inside bytes").unwrap();
        let root = CaptureRoot::open(run.path()).unwrap();
        let (files, skipped) = walk(&root).unwrap();
        assert!(skipped.is_empty());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&outside_file, &path).unwrap();

        let error = files["output"].read_content_bounded(1024).unwrap_err();
        assert!(
            error.to_string().contains("changed after inspection"),
            "{error}"
        );
    }

    #[test]
    fn capture_walk_uses_opened_directory() {
        let container = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let run = container.path().join("run");
        std::fs::create_dir(&run).unwrap();
        std::fs::write(run.join("output"), b"inside bytes").unwrap();
        std::fs::write(outside.path().join("output"), b"outside bytes").unwrap();
        let root = CaptureRoot::open(&run).unwrap();
        std::fs::rename(&run, container.path().join("original")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &run).unwrap();

        let (files, skipped) = walk(&root).unwrap();
        assert!(skipped.is_empty());
        assert_eq!(captured_bytes(&files["output"]), b"inside bytes");
    }

    #[test]
    fn reopened_parent_refuses_replaced_directory() {
        let run = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let parent = run.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(parent.join("output"), b"inside bytes").unwrap();
        std::fs::write(outside.path().join("output"), b"outside bytes").unwrap();

        let root = CaptureRoot::open(run.path()).unwrap();
        let (files, skipped) = walk(&root).unwrap();
        assert!(skipped.is_empty());
        std::fs::rename(&parent, run.path().join("original")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &parent).unwrap();

        let error = files["parent/output"]
            .read_content_bounded(1024)
            .unwrap_err();
        assert!(
            error.to_string().contains("changed after inspection"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn deadline_closes_a_held_stream() {
        use tokio::io::AsyncWriteExt;

        let (mut writer, reader) = tokio::io::duplex(64);
        writer.write_all(b"before").await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(20);
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            drain_until(reader, 128, deadline),
        )
        .await
        .expect("output drain exceeded its outer deadline");
        assert_eq!(result.stop, DrainStop::Deadline);
        assert_eq!(result.tail.retained(), b"before");
        assert!(writer.write_all(b"after").await.is_err());
    }
}
