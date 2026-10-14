use std::error::Error as _;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use khive_fs::tree_discovery::{
    walk_tree_reported, ReportedWalkLimits, WalkFailureKind, WalkFile, WalkProgress, WalkReport,
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "khive-reported-walk-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn file(&self, relative: impl AsRef<Path>) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"fixture").unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn limits(depth: usize, entries: usize, files: usize) -> ReportedWalkLimits {
    ReportedWalkLimits {
        max_depth: depth,
        max_entries: entries,
        max_files: files,
    }
}

fn paths(report: &WalkReport) -> Vec<PathBuf> {
    report
        .files
        .iter()
        .map(|file| file.relative_path().to_owned())
        .collect()
}

fn expected(paths: &[&str]) -> Vec<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
}

fn contents(file: &WalkFile) -> String {
    let mut text = String::new();
    file.open_read().unwrap().read_to_string(&mut text).unwrap();
    text
}

#[test]
fn complete_listings_filter_regular_files_once_and_sort_final_paths() {
    let scratch = Scratch::new();
    for path in [".hidden", "a.reject", "b/keep", "z"] {
        scratch.file(path);
    }
    let mut seen = Vec::new();
    let report = walk_tree_reported(scratch.path(), limits(1, 5, 3), |path| {
        seen.push(path.to_owned());
        path != Path::new("a.reject")
    })
    .unwrap();
    assert_eq!(seen, expected(&[".hidden", "a.reject", "z", "b/keep"]));
    assert_eq!(paths(&report), expected(&[".hidden", "b/keep", "z"]));
    assert_eq!(
        report.progress,
        WalkProgress {
            entries_visited: 5,
            files_discovered: 3,
            files_skipped: 1,
            links_skipped: 0,
        }
    );
    assert!(report.files.iter().all(|file| contents(file) == "fixture"));
    assert!(report.files.iter().all(|file| file
        .relative_path()
        .components()
        .all(|part| matches!(part, std::path::Component::Normal(_)))));
}

#[test]
fn zero_limits_allow_empty_roots_and_count_the_first_excess_candidate() {
    let scratch = Scratch::new();
    let empty =
        walk_tree_reported(scratch.path(), limits(0, 0, 0), |_| panic!("empty root")).unwrap();
    assert_eq!(empty.progress, WalkProgress::default());
    assert!(empty.files.is_empty());
    scratch.file("only");
    let exact = walk_tree_reported(scratch.path(), limits(0, 1, 1), |_| true).unwrap();
    assert_eq!(paths(&exact), expected(&["only"]));
    let rejected = walk_tree_reported(scratch.path(), limits(0, 1, 0), |_| false).unwrap();
    assert_eq!(rejected.progress.files_skipped, 1);
    assert_eq!(rejected.progress.files_discovered, 0);
    let mut calls = 0;
    let file_error = walk_tree_reported(scratch.path(), limits(0, 1, 0), |_| {
        calls += 1;
        true
    })
    .unwrap_err();
    assert!(matches!(file_error.kind, WalkFailureKind::FileLimit));
    assert_eq!(file_error.path, Path::new("only"));
    assert_eq!(file_error.depth, 0);
    assert_eq!(file_error.progress.entries_visited, 1);
    assert_eq!(file_error.progress.files_discovered, 1);
    assert_eq!(calls, 1);
    let entry_error = walk_tree_reported(scratch.path(), limits(0, 0, 1), |_| {
        panic!("partial listing must not filter")
    })
    .unwrap_err();
    assert!(matches!(entry_error.kind, WalkFailureKind::EntryLimit));
    assert_eq!(entry_error.path, Path::new("only"));
    assert_eq!(entry_error.depth, 0);
    assert_eq!(
        entry_error.progress,
        WalkProgress {
            entries_visited: 1,
            ..Default::default()
        }
    );
    assert!(entry_error.source().is_none());
}

#[test]
fn depth_refusal_uses_child_depth_and_precedes_later_filter_calls() {
    let scratch = Scratch::new();
    fs::create_dir(scratch.path().join("a-empty")).unwrap();
    scratch.file("z");
    let failure = walk_tree_reported(scratch.path(), limits(0, 2, 1), |_| {
        panic!("directory is refused before z")
    })
    .unwrap_err();
    assert!(matches!(failure.kind, WalkFailureKind::DepthLimit));
    assert_eq!(failure.path, Path::new("a-empty"));
    assert_eq!(failure.depth, 1);
    assert_eq!(
        failure.progress,
        WalkProgress {
            entries_visited: 2,
            ..Default::default()
        }
    );
    let report = walk_tree_reported(scratch.path(), limits(1, 2, 1), |_| true).unwrap();
    assert_eq!(paths(&report), expected(&["z"]));
}

#[test]
fn file_limit_follows_breadth_first_processing_and_retains_attempted_count() {
    let scratch = Scratch::new();
    for path in ["a/deep/file", "a/file", "b/file", "z"] {
        scratch.file(path);
    }
    let mut seen = Vec::new();
    let failure = walk_tree_reported(scratch.path(), limits(2, 6, 2), |path| {
        seen.push(path.to_owned());
        true
    })
    .unwrap_err();
    assert!(matches!(failure.kind, WalkFailureKind::FileLimit));
    assert_eq!(failure.path, Path::new("b/file"));
    assert_eq!(failure.depth, 1);
    assert_eq!(seen, expected(&["z", "a/file", "b/file"]));
    assert_eq!(
        failure.progress,
        WalkProgress {
            entries_visited: 6,
            files_discovered: 3,
            ..Default::default()
        }
    );
}

#[test]
fn later_partial_listing_retains_prior_progress_without_any_of_its_callbacks() {
    let scratch = Scratch::new();
    for path in ["a/keep", "b/first", "b/second", "z"] {
        scratch.file(path);
    }
    let mut seen = Vec::new();
    let failure = walk_tree_reported(scratch.path(), limits(1, 5, 10), |path| {
        seen.push(path.to_owned());
        true
    })
    .unwrap_err();
    assert!(matches!(failure.kind, WalkFailureKind::EntryLimit));
    assert_eq!(failure.path.parent(), Some(Path::new("b")));
    assert!(matches!(
        failure.path.file_name().and_then(|name| name.to_str()),
        Some("first" | "second")
    ));
    assert_eq!(failure.depth, 1);
    assert_eq!(
        failure.progress,
        WalkProgress {
            entries_visited: 6,
            files_discovered: 2,
            ..Default::default()
        }
    );
    assert_eq!(seen, expected(&["z", "a/keep"]));
}

#[test]
fn root_io_failures_have_empty_context_and_preserve_the_native_error_source() {
    let scratch = Scratch::new();
    scratch.file("file");
    for root in [
        PathBuf::new(),
        scratch.path().join("missing"),
        scratch.path().join("file"),
    ] {
        let failure =
            walk_tree_reported(&root, limits(1, 10, 10), |_| panic!("invalid root")).unwrap_err();
        assert!(failure.path.as_os_str().is_empty());
        assert_eq!(failure.depth, 0);
        assert_eq!(failure.progress, WalkProgress::default());
        let WalkFailureKind::Io(error) = &failure.kind else {
            panic!("root error must be native I/O")
        };
        let source = failure
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        assert!(std::ptr::eq(error, source));
        if root.as_os_str().is_empty() {
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }
}

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

// APFS refuses these raw non-UTF-8 names; Linux fixtures retain the native bytes.
#[cfg(target_os = "linux")]
#[test]
fn invalid_utf8_names_are_retained_and_overflow_reports_the_native_excess_witness() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let scratch = Scratch::new();
    let names = [
        OsStr::new("z"),
        OsStr::new(".hidden"),
        OsStr::from_bytes(b"bad-\xff"),
    ];
    for name in names {
        scratch.file(name);
    }
    let failure = walk_tree_reported(scratch.path(), limits(0, 1, 3), |_| {
        panic!("incomplete raw listing")
    })
    .unwrap_err();
    assert!(matches!(failure.kind, WalkFailureKind::EntryLimit));
    assert!(names.contains(&failure.path.as_os_str()));
    assert_eq!(failure.depth, 0);
    assert_eq!(
        failure.progress,
        WalkProgress {
            entries_visited: 2,
            ..Default::default()
        }
    );
    let mut seen = Vec::new();
    let report = walk_tree_reported(scratch.path(), limits(0, 3, 3), |path| {
        seen.push(path.to_owned());
        true
    })
    .unwrap();
    assert_eq!(report.progress.entries_visited, 3);
    assert_eq!(report.progress.files_discovered, 3);
    assert!(paths(&report).contains(&PathBuf::from(OsStr::from_bytes(b"bad-\xff"))));
    let mut sorted: Vec<_> = names.into_iter().map(PathBuf::from).collect();
    sorted.sort();
    assert_eq!(seen, sorted);
    assert_eq!(paths(&report), sorted);

    // A singleton provides an exact raw-byte excess witness independently of
    // the unspecified native ordering of separate directory enumerations.
    let singleton = Scratch::new();
    let raw_name = OsStr::from_bytes(b"only-\xfe");
    singleton.file(raw_name);
    let failure = walk_tree_reported(singleton.path(), limits(0, 0, 1), |_| {
        panic!("zero entry cap must not filter")
    })
    .unwrap_err();
    assert!(matches!(failure.kind, WalkFailureKind::EntryLimit));
    assert_eq!(failure.path.as_os_str().as_bytes(), b"only-\xfe");
    assert_eq!(failure.depth, 0);
    assert_eq!(
        failure.progress,
        WalkProgress {
            entries_visited: 1,
            ..Default::default()
        }
    );
}
