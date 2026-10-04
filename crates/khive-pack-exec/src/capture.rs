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
    use khive_fs::fd_relative::{c_name, list_names, open_at, stat_at, stat_fd};
    use khive_fs::opened_file::opened_file_path;
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;
    use std::sync::Arc;

    /// Longest relative path the walk accepts, in bytes of its lossy UTF-8
    /// form. Each directory is re-resolved from the root one component at a
    /// time, so this cap also bounds the per-directory reopen cost; a deeper
    /// tree is a capture error rather than an unbounded walk.
    const MAX_CAPTURE_PATH_BYTES: usize = 1024;

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

        /// Asks the kernel for the pinned run directory's current path and
        /// checks that the path still names it. A run directory the tool
        /// removed, or removed and recreated at the same path, lists as empty
        /// through the pinned descriptor and cannot be told from a run that
        /// wrote nothing, and `st_nlink` does not drop to zero for it on macOS.
        /// A renamed run directory is still the pinned one, so it passes, also
        /// when a new directory is then created at the old path: capture lists
        /// the pinned tree, not the new directory. Returns `Ok(None)` when the
        /// root is still named. Otherwise `Ok(Some(detail))` names the detector
        /// and starts with `root_missing` when the path is gone or names
        /// another file, or `root_unverified` when the path could not be
        /// queried or read (on macOS, `F_GETPATH` fails for a directory moved
        /// to a path longer than the platform limit). `Err` with
        /// [`io::ErrorKind::Unsupported`] means this platform has no
        /// descriptor-to-path query; any other `Err` means the pinned
        /// descriptor itself could not be read.
        pub fn missing_root(&self) -> io::Result<Option<String>> {
            use std::os::unix::fs::MetadataExt as _;

            let path = match opened_file_path(&self.directory).map_err(capture_path_error) {
                Ok(path) => path,
                Err(error) if error.kind() == io::ErrorKind::Unsupported => return Err(error),
                Err(error) => {
                    return Ok(Some(format!(
                    "root_unverified: {DESCRIPTOR_PATH_QUERY} on the run directory failed: {error}"
                )))
                }
            };
            let pinned = self.directory.metadata()?;
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    if metadata.dev() == pinned.dev() && metadata.ino() == pinned.ino() {
                        Ok(None)
                    } else {
                        Ok(Some(format!(
                            "root_missing: {DESCRIPTOR_PATH_QUERY} path of the run directory now names a different file"
                        )))
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Some(format!(
                    "root_missing: {DESCRIPTOR_PATH_QUERY} path of the run directory no longer exists"
                ))),
                Err(error) => Ok(Some(format!(
                    "root_unverified: lstat of the {DESCRIPTOR_PATH_QUERY} path of the run directory failed: {error}"
                ))),
            }
        }
    }

    #[cfg(target_vendor = "apple")]
    const DESCRIPTOR_PATH_QUERY: &str = "F_GETPATH";
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const DESCRIPTOR_PATH_QUERY: &str = "/proc/self/fd";
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    const DESCRIPTOR_PATH_QUERY: &str = "descriptor path query";

    fn capture_path_error(error: io::Error) -> io::Error {
        if error.raw_os_error().is_some() {
            return error;
        }
        match error.kind() {
            io::ErrorKind::Unsupported => io::Error::new(
                io::ErrorKind::Unsupported,
                "no descriptor-to-path query on this platform",
            ),
            #[cfg(target_vendor = "apple")]
            io::ErrorKind::InvalidData => io::Error::new(
                io::ErrorKind::InvalidData,
                "F_GETPATH result is not terminated",
            ),
            _ => error,
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

    /// The shared descriptor helpers refuse a NUL byte in a name with their own wording; capture
    /// reports it as a capture name. Any other error passes through unchanged.
    fn capture_name_error(error: io::Error) -> io::Error {
        if error.kind() == io::ErrorKind::InvalidInput && error.raw_os_error().is_none() {
            io::Error::new(io::ErrorKind::InvalidInput, "NUL in capture name")
        } else {
            error
        }
    }

    fn changed() -> io::Error {
        io::Error::other("capture entry changed after inspection")
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
                return Err(capture_name_error(error));
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
                let checked = stat_at(parent, &self.name).map_err(capture_name_error)?;
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
            let entries = list_names(directory).map_err(|error| {
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
                if child_rel.len() > MAX_CAPTURE_PATH_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "capture path exceeds {MAX_CAPTURE_PATH_BYTES} bytes under {:?}",
                            child_rel.chars().take(64).collect::<String>()
                        ),
                    ));
                }
                let stat = stat_at(directory, &name).map_err(|error| {
                    let error = capture_name_error(error);
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
        #[cfg(target_vendor = "apple")]
        use std::os::fd::FromRawFd;

        #[test]
        fn capture_path_error_keeps_the_unsupported_platform_message() {
            let error = capture_path_error(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure opened-file path resolution is unsupported on this Unix target",
            ));
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert_eq!(
                error.to_string(),
                "no descriptor-to-path query on this platform"
            );

            for errno in [libc::EBADF, libc::ENOTSUP, libc::EILSEQ] {
                let os_error = capture_path_error(io::Error::from_raw_os_error(errno));
                assert_eq!(os_error.raw_os_error(), Some(errno));
            }
        }

        #[cfg(target_vendor = "apple")]
        #[test]
        fn capture_path_error_keeps_the_unterminated_apple_query_message() {
            let error = capture_path_error(io::Error::new(
                io::ErrorKind::InvalidData,
                "F_GETPATH returned no NUL terminator",
            ));
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(error.to_string(), "F_GETPATH result is not terminated");
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn shared_opened_file_path_keeps_the_removed_directory_suffix() {
            let parent = tempfile::tempdir().unwrap();
            let removed = parent.path().canonicalize().unwrap().join("removed");
            std::fs::create_dir(&removed).unwrap();
            let root = CaptureRoot::open(&removed).unwrap();
            std::fs::remove_dir(&removed).unwrap();

            let mut expected = removed.into_os_string();
            expected.push(" (deleted)");
            assert_eq!(
                opened_file_path(&root.directory).unwrap(),
                std::path::PathBuf::from(expected)
            );
            assert_eq!(
                root.missing_root().unwrap().as_deref(),
                Some("root_missing: /proc/self/fd path of the run directory no longer exists")
            );
        }

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

        #[test]
        fn a_nul_byte_in_a_capture_name_keeps_the_capture_error_text() {
            use std::os::unix::ffi::OsStrExt;

            let run = tempfile::tempdir().unwrap();
            let root = CaptureRoot::open(run.path()).unwrap();
            let name = OsStr::from_bytes(b"a\0b");
            let found = |kind: libc::mode_t, mode: u32| Found {
                root: Arc::clone(&root.directory),
                parent_path: Arc::new(Vec::new()),
                name: name.to_os_string(),
                identity: Identity {
                    dev: 0,
                    ino: 0,
                    kind,
                },
                mode,
            };
            let regular = found(libc::S_IFREG, 644);
            let symlink = found(libc::S_IFLNK, 120000);
            for entry in [regular, symlink] {
                let mode = entry.mode;
                let error = entry.read_content_bounded(16).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "mode {mode}");
                assert_eq!(error.to_string(), "NUL in capture name", "mode {mode}");
            }
        }

        /// Create `levels` nested directories named `name` below `root` by
        /// descriptor, so the chain may be longer than PATH_MAX.
        fn mkdir_chain(root: &File, name: &OsStr, levels: usize) {
            let mut parent = root.try_clone().unwrap();
            for _ in 0..levels {
                let c = c_name(name).unwrap();
                let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), c.as_ptr(), 0o700) };
                assert_eq!(rc, 0, "mkdirat: {}", io::Error::last_os_error());
                parent = open_at(&parent, name, true).unwrap();
            }
        }

        #[test]
        fn walk_refuses_a_path_past_the_capture_byte_cap() {
            let name = OsString::from("d".repeat(200));

            // Five levels: 5 * 200 bytes plus four separators = 1004 bytes.
            let within = tempfile::tempdir().unwrap();
            let root = CaptureRoot::open(within.path()).unwrap();
            mkdir_chain(&root.directory, &name, 5);
            let (files, skipped) = walk(&root).expect("a path within the cap is captured");
            assert!(files.is_empty());
            assert!(skipped.is_empty());

            // Six levels: 1205 bytes, past the cap.
            let past = tempfile::tempdir().unwrap();
            let root = CaptureRoot::open(past.path()).unwrap();
            mkdir_chain(&root.directory, &name, 6);
            let error = walk(&root).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(
                error.to_string().contains(&format!(
                    "capture path exceeds {MAX_CAPTURE_PATH_BYTES} bytes"
                )),
                "{error}"
            );
        }

        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
        #[test]
        fn missing_root_tells_a_removed_or_replaced_run_directory_from_a_renamed_one() {
            let parent = tempfile::tempdir().unwrap();

            let kept = parent.path().join("kept");
            std::fs::create_dir(&kept).unwrap();
            let root = CaptureRoot::open(&kept).unwrap();
            assert_eq!(root.missing_root().unwrap(), None);

            // Rename-away keeps the pinned directory, so capture stays complete.
            let renamed = parent.path().join("renamed");
            std::fs::create_dir(&renamed).unwrap();
            let root = CaptureRoot::open(&renamed).unwrap();
            std::fs::rename(&renamed, parent.path().join("moved")).unwrap();
            assert_eq!(root.missing_root().unwrap(), None);

            // A new directory at the old path does not change that: the pinned
            // tree is still named at its new path.
            std::fs::create_dir(&renamed).unwrap();
            assert_eq!(root.missing_root().unwrap(), None);

            let removed = parent.path().join("removed");
            std::fs::create_dir(&removed).unwrap();
            std::fs::write(removed.join("output"), b"x").unwrap();
            let root = CaptureRoot::open(&removed).unwrap();
            std::fs::remove_dir_all(&removed).unwrap();
            let detail = root
                .missing_root()
                .unwrap()
                .expect("removed root is missing");
            assert!(detail.starts_with("root_missing: "), "{detail}");
            assert!(detail.contains(DESCRIPTOR_PATH_QUERY), "{detail}");
            let (files, _) = walk(&root).expect("the pinned directory still lists");
            assert!(files.is_empty(), "a removed root lists as empty");

            let replaced = parent.path().join("replaced");
            std::fs::create_dir(&replaced).unwrap();
            let root = CaptureRoot::open(&replaced).unwrap();
            std::fs::remove_dir(&replaced).unwrap();
            std::fs::create_dir(&replaced).unwrap();
            let detail = root
                .missing_root()
                .unwrap()
                .expect("a new directory at the same path is not the pinned one");
            assert!(detail.starts_with("root_missing: "), "{detail}");
            assert!(
                detail.contains("different file") || detail.contains("no longer exists"),
                "{detail}"
            );
        }

        #[cfg(target_vendor = "apple")]
        #[test]
        fn missing_root_reports_an_unqueryable_path_as_unverified() {
            let parent = tempfile::tempdir().unwrap();
            let run = parent.path().join("run");
            std::fs::create_dir(&run).unwrap();
            let root = CaptureRoot::open(&run).unwrap();

            // Move the run directory under a chain whose path is longer than
            // the platform limit; each step is relative, so no call passes it.
            let name = "d".repeat(200);
            let mut deep = std::fs::File::open(parent.path()).unwrap();
            for _ in 0..7 {
                let component = std::ffi::CString::new(name.as_str()).unwrap();
                // SAFETY: `deep` is an open directory and `component` is a NUL-terminated name.
                let made = unsafe { libc::mkdirat(deep.as_raw_fd(), component.as_ptr(), 0o700) };
                assert_eq!(made, 0, "{}", io::Error::last_os_error());
                // SAFETY: as above; the returned descriptor is owned by the new `File`.
                let fd = unsafe {
                    libc::openat(
                        deep.as_raw_fd(),
                        component.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                    )
                };
                assert!(fd >= 0, "{}", io::Error::last_os_error());
                // SAFETY: `fd` was just returned by `openat` and is not owned elsewhere.
                deep = unsafe { std::fs::File::from_raw_fd(fd) };
            }
            let parent_dir = std::fs::File::open(parent.path()).unwrap();
            let from = std::ffi::CString::new("run").unwrap();
            // SAFETY: both descriptors are open directories and both names are NUL-terminated.
            let moved = unsafe {
                libc::renameat(
                    parent_dir.as_raw_fd(),
                    from.as_ptr(),
                    deep.as_raw_fd(),
                    from.as_ptr(),
                )
            };
            assert_eq!(moved, 0, "{}", io::Error::last_os_error());

            let detail = root
                .missing_root()
                .unwrap()
                .expect("a path the kernel cannot report is not confirmed");
            assert!(detail.starts_with("root_unverified: "), "{detail}");
            let (files, _) = walk(&root).expect("the pinned directory still lists");
            assert!(files.is_empty());
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
