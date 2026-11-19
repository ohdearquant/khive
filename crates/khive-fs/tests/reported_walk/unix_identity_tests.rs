#[cfg(unix)]
mod unix {
    use super::*;
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{symlink, FileTypeExt, OpenOptionsExt, PermissionsExt};

    fn fifo(path: &Path) {
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is NUL-terminated and alive for the call.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    }

    #[test]
    fn links_and_special_files_are_counted_without_filtering_or_descent() {
        let scratch = Scratch::new();
        for path in ["dir/inside", "keep", "reject"] {
            scratch.file(path);
        }
        symlink("keep", scratch.path().join("file-link")).unwrap();
        symlink("dir", scratch.path().join("dir-link")).unwrap();
        symlink("missing", scratch.path().join("broken")).unwrap();
        fifo(&scratch.path().join("pipe"));
        let mut seen = Vec::new();
        let report = walk_tree_reported(scratch.path(), limits(1, 8, 2), |path| {
            seen.push(path.to_owned());
            path != Path::new("reject")
        })
        .unwrap();
        assert_eq!(seen, expected(&["keep", "reject", "dir/inside"]));
        assert_eq!(paths(&report), expected(&["dir/inside", "keep"]));
        assert_eq!(
            report.progress,
            WalkProgress {
                entries_visited: 8,
                files_discovered: 2,
                files_skipped: 5,
                links_skipped: 3
            }
        );
    }

    #[test]
    fn later_sibling_metadata_error_retains_complete_listing_and_prior_file_count() {
        let scratch = Scratch::new();
        scratch.file("a-trigger");
        scratch.file("z-victim");
        let mut seen = Vec::new();
        let failure = walk_tree_reported(scratch.path(), limits(0, 2, 2), |path| {
            seen.push(path.to_owned());
            fs::remove_file(scratch.path().join("z-victim")).unwrap();
            true
        })
        .unwrap_err();
        assert_eq!(seen, expected(&["a-trigger"]));
        assert_eq!(failure.path, Path::new("z-victim"));
        assert_eq!(failure.depth, 0);
        assert_eq!(
            failure.progress,
            WalkProgress {
                entries_visited: 2,
                files_discovered: 1,
                ..Default::default()
            }
        );
        let WalkFailureKind::Io(error) = &failure.kind else {
            panic!("missing sibling must stay an I/O failure")
        };
        assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
    }

    #[test]
    fn ordinary_user_child_permission_failure_keeps_prior_progress() {
        // SAFETY: geteuid has no arguments or memory preconditions.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("permission control requires an ordinary user; skipped as root");
            return;
        }
        let scratch = Scratch::new();
        scratch.file("a-readable");
        scratch.file("z-denied/secret");
        let denied = scratch.path().join("z-denied");
        let permissions = fs::metadata(&denied).unwrap().permissions();
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o0)).unwrap();
        let mut seen = Vec::new();
        let result = walk_tree_reported(scratch.path(), limits(1, 3, 2), |path| {
            seen.push(path.to_owned());
            true
        });
        // Restore access before checking the result, including an unexpected success.
        fs::set_permissions(&denied, permissions).unwrap();
        let failure = result.unwrap_err();
        assert_eq!(failure.path, Path::new("z-denied"));
        assert_eq!(
            failure.depth, 0,
            "child-open errors use the containing depth"
        );
        assert_eq!(
            failure.progress,
            WalkProgress {
                entries_visited: 2,
                files_discovered: 1,
                ..Default::default()
            }
        );
        assert_eq!(seen, expected(&["a-readable"]));
        let WalkFailureKind::Io(error) = &failure.kind else {
            panic!("permission refusal must remain a native I/O error")
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn queued_directory_and_selected_parent_survive_path_replacement() {
        let scratch = Scratch::new();
        scratch.file("root/a-dir/original");
        scratch.file("root/z-trigger");
        scratch.file("outside/wrong");
        let root = scratch.path().join("root");
        let report = walk_tree_reported(&root, limits(1, 3, 2), |path| {
            if path == Path::new("z-trigger") {
                fs::rename(root.join("a-dir"), scratch.path().join("moved")).unwrap();
                symlink(scratch.path().join("outside"), root.join("a-dir")).unwrap();
            }
            true
        })
        .unwrap();
        assert_eq!(paths(&report), expected(&["a-dir/original", "z-trigger"]));
        fs::rename(
            scratch.path().join("moved"),
            scratch.path().join("moved-again"),
        )
        .unwrap();
        assert_eq!(contents(&report.files[0]), "fixture");
        fs::remove_file(scratch.path().join("moved-again/original")).unwrap();
        fs::write(
            scratch.path().join("moved-again/original"),
            b"replacement leaf",
        )
        .unwrap();
        assert_eq!(
            contents(&report.files[0]),
            "replacement leaf",
            "repeat opens may observe a new regular leaf in the retained parent"
        );
    }

    #[test]
    fn selected_leaf_replaced_by_symlink_is_refused_at_open_time() {
        let scratch = Scratch::new();
        scratch.file("root/file");
        scratch.file("outside");
        let root = scratch.path().join("root");
        let report = walk_tree_reported(&root, limits(0, 1, 1), |_| true).unwrap();
        fs::remove_file(root.join("file")).unwrap();
        symlink(scratch.path().join("outside"), root.join("file")).unwrap();
        let error = report.files[0].open_read().unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ELOOP));
    }

    #[test]
    fn fifo_replacement_is_nonblocking_and_left_for_handle_type_rejection() {
        let scratch = Scratch::new();
        scratch.file("file");
        let report = walk_tree_reported(scratch.path(), limits(0, 1, 1), |_| true).unwrap();
        let path = scratch.path().join("file");
        fs::remove_file(&path).unwrap();
        fifo(&path);
        // Peers keep a blocking regression from hanging the suite; the returned
        // descriptor's actual flags independently enforce the nonblocking contract.
        let _reader = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        let _writer = fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        let file = report.files[0]
            .open_read()
            .expect("open phase must not hide caller type checks");
        // SAFETY: file owns a live descriptor and F_GETFL only reads its flags.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_ne!(flags & libc::O_NONBLOCK, 0);
        let metadata = file.metadata().unwrap();
        assert!(metadata.file_type().is_fifo());
        assert!(
            !metadata.is_file(),
            "the caller must refuse the opened nonregular handle before reading"
        );
    }

    #[test]
    fn normalized_final_root_symlinks_are_refused_but_ancestor_links_remain_allowed() {
        let scratch = Scratch::new();
        scratch.file("real/file");
        symlink("real", scratch.path().join("link")).unwrap();
        for spelling in ["link", "link/", "link/.", "link//."] {
            let failure =
                walk_tree_reported(&scratch.path().join(spelling), limits(1, 3, 3), |_| {
                    panic!("root link must refuse")
                })
                .unwrap_err();
            assert!(matches!(failure.kind, WalkFailureKind::Io(_)), "{spelling}");
            assert!(failure.path.as_os_str().is_empty());
            assert_eq!(failure.depth, 0);
            assert_eq!(failure.progress, WalkProgress::default());
        }
        let report =
            walk_tree_reported(&scratch.path().join("real/."), limits(0, 1, 1), |_| true).unwrap();
        assert_eq!(paths(&report), expected(&["file"]));
        scratch.file("parent/child/file");
        symlink("parent", scratch.path().join("parent-link")).unwrap();
        let report = walk_tree_reported(
            &scratch.path().join("parent-link/child"),
            limits(0, 1, 1),
            |_| true,
        )
        .unwrap();
        assert_eq!(paths(&report), expected(&["file"]));
    }
}
