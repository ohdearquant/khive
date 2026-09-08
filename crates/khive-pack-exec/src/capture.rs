//! Output capture (tail-preserving, byte-counting) and run-directory walks.

use std::collections::BTreeMap;
use std::collections::VecDeque;
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

/// A regular file found in the run directory after the run.
#[derive(Debug, Clone)]
pub struct Found {
    pub abs: PathBuf,
    pub mode: u32,
}

/// Walk `root` without following symlinks. Symlinks are reported by relative
/// path in `skipped` and never opened; directories are descended; anything
/// else (sockets, fifos, devices) is skipped as well.
pub fn walk(root: &Path) -> std::io::Result<(BTreeMap<String, Found>, Vec<String>)> {
    let mut files = BTreeMap::new();
    let mut skipped = Vec::new();
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let abs = entry.path();
            let meta = match std::fs::symlink_metadata(&abs) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let ft = meta.file_type();
            if ft.is_symlink() {
                skipped.push(child_rel);
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
            use std::os::unix::fs::PermissionsExt;
            let mode = if meta.permissions().mode() & 0o111 != 0 {
                755
            } else {
                644
            };
            files.insert(child_rel, Found { abs, mode });
        }
    }
    Ok((files, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn walk_skips_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"1").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b"), b"2").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("escape")).unwrap();
        let (files, skipped) = walk(dir.path()).unwrap();
        assert_eq!(
            files.keys().cloned().collect::<Vec<_>>(),
            vec!["a", "sub/b"]
        );
        assert_eq!(skipped, vec!["escape"]);
    }
}
