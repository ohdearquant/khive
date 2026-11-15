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

fn readonly_descriptor_flags(file: &File) {
    close_on_exec(file);
    // SAFETY: file owns a live descriptor; F_GETFL only reads its status flags.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(flags & libc::O_ACCMODE, libc::O_RDONLY);
    assert_ne!(flags & libc::O_NONBLOCK, 0);
}

#[test]
fn readonly_open_preserves_regular_bytes_and_accepts_directories() {
    use khive_fs::opened_file::open_readonly_file_nofollow;

    let scratch = Scratch::new();
    let real = scratch.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("data"), b"readonly unchanged").unwrap();
    symlink("real", scratch.path().join("ancestor")).unwrap();
    for path in [real.join("data"), scratch.path().join("ancestor/data")] {
        let mut file = open_readonly_file_nofollow(&path).unwrap();
        readonly_descriptor_flags(&file);
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"readonly unchanged");
    }
    let directory = open_readonly_file_nofollow(&real).unwrap();
    readonly_descriptor_flags(&directory);
    assert!(directory.metadata().unwrap().is_dir());
    assert_eq!(
        std::fs::read(real.join("data")).unwrap(),
        b"readonly unchanged"
    );
}

#[test]
fn readonly_open_preserves_raw_errors_and_refuses_final_symlinks() {
    use khive_fs::opened_file::open_readonly_file_nofollow;

    let scratch = Scratch::new();
    let data = scratch.path().join("data");
    std::fs::write(&data, b"unchanged").unwrap();
    symlink("data", scratch.path().join("alias")).unwrap();
    assert_eq!(
        open_readonly_file_nofollow(&scratch.path().join("alias"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ELOOP)
    );
    let missing = open_readonly_file_nofollow(&scratch.path().join("missing")).unwrap_err();
    assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    assert_eq!(missing.raw_os_error(), Some(libc::ENOENT));
    let nul = PathBuf::from(OsString::from_vec(b"bad\0path".to_vec()));
    assert_eq!(
        open_readonly_file_nofollow(&nul).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(std::fs::read(&data).unwrap(), b"unchanged");
}

struct BoundedReadonlyChild {
    child: Child,
    cleanup_attempted: bool,
}

impl BoundedReadonlyChild {
    fn stop_and_reap(&mut self) -> bool {
        self.cleanup_attempted = true;
        let _ = self.child.kill();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => return false,
            }
        }
    }

    fn wait_bounded(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().expect("poll readonly FIFO child") {
                self.cleanup_attempted = true;
                return status;
            }
            if Instant::now() >= deadline {
                assert!(
                    self.stop_and_reap(),
                    "readonly FIFO child exceeded the reap deadline"
                );
                panic!("readonly FIFO open waited for a writer");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for BoundedReadonlyChild {
    fn drop(&mut self) {
        if !self.cleanup_attempted {
            let _ = self.stop_and_reap();
        }
    }
}

const READONLY_FIFO_PATH: &str = "KHIVE_FS_READONLY_NOFOLLOW_FIFO_PATH";
const READONLY_FIFO_MARKER: &str = "KHIVE_FS_READONLY_NOFOLLOW_FIFO_MARKER";

#[test]
#[ignore = "invoked only by readonly_fifo_open_finishes_without_a_writer in an owned child"]
fn readonly_fifo_child_case() {
    use khive_fs::opened_file::open_readonly_file_nofollow;
    use std::os::unix::fs::FileTypeExt as _;

    let path = PathBuf::from(std::env::var_os(READONLY_FIFO_PATH).expect("readonly FIFO path"));
    let marker =
        PathBuf::from(std::env::var_os(READONLY_FIFO_MARKER).expect("readonly FIFO marker"));
    assert!(std::fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_fifo());
    let file = open_readonly_file_nofollow(&path).expect("raw readonly FIFO open");
    readonly_descriptor_flags(&file);
    assert!(file.metadata().unwrap().file_type().is_fifo());
    std::fs::write(marker, b"readonly FIFO descriptor checked").unwrap();
}

#[test]
fn readonly_fifo_open_finishes_without_a_writer() {
    let scratch = Scratch::new();
    let fifo = scratch.path().join("readonly-fifo");
    let marker = scratch.path().join("readonly-done");
    let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is NUL-terminated and remains alive for mkfifo.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let mut child = BoundedReadonlyChild {
        child: Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "readonly_fifo_child_case",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(READONLY_FIFO_PATH, &fifo)
            .env(READONLY_FIFO_MARKER, &marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn readonly FIFO child"),
        cleanup_attempted: false,
    };
    assert!(child.wait_bounded().success(), "readonly FIFO child failed");
    assert_eq!(
        std::fs::read(marker).unwrap(),
        b"readonly FIFO descriptor checked"
    );
}
