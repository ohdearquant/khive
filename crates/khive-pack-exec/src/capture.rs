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
    use khive_fs::directory_walk::{read_link_at_bounded, BoundedLinkRead};
    #[cfg(test)]
    use khive_fs::fd_relative::c_name;
    use khive_fs::fd_relative::{list_names, open_at, stat_at, stat_fd};
    use khive_fs::opened_file::opened_file_path;
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};
    use std::fs::{File, OpenOptions};
    use std::io;
    #[cfg(test)]
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
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
        match read_link_at_bounded(parent, name, max_bytes)? {
            BoundedLinkRead::Complete(bytes) => Ok(CaptureRead::Complete(CapturedContent {
                digest: blake3::hash(&bytes).to_hex().to_string(),
                bytes,
            })),
            BoundedLinkRead::TooLarge { observed_at_least } => {
                Ok(CaptureRead::TooLarge { observed_at_least })
            }
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
                let Some(name_str) = name.to_str() else {
                    let mut escaped = String::from("non-UTF-8 path: ");
                    let mut append = |byte: u8| {
                        const HEX: &[u8; 16] = b"0123456789abcdef";
                        escaped.push_str("\\x");
                        escaped.push(char::from(HEX[usize::from(byte >> 4)]));
                        escaped.push(char::from(HEX[usize::from(byte & 0x0f)]));
                    };
                    if !rel.is_empty() {
                        for byte in rel.bytes().chain(std::iter::once(b'/')) {
                            append(byte);
                        }
                    }
                    for &byte in name.as_bytes() {
                        append(byte);
                    }
                    // Skipping a directory skips its subtree as well: its name
                    // cannot be represented by the capture's string paths.
                    skipped.push(escaped);
                    continue;
                };
                let child_rel = if rel.is_empty() {
                    name_str.to_string()
                } else {
                    format!("{rel}/{name_str}")
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
    #[path = "tests.rs"]
    mod tests;
}

#[cfg(unix)]
pub use platform::{walk, CaptureRoot};

#[cfg(all(test, unix))]
#[path = "capture/tests.rs"]
mod tests;

#[cfg(all(test, unix))]
mod non_utf8_capture_tests {
    use super::platform::Found;
    use super::{walk, CaptureRead, CaptureRoot};
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "khive-capture-non-utf8-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).expect("create temporary capture directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn invalid_name(bytes: &[u8]) -> OsString {
        OsString::from_vec(bytes.to_vec())
    }

    fn assert_content(found: &Found, expected: &[u8]) {
        match found.read_content_bounded(64).unwrap() {
            CaptureRead::Complete(content) => assert_eq!(content.bytes, expected),
            CaptureRead::TooLarge { .. } => panic!("small file was refused"),
        }
    }

    #[test]
    fn distinct_invalid_names_are_reported_without_collision() {
        let tmp = TempDir::new();
        std::fs::write(tmp.path().join(invalid_name(b"a\x80")), b"first").unwrap();
        std::fs::write(tmp.path().join(invalid_name(b"a\x81")), b"second").unwrap();
        std::fs::write(tmp.path().join("valid"), b"kept").unwrap();
        let root = CaptureRoot::open(tmp.path()).unwrap();
        let (files, skipped) = walk(&root).unwrap();
        assert_eq!(files.len(), 1);
        assert_content(files.get("valid").unwrap(), b"kept");
        assert_eq!(skipped.len(), 2);
        assert!(skipped.contains(&"non-UTF-8 path: \\x61\\x80".to_string()));
        assert!(skipped.contains(&"non-UTF-8 path: \\x61\\x81".to_string()));
    }

    #[test]
    fn invalid_name_does_not_hide_literal_replacement_character() {
        let tmp = TempDir::new();
        std::fs::write(tmp.path().join(invalid_name(b"a\x80")), b"invalid").unwrap();
        std::fs::write(tmp.path().join("a\u{fffd}"), b"valid").unwrap();
        let root = CaptureRoot::open(tmp.path()).unwrap();
        let (files, skipped) = walk(&root).unwrap();
        assert_eq!(files.len(), 1);
        assert_content(files.get("a\u{fffd}").unwrap(), b"valid");
        assert_eq!(skipped, vec!["non-UTF-8 path: \\x61\\x80".to_string()]);
    }

    #[test]
    fn invalid_directory_is_reported_without_walking_its_subtree() {
        let tmp = TempDir::new();
        let dir = tmp.path().join("parent");
        std::fs::create_dir(&dir).unwrap();
        let invalid = dir.join(invalid_name(b"bad\x80"));
        std::fs::create_dir(&invalid).unwrap();
        std::fs::write(invalid.join("inside"), b"not visited").unwrap();
        std::fs::write(dir.join("valid"), b"kept").unwrap();
        let root = CaptureRoot::open(tmp.path()).unwrap();
        let (files, skipped) = walk(&root).unwrap();
        assert_eq!(files.len(), 1);
        assert_content(files.get("parent/valid").unwrap(), b"kept");
        assert_eq!(
            skipped,
            vec![
                "non-UTF-8 path: \\x70\\x61\\x72\\x65\\x6e\\x74\\x2f\\x62\\x61\\x64\\x80"
                    .to_string()
            ]
        );
    }

    #[test]
    fn valid_replacement_character_path_is_preserved() {
        let tmp = TempDir::new();
        std::fs::write(tmp.path().join("a\u{fffd}"), b"valid").unwrap();
        std::fs::write(tmp.path().join(r"literal\x61"), b"literal").unwrap();
        let root = CaptureRoot::open(tmp.path()).unwrap();
        let (files, skipped) = walk(&root).unwrap();
        assert_eq!(files.len(), 2);
        assert_content(files.get("a\u{fffd}").unwrap(), b"valid");
        assert_content(files.get(r"literal\x61").unwrap(), b"literal");
        assert!(skipped.is_empty());
    }
}
