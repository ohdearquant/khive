//! Public final-component opens and descriptor-relative link reads.

#![cfg(unix)]

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use khive_fs::directory_walk::{open_dir_nofollow, read_link_at};
use khive_fs::opened_file::{open_regular_file_nofollow, ContainedOpenError};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        for _ in 0..1000 {
            let path = std::env::temp_dir().join(format!(
                "khive-fs-path-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create private scratch: {error}"),
            }
        }
        panic!("could not allocate a private scratch directory");
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn close_on_exec(file: &File) {
    // SAFETY: file owns a live descriptor; F_GETFD reads its descriptor flags.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
    assert!(flags >= 0);
    assert_ne!(flags & libc::FD_CLOEXEC, 0);
}

#[test]
fn read_link_preserves_bytes_grows_and_uses_the_held_parent() {
    let scratch = Scratch::new();
    let original = scratch.path().join("parent");
    let renamed = scratch.path().join("renamed");
    std::fs::create_dir(&original).unwrap();
    let parent = File::open(&original).unwrap();
    let mut bytes = b"../".repeat(64);
    bytes.extend_from_slice(&[0xff, b'x']);
    let target = OsString::from_vec(bytes.clone());
    symlink(&target, original.join("link")).unwrap();
    std::fs::rename(&original, &renamed).unwrap();
    std::fs::create_dir(&original).unwrap();
    symlink("wrong", original.join("link")).unwrap();
    let actual = read_link_at(&parent, OsStr::new("link")).unwrap();
    assert_eq!(actual.as_os_str().as_bytes(), bytes);
    std::fs::write(renamed.join("regular"), b"data").unwrap();
    assert_eq!(
        read_link_at(&parent, OsStr::new("regular"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EINVAL)
    );
    for name in [
        OsStr::new(""),
        OsStr::new("a/b"),
        OsStr::new("/link"),
        OsStr::from_bytes(b"a\0b"),
    ] {
        assert_eq!(
            read_link_at(&parent, name).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

#[test]
fn regular_open_refuses_final_links_and_non_regular_files() {
    let scratch = Scratch::new();
    let real = scratch.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("data"), b"unchanged").unwrap();
    symlink("real", scratch.path().join("ancestor")).unwrap();
    symlink("data", real.join("alias")).unwrap();
    for path in [real.join("data"), scratch.path().join("ancestor/data")] {
        let mut file = open_regular_file_nofollow(&path).unwrap();
        close_on_exec(&file);
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"unchanged");
    }
    assert!(matches!(
        open_regular_file_nofollow(&real.join("alias")),
        Err(ContainedOpenError::Open(_))
    ));
    assert!(matches!(
        open_regular_file_nofollow(&real),
        Err(ContainedOpenError::NotRegular)
    ));
    match open_regular_file_nofollow(&real.join("missing")) {
        Err(ContainedOpenError::Open(error)) => assert_eq!(error.kind(), io::ErrorKind::NotFound),
        other => panic!("expected raw missing-file open error: {other:?}"),
    }
    let nul = PathBuf::from(OsString::from_vec(b"bad\0path".to_vec()));
    match open_regular_file_nofollow(&nul) {
        Err(ContainedOpenError::Open(error)) => {
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput)
        }
        other => panic!("expected raw NUL-path open error: {other:?}"),
    }
    assert_eq!(std::fs::read(real.join("data")).unwrap(), b"unchanged");
}

#[test]
fn directory_open_owns_the_inode_and_preserves_ancestor_resolution() {
    let scratch = Scratch::new();
    let real = scratch.path().join("real");
    std::fs::create_dir_all(real.join("child")).unwrap();
    symlink("real", scratch.path().join("alias")).unwrap();
    std::fs::write(scratch.path().join("regular"), b"data").unwrap();
    assert!(open_dir_nofollow(&scratch.path().join("alias")).is_err());
    assert!(open_dir_nofollow(&scratch.path().join("regular")).is_err());
    let file = File::from(open_dir_nofollow(&scratch.path().join("alias/child")).unwrap());
    close_on_exec(&file);
    let expected = std::fs::metadata(real.join("child")).unwrap();
    std::fs::rename(&real, scratch.path().join("renamed")).unwrap();
    std::fs::create_dir_all(real.join("child")).unwrap();
    let held = file.metadata().unwrap();
    assert!(held.is_dir());
    assert_eq!((held.dev(), held.ino()), (expected.dev(), expected.ino()));
}

struct ReapChild(Child);

impl Drop for ReapChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const FIFO_CHILD_PATH: &str = "KHIVE_FS_PATH_NOFOLLOW_FIFO_CHILD";
const FIFO_DONE: &str = "khive-fs: fifo regular and directory opens refused";

#[test]
#[ignore = "invoked by fifo_refusal_finishes_without_a_writer in a bounded child"]
fn fifo_child_case() {
    let Some(path) = std::env::var_os(FIFO_CHILD_PATH) else {
        return;
    };
    let path = PathBuf::from(path);
    assert!(matches!(
        open_regular_file_nofollow(&path),
        Err(ContainedOpenError::NotRegular)
    ));
    assert!(open_dir_nofollow(&path).is_err());
    println!("{FIFO_DONE}");
}

#[test]
fn fifo_refusal_finishes_without_a_writer() {
    let scratch = Scratch::new();
    let fifo = scratch.path().join("fifo");
    let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is NUL-terminated and lives for the duration of mkfifo.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let mut child = ReapChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fifo_child_case",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(FIFO_CHILD_PATH, &fifo)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "FIFO open waited for a writer");
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(status.success(), "child failed: {stdout}\n{stderr}");
    assert!(
        stdout.contains(FIFO_DONE),
        "child case did not run: {stdout}\n{stderr}"
    );
}
