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
    std::os::unix::fs::symlink(outside.path().join("outside"), run.path().join("link")).unwrap();
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

#[test]
fn bounded_symlink_capture_keeps_exact_bytes_digest_and_overflow_evidence() {
    use std::os::unix::ffi::OsStringExt;

    let run = tempfile::tempdir().unwrap();
    let mut target = vec![b'x'; 256];
    target[127] = 0xff;
    std::os::unix::fs::symlink(OsString::from_vec(target.clone()), run.path().join("link"))
        .unwrap();
    let root = CaptureRoot::open(run.path()).unwrap();
    let (files, skipped) = walk(&root).unwrap();
    assert!(skipped.is_empty());
    let found = &files["link"];
    for (cap, observed) in [(0, 1), (255, 256)] {
        assert!(matches!(
            found.read_content_bounded(cap).unwrap(),
            CaptureRead::TooLarge { observed_at_least } if observed_at_least == observed
        ));
    }
    for cap in [256, u64::MAX] {
        let CaptureRead::Complete(content) = found.read_content_bounded(cap).unwrap() else {
            panic!("the exact symlink byte cap must succeed");
        };
        assert_eq!(content.bytes, target);
        assert_eq!(content.digest, blake3::hash(&target).to_hex().to_string());
    }
}
