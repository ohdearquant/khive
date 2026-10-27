use super::*;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::FromRawFd;

#[test]
fn limit_pipe_descriptors_are_close_on_exec() {
    let (reader, writer) = limit_pipe().unwrap();
    let reader = unsafe { std::fs::File::from_raw_fd(reader) };
    let writer = unsafe { std::fs::File::from_raw_fd(writer) };
    for fd in [
        std::os::fd::AsRawFd::as_raw_fd(&reader),
        std::os::fd::AsRawFd::as_raw_fd(&writer),
    ] {
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }
}

#[tokio::test]
async fn limit_report_arrives_without_waiting_for_writer_eof() {
    let (reader, writer) = limit_pipe().unwrap();
    let writer = unsafe { std::fs::File::from_raw_fd(writer) };
    let bytes = encode_limit_report(
        PreparedLimits::new(&sandbox::Limits {
            cpu_seconds: Some(1),
            ..Default::default()
        }),
        |_, value| Ok(value),
    )
    .unwrap();
    let written = unsafe {
        libc::write(
            std::os::fd::AsRawFd::as_raw_fd(&writer),
            bytes.as_ptr().cast(),
            bytes.len(),
        )
    };
    assert_eq!(written, bytes.len() as isize);
    let outcome = tokio::time::timeout(
        Duration::from_millis(750),
        collect_limit_report(reader, Duration::from_secs(1)),
    )
    .await;
    // Close the held writer before asserting the deadline. A broken
    // EOF-dependent reader must be released before this test's Tokio
    // runtime shuts down, so the failure stays bounded.
    drop(writer);
    let report = outcome.expect("report read waited for writer EOF");
    assert_eq!(report, json!({"cpu_seconds": 1}));
}

#[tokio::test]
async fn silent_limit_report_writer_releases_blocking_worker() {
    let (reader, writer) = limit_pipe().unwrap();
    let writer = unsafe { std::fs::File::from_raw_fd(writer) };
    let task =
        tokio::task::spawn_blocking(move || read_limit_report(reader, Duration::from_millis(30)));
    let outcome = tokio::time::timeout(Duration::from_millis(750), task).await;
    drop(writer);
    let report = outcome
        .expect("blocking report reader did not reach its deadline")
        .unwrap();
    assert_eq!(report, json!({}));
}

#[test]
fn pending_limit_report_does_not_hold_async_executor() {
    let (reader, writer) = limit_pipe().unwrap();
    let writer = unsafe { std::fs::File::from_raw_fd(writer) };
    let (progress_tx, progress_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let report = runtime.block_on(async move {
            let report_task =
                tokio::spawn(collect_limit_report(reader, Duration::from_millis(800)));
            tokio::task::yield_now().await;
            let _ = progress_tx.send(());
            report_task.await.unwrap()
        });
        let _ = done_tx.send(report);
    });
    // A synchronous pipe read on the current-thread executor prevents its
    // own Tokio timer from firing. Observe progress from this independent
    // test thread, then release the held writer before any assertion.
    let progress = progress_rx.recv_timeout(Duration::from_millis(500));
    drop(writer);
    let report = done_rx.recv_timeout(Duration::from_secs(2));
    if report.is_ok() {
        worker.join().unwrap();
    }
    assert!(
        progress.is_ok(),
        "pending report blocked the async executor"
    );
    assert_eq!(
        report.expect("pending report exceeded its deadline"),
        json!({})
    );
}

#[test]
fn wide_list_limit_saturates_before_clamping() {
    let params = json!({"limit": u64::from(u32::MAX) + 1});
    assert_eq!(opt_limit(&params, "limit", 20, 500).unwrap(), 500);
    let params = json!({"limit": u64::MAX});
    assert_eq!(opt_limit(&params, "limit", 20, 500).unwrap(), 500);
}

#[test]
fn input_budget_counts_repeated_materialization_and_cannot_wrap() {
    assert_eq!(checked_input_total(3, 2, 5).unwrap(), 5);
    assert!(checked_input_total(3, 3, 5).is_err());
    assert!(checked_input_total(u64::MAX, 1, u64::MAX).is_err());
}

#[test]
fn binary_digest_is_streamed_and_a_failed_read_is_not_a_digest() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("tool");
    let bytes = vec![0x5a; 128 * 1024];
    std::fs::write(&binary, &bytes).unwrap();
    let budget = BinaryDigestBudget {
        started: Instant::now(),
        timeout: Duration::from_secs(5),
        max_bytes: MAX_TOOL_BINARY_BYTES,
    };
    let progress = BinaryDigestProgress::default();
    assert_eq!(
        hash_tool_binary(&binary, budget, &progress).unwrap(),
        digest_hex(&bytes)
    );
    std::fs::remove_file(&binary).unwrap();
    let missing_budget = BinaryDigestBudget {
        started: Instant::now(),
        ..budget
    };
    let missing = hash_tool_binary(&binary, missing_budget, &BinaryDigestProgress::default());
    assert!(matches!(
        missing,
        Err(BinaryDigestError {
            kind: BinaryDigestErrorKind::Read,
            ..
        })
    ));
}

#[test]
fn binary_digest_refuses_over_cap_without_returning_a_partial_digest() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("oversize-tool");
    std::fs::write(&binary, vec![0x5a; 2 * 1024]).unwrap();
    let budget = BinaryDigestBudget {
        started: Instant::now(),
        timeout: Duration::from_secs(5),
        max_bytes: 1024,
    };
    let progress = BinaryDigestProgress::default();
    let error = hash_tool_binary(&binary, budget, &progress).unwrap_err();
    assert_eq!(error.kind, BinaryDigestErrorKind::ByteLimit);
    assert_eq!(error.kind.code(), "binary_digest_byte_limit");
    assert_eq!(error.detail()["bytes_read"], 1025);
    assert_eq!(error.detail()["byte_cap"], 1024);
    assert_eq!(error.detail()["path_class"], "regular_file");
}

#[test]
fn binary_digest_refuses_expired_deadline_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("tool");
    std::fs::write(&binary, b"tool").unwrap();
    let budget = BinaryDigestBudget {
        started: Instant::now() - Duration::from_secs(2),
        timeout: Duration::from_secs(1),
        max_bytes: MAX_TOOL_BINARY_BYTES,
    };
    let error = hash_tool_binary(&binary, budget, &BinaryDigestProgress::default()).unwrap_err();
    assert_eq!(error.kind, BinaryDigestErrorKind::TimeLimit);
    assert_eq!(error.kind.code(), "binary_digest_time_limit");
    assert_eq!(error.detail()["bytes_read"], 0);
    assert_eq!(error.detail()["time_cap_ms"], 1000);
}

#[test]
fn binary_digest_refuses_reader_that_stalls_past_its_deadline() {
    struct SlowEof {
        read_called: bool,
    }
    impl Read for SlowEof {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            self.read_called = true;
            std::thread::sleep(Duration::from_millis(75));
            Ok(0)
        }
    }

    let budget = BinaryDigestBudget {
        started: Instant::now(),
        timeout: Duration::from_millis(50),
        max_bytes: MAX_TOOL_BINARY_BYTES,
    };
    let mut reader = SlowEof { read_called: false };
    let error =
        hash_tool_binary_reader(&mut reader, budget, &BinaryDigestProgress::default()).unwrap_err();
    assert!(
        reader.read_called,
        "the reader did not consume the deadline"
    );
    assert_eq!(error.kind.code(), "binary_digest_time_limit");
    assert!(error.elapsed_ms >= error.time_cap_ms);
    assert_eq!(error.bytes_read, 0);
    assert_eq!(error.time_cap_ms, 50);
}

#[tokio::test(start_paused = true)]
async fn report_collection_spends_the_same_wall_budget_as_child_wait() {
    let started = tokio::time::Instant::now();
    let (report, status) = collect_report_and_wait(
        started,
        Duration::from_secs(1),
        async {
            tokio::time::sleep(Duration::from_millis(600)).await;
            json!({"delayed": true})
        },
        async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            "exited"
        },
    )
    .await;
    assert_eq!(report, json!({"delayed": true}));
    assert!(status.is_err(), "child outlived the shared wall deadline");
}

#[test]
fn invalid_resolved_timeout_cannot_reach_duration_conversion() {
    let mut config = sandbox::resolve(&Default::default());
    config.timeout_default_s = f64::NAN;
    assert!(parse_request(
        &json!({"tree": "tree", "tool": "tool", "actor": "local"}),
        &config,
    )
    .is_err());
}

#[test]
fn limit_report_uses_values_prepared_before_spawn() {
    let mut limits = sandbox::Limits {
        cpu_seconds: Some(5),
        file_size: Some(1_048_576),
        address_space: Some(4_096),
        nproc: Some(2),
    };
    let prepared = PreparedLimits::new(&limits);
    limits.cpu_seconds = Some(99);
    limits.file_size = None;
    assert_eq!((limits.cpu_seconds, limits.file_size), (Some(99), None));

    let mut applied = Vec::new();
    let report = encode_limit_report(prepared, |resource, value| {
        applied.push((resource, value));
        Ok(value + 1)
    })
    .unwrap();
    assert_eq!(applied[0], (libc::RLIMIT_CPU, 5));
    assert_eq!(applied[1], (libc::RLIMIT_FSIZE, 1_048_576));

    let expected = json!({ "cpu_seconds": 6, "file_size": 1_048_577 });
    #[cfg(not(target_os = "macos"))]
    let expected = {
        assert_eq!(applied[2], (libc::RLIMIT_AS, 4_096));
        assert_eq!(applied[3], (libc::RLIMIT_NPROC, 2));
        let mut expected = expected;
        expected["address_space"] = json!(4_097);
        expected["nproc"] = json!(3);
        expected
    };
    #[cfg(target_os = "macos")]
    assert_eq!(applied.len(), 2);
    assert_eq!(decode_limit_report(&report), expected);

    let empty = encode_limit_report(PreparedLimits::new(&sandbox::Limits::default()), |_, _| {
        panic!("unset limit must not be applied")
    })
    .unwrap();
    assert_eq!(decode_limit_report(&empty), json!({}));
}

#[test]
fn materialize_preserves_literal_symlink_targets() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("run");
    let targets: &[(&str, &[u8])] = &[
        ("inside", b"sub/../file"),
        ("escape", b"../../x"),
        ("absolute", b"/absolute/missing"),
        ("non_utf8", b"target-\xff"),
    ];
    let mut entries = vec![TreeEntry {
        path: "file".into(),
        content_ref: digest_hex(b"content"),
        mode: 644,
    }];
    let mut blobs = BTreeMap::from([(digest_hex(b"content"), b"content".to_vec())]);
    for (path, target) in targets {
        let content_ref = digest_hex(target);
        entries.push(TreeEntry {
            path: (*path).into(),
            content_ref: content_ref.clone(),
            mode: 120000,
        });
        blobs.insert(content_ref, target.to_vec());
    }
    materialize(&root, &entries, &blobs).unwrap();
    assert_eq!(std::fs::read(root.join("file")).unwrap(), b"content");
    for (path, expected) in targets {
        let path = root.join(path);
        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_link(path).unwrap().as_os_str().as_bytes(),
            *expected
        );
    }
}

#[test]
fn materialize_symlink_aliases_never_redirect_file_writes() {
    for descendant in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("child");
        std::fs::write(&sentinel, b"unchanged").unwrap();
        let link_target = if descendant {
            outside.clone()
        } else {
            sentinel.clone()
        };
        let target = link_target.as_os_str().as_bytes().to_vec();
        let link_ref = digest_hex(&target);
        let file_ref = digest_hex(b"replacement");
        let entries = vec![
            TreeEntry {
                path: "A".into(),
                content_ref: link_ref.clone(),
                mode: 120000,
            },
            TreeEntry {
                path: if descendant { "a/child" } else { "a" }.into(),
                content_ref: file_ref.clone(),
                mode: 644,
            },
        ];
        let bytes = BTreeMap::from([(link_ref, target), (file_ref, b"replacement".to_vec())]);
        let root = dir.path().join("run");
        let result = materialize(&root, &entries, &bytes);
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged");
        match result {
            Ok(()) => {
                // A case-sensitive filesystem can represent both paths safely.
                assert!(std::fs::symlink_metadata(root.join("A"))
                    .unwrap()
                    .file_type()
                    .is_symlink());
                assert_eq!(
                    std::fs::read(root.join(if descendant { "a/child" } else { "a" })).unwrap(),
                    b"replacement"
                );
            }
            Err(error) => {
                assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
                assert!(!root.exists(), "the partial input tree must be removed");
            }
        }
    }
}

#[test]
fn materialize_refuses_an_existing_root_before_writes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("run");
    std::fs::create_dir(&root).unwrap();
    let existing = root.join("file");
    std::fs::write(&existing, b"unchanged").unwrap();
    let content_ref = digest_hex(b"replacement");
    let entries = vec![TreeEntry {
        path: "file".into(),
        content_ref: content_ref.clone(),
        mode: 644,
    }];
    let bytes = BTreeMap::from([(content_ref, b"replacement".to_vec())]);
    assert_eq!(
        materialize(&root, &entries, &bytes).unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(std::fs::read(existing).unwrap(), b"unchanged");
}

fn assert_binary_digest_non_regular(path: &Path, expected_class: &str) {
    let budget = BinaryDigestBudget {
        started: Instant::now(),
        timeout: Duration::from_secs(5),
        max_bytes: MAX_TOOL_BINARY_BYTES,
    };
    let error = hash_tool_binary(path, budget, &BinaryDigestProgress::default()).unwrap_err();
    assert_eq!(error.kind, BinaryDigestErrorKind::Read);
    assert_eq!(error.kind.code(), "binary_digest_read");
    assert_eq!(error.bytes_read, 0);
    assert_eq!(error.path_class, Some(expected_class));
    assert_eq!(
        error.cause.as_deref(),
        Some("registered tool is not a regular file")
    );
    let detail = error.detail();
    assert_eq!(detail["bytes_read"], 0);
    assert_eq!(detail["path_class"], expected_class);
    assert_eq!(detail["cause"], "registered tool is not a regular file");
}

#[test]
fn binary_digest_nofollow_keeps_regular_bytes_and_directory_error_details() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("regular-tool");
    let bytes = b"real regular tool bytes";
    std::fs::write(&binary, bytes).unwrap();
    let budget = BinaryDigestBudget {
        started: Instant::now(),
        timeout: Duration::from_secs(5),
        max_bytes: MAX_TOOL_BINARY_BYTES,
    };
    let progress = BinaryDigestProgress::default();
    assert_eq!(
        hash_tool_binary(&binary, budget, &progress).unwrap(),
        digest_hex(bytes)
    );
    assert_eq!(
        progress.bytes_read.load(Ordering::Relaxed),
        bytes.len() as u64
    );
    assert_eq!(*progress.path_class.lock().unwrap(), Some("regular_file"));
    assert_eq!(std::fs::read(&binary).unwrap(), bytes);
    let directory = dir.path().join("directory-tool");
    std::fs::create_dir(&directory).unwrap();
    assert_binary_digest_non_regular(&directory, "directory");
}

struct BoundedDigestChild {
    child: std::process::Child,
    cleanup_attempted: bool,
}

impl BoundedDigestChild {
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
            if let Some(status) = self
                .child
                .try_wait()
                .expect("poll binary digest FIFO child")
            {
                self.cleanup_attempted = true;
                return status;
            }
            if Instant::now() >= deadline {
                assert!(
                    self.stop_and_reap(),
                    "binary digest FIFO child exceeded the reap deadline"
                );
                panic!("binary digest FIFO open waited for a writer");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for BoundedDigestChild {
    fn drop(&mut self) {
        if !self.cleanup_attempted {
            let _ = self.stop_and_reap();
        }
    }
}

const DIGEST_FIFO_PATH: &str = "KHIVE_EXEC_NOFOLLOW_FIFO_PATH";
const DIGEST_FIFO_MARKER: &str = "KHIVE_EXEC_NOFOLLOW_FIFO_MARKER";

#[test]
#[ignore = "invoked only by binary_digest_fifo_refusal_is_bounded_and_keeps_details in an owned child"]
fn binary_digest_fifo_child_case() {
    use std::os::unix::fs::FileTypeExt as _;

    let path = PathBuf::from(std::env::var_os(DIGEST_FIFO_PATH).expect("binary digest FIFO path"));
    let marker =
        PathBuf::from(std::env::var_os(DIGEST_FIFO_MARKER).expect("binary digest FIFO marker"));
    assert!(std::fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_fifo());
    assert_binary_digest_non_regular(&path, "other");
    std::fs::write(marker, b"binary digest FIFO refusal checked").unwrap();
}

#[test]
fn binary_digest_fifo_refusal_is_bounded_and_keeps_details() {
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("digest-fifo");
    let marker = dir.path().join("digest-done");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is NUL-terminated and remains alive for mkfifo.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let child_name = concat!(module_path!(), "::binary_digest_fifo_child_case");
    let (_, child_name) = child_name
        .split_once("::")
        .expect("crate-qualified child test");
    let mut child = BoundedDigestChild {
        child: std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                child_name,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(DIGEST_FIFO_PATH, &fifo)
            .env(DIGEST_FIFO_MARKER, &marker)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("spawn binary digest FIFO child"),
        cleanup_attempted: false,
    };
    assert!(
        child.wait_bounded().success(),
        "binary digest FIFO child failed"
    );
    assert_eq!(
        std::fs::read(marker).unwrap(),
        b"binary digest FIFO refusal checked"
    );
}
