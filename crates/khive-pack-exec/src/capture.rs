//! Output capture (tail-preserving, byte-counting) and run-directory walks.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};

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

/// Drain `reader` into a tail buffer until EOF.
pub async fn drain<R: AsyncRead + Unpin>(mut reader: R, cap: u64) -> Tail {
    let mut tail = Tail::new(cap);
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => tail.push(&chunk[..n]),
            Err(_) => break,
        }
    }
    tail
}

/// A regular file or symlink found in the run directory after the run.
#[derive(Debug, Clone)]
pub struct Found {
    pub abs: PathBuf,
    pub mode: u32,
}

/// One bounded file capture, with its content hash accumulated while reading.
pub struct CapturedContent {
    pub bytes: Vec<u8>,
    pub digest: String,
}

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

impl Found {
    pub fn read_content_bounded(&self, max_bytes: u64) -> std::io::Result<CaptureRead> {
        if self.mode == 120000 {
            let bytes = std::fs::read_link(&self.abs)?
                .into_os_string()
                .into_encoded_bytes();
            if bytes.len() as u64 > max_bytes {
                return Ok(CaptureRead::TooLarge {
                    observed_at_least: bytes.len() as u64,
                });
            }
            Ok(CaptureRead::Complete(CapturedContent {
                digest: blake3::hash(&bytes).to_hex().to_string(),
                bytes,
            }))
        } else {
            let file = std::fs::File::open(&self.abs)?;
            let advertised_len = file.metadata()?.len();
            read_regular_bounded(file, advertised_len, max_bytes)
        }
    }
}

/// Walk `root` without following symlinks. Files and symlinks become entries;
/// directories are descended; sockets, fifos and devices are reported in
/// `skipped`. An unreadable entry is an error, never evidence that an input
/// path was deleted.
pub fn walk(root: &Path) -> std::io::Result<(BTreeMap<String, Found>, Vec<String>)> {
    let mut files = BTreeMap::new();
    let mut skipped = Vec::new();
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("read capture directory {}: {error}", dir.display()),
            )
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!("enumerate capture directory {}: {error}", dir.display()),
                )
            })?;
            let name = entry.file_name().to_string_lossy().to_string();
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let abs = entry.path();
            let meta = std::fs::symlink_metadata(&abs).map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!("stat capture entry {}: {error}", abs.display()),
                )
            })?;
            let ft = meta.file_type();
            if ft.is_symlink() {
                files.insert(child_rel, Found { abs, mode: 120000 });
                continue;
            }
            if ft.is_dir() {
                stack.push((abs, child_rel));
                continue;
            }
            if !ft.is_file() {
                skipped.push(child_rel);
                continue;
            }
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o111 != 0 {
                    755
                } else {
                    644
                }
            };
            #[cfg(not(unix))]
            let mode = 644;
            files.insert(child_rel, Found { abs, mode });
        }
    }
    Ok((files, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captured_bytes(found: &Found) -> Vec<u8> {
        match found.read_content_bounded(1024).unwrap() {
            CaptureRead::Complete(content) => content.bytes,
            CaptureRead::TooLarge { .. } => panic!("small fixture exceeded capture cap"),
        }
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
        let found = Found {
            abs: path,
            mode: 644,
        };
        assert!(matches!(
            found.read_content_bounded(1024).unwrap(),
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
        let error = walk(&missing).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(error.to_string().contains("missing-run"));
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
        let result = walk(dir.path());
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
        let (files, skipped) = walk(dir.path()).unwrap();
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
        let (files, skipped) = walk(dir.path()).unwrap();
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
        let (files, mut skipped) = walk(dir.path()).unwrap();
        assert!(files.is_empty());
        skipped.sort();
        assert_eq!(skipped, vec!["fifo", "socket"]);
    }
}
