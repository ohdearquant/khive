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
