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
