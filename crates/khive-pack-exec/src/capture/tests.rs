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
