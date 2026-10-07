#[test]
fn ann_root_is_database_scoped_sibling_dir() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let backend = StorageBackend::sqlite_for_test(&path).expect("file backend");
    let got = backend.ann_root().expect("file backend must return Some");
    assert_eq!(got, dir.path().join("data.db.ann"));
    assert!(StorageBackend::memory().unwrap().ann_root().is_none());
}

/// Two distinct non-UTF-8 database filenames must never share an ANN
/// root: a lossy UTF-8 conversion collapses both to the replacement
/// character, letting one database adopt the other's segments. Exercised
/// on the path derivation directly — APFS (macOS CI) refuses to create
/// files with non-UTF-8 names, so a real backend cannot be opened there.
#[cfg(unix)]
#[test]
fn ann_root_distinct_for_non_utf8_filenames() {
    use std::os::unix::ffi::OsStrExt;
    let path_a = std::path::Path::new("/data").join(std::ffi::OsStr::from_bytes(b"\xff.db"));
    let path_b = std::path::Path::new("/data").join(std::ffi::OsStr::from_bytes(b"\xfe.db"));
    let root_a = ann_root_for(&path_a).expect("Some for a file path");
    let root_b = ann_root_for(&path_b).expect("Some for a file path");
    assert_ne!(
        root_a, root_b,
        "distinct database files must map to distinct ANN roots"
    );
}
