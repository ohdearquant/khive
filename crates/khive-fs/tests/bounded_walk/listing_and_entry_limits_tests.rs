static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "khive-bounded-walk-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn file(&self, path: impl AsRef<Path>) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"fixture").unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn limits(depth: usize, entries: usize, follow: bool) -> WalkLimits {
    WalkLimits {
        max_depth: depth,
        max_entries: entries,
        follow_symlinks_within_root: follow,
    }
}

fn paths(paths: Vec<RelPath>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .map(|path| path.as_path().to_path_buf())
        .collect()
}

fn expected(paths: &[&str]) -> Vec<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
}

// Needs a non-UTF-8 file name, which APFS refuses (EILSEQ), so this runs where such names exist.
#[cfg(target_os = "linux")]
#[test]
fn bounded_listing_refuses_one_over_counts_raw_names_and_preserves_listing_position() {
    let scratch = Scratch::new();
    let directory = File::open(scratch.path()).unwrap();
    assert!(list_names_bounded(&directory, 0).unwrap().is_empty());
    scratch.file("z");
    scratch.file(".hidden");
    scratch.file(OsStr::from_bytes(b"bad-\xff"));
    symlink("z", scratch.path().join("link")).unwrap();
    let error = list_names_bounded(&directory, 3).unwrap_err();
    assert!(matches!(error, ListNamesError::LimitExceeded { max: 3 }));
    assert!(error.to_string().contains('3'));
    assert_eq!(
        list_names_bounded(&directory, 4).unwrap(),
        [".hidden", "link", "z"]
    );
    assert_eq!(list_names(&directory).unwrap().len(), 4);
    assert_eq!(
        list_names_bounded(&directory, 4).unwrap(),
        [".hidden", "link", "z"]
    );
    assert!(matches!(
        list_names_bounded(&directory, 0),
        Err(ListNamesError::LimitExceeded { max: 0 })
    ));

    let regular = File::open(scratch.path().join("z")).unwrap();
    let ListNamesError::Io(error) = list_names_bounded(&regular, 2).unwrap_err() else {
        panic!("non-directory must retain the native error")
    };
    assert_eq!(error.raw_os_error(), Some(libc::ENOTDIR));
}

// Needs a non-UTF-8 file name, which APFS refuses (EILSEQ), so this runs where such names exist.
#[cfg(target_os = "linux")]
#[test]
fn global_entries_count_filtered_and_non_utf8_names_before_processing() {
    let scratch = Scratch::new();
    assert!(walk_tree(scratch.path(), limits(0, 0, false), |_, _| true)
        .unwrap()
        .is_empty());
    scratch.file("dir/file");
    scratch.file(".hidden");
    scratch.file(OsStr::from_bytes(b"bad-\xff"));
    let mut filtered = 0;
    assert!(matches!(
        walk_tree(scratch.path(), limits(1, 2, false), |_, _| {
            filtered += 1;
            false
        }),
        Err(WalkError::EntriesExceeded { max_entries: 2 })
    ));
    assert_eq!(
        filtered, 0,
        "a directory's complete raw-name budget is checked first"
    );
    assert!(walk_tree(scratch.path(), limits(0, 3, false), |_, _| false)
        .unwrap()
        .is_empty());
    assert!(matches!(
        walk_tree(scratch.path(), limits(1, 3, false), |_, _| true),
        Err(WalkError::EntriesExceeded { max_entries: 3 })
    ));
    let result = paths(walk_tree(scratch.path(), limits(1, 4, false), |_, _| true).unwrap());
    assert_eq!(result.len(), 3);
    assert!(result.contains(&PathBuf::from(OsStr::from_bytes(b"bad-\xff"))));
    assert!(result.contains(&PathBuf::from("dir/file")));
}

#[test]
fn prefix_listing_repeats_and_stays_on_the_held_directory() {
    use khive_fs::fd_relative::list_names_prefix_bounded;
    use std::fs::File;

    let scratch = Scratch::new();
    let original = scratch.path().join("original");
    let moved = scratch.path().join("moved");
    fs::create_dir(&original).unwrap();
    let held = File::open(&original).unwrap();
    assert_eq!(
        list_names_prefix_bounded(&held, 0).unwrap(),
        (Vec::<String>::new(), false)
    );
    for name in ["z", "a", ".hidden"] {
        fs::write(original.join(name), b"original").unwrap();
    }
    symlink("z", original.join("link")).unwrap();
    assert_eq!(
        list_names_prefix_bounded(&held, 0).unwrap(),
        (Vec::<String>::new(), true)
    );
    let expected = [".hidden", "a", "link", "z"];
    for _ in 0..2 {
        let (mut names, truncated) = list_names_prefix_bounded(&held, 4).unwrap();
        assert!(!truncated);
        names.sort();
        assert_eq!(names, expected);
        let (names, truncated) = list_names_prefix_bounded(&held, 2).unwrap();
        assert!(truncated);
        assert_eq!(names.len(), 2);
        assert!(names.iter().all(|name| expected.contains(&name.as_str())));
    }
    fs::rename(&original, &moved).unwrap();
    fs::create_dir(&original).unwrap();
    fs::write(original.join("replacement"), b"replacement").unwrap();
    let (mut names, truncated) = list_names_prefix_bounded(&held, 4).unwrap();
    names.sort();
    assert_eq!(names, expected);
    assert!(!truncated);
    assert_eq!(
        list_names_prefix_bounded(&File::open(&original).unwrap(), 1).unwrap(),
        (vec!["replacement".to_string()], false)
    );
    let regular = File::open(moved.join("z")).unwrap();
    assert_eq!(
        list_names_prefix_bounded(&regular, 2)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOTDIR)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn prefix_listing_counts_a_real_non_utf8_name_at_zero_and_exact_cap() {
    use khive_fs::fd_relative::list_names_prefix_bounded;
    use std::ffi::OsStr;
    use std::fs::File;
    use std::os::unix::ffi::OsStrExt as _;

    let scratch = Scratch::new();
    scratch.file(OsStr::from_bytes(b"raw-\xff"));
    let held = File::open(scratch.path()).unwrap();
    assert_eq!(
        list_names_prefix_bounded(&held, 0).unwrap(),
        (Vec::<String>::new(), true)
    );
    assert_eq!(
        list_names_prefix_bounded(&held, 1).unwrap(),
        (Vec::<String>::new(), false)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn prefix_listing_leaves_an_independently_observed_caller_cursor_unchanged() {
    use khive_fs::fd_relative::list_names_prefix_bounded;
    use std::fs::File;
    use std::os::fd::{AsRawFd as _, IntoRawFd as _};

    struct HeldStream(*mut libc::DIR);
    impl Drop for HeldStream {
        fn drop(&mut self) {
            // SAFETY: this test uniquely owns the successful fdopendir result.
            unsafe { libc::closedir(self.0) };
        }
    }
    let scratch = Scratch::new();
    scratch.file("a");
    scratch.file("z");
    let directory = File::open(scratch.path()).unwrap();
    let fd = directory.try_clone().unwrap().into_raw_fd();
    // SAFETY: fd is owned here; fdopendir takes it only on success.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        // SAFETY: fdopendir failed and did not take ownership.
        unsafe { libc::close(fd) };
        panic!("open independent cursor oracle: {error}");
    }
    let stream = HeldStream(stream);
    // SAFETY: stream owns a live DIR; the call advances its shared kernel position.
    assert!(!unsafe { libc::readdir(stream.0) }.is_null());
    // SAFETY: directory owns a live fd; SEEK_CUR with zero only observes its offset.
    let before = unsafe { libc::lseek(directory.as_raw_fd(), 0, libc::SEEK_CUR) };
    assert!(
        before > 0,
        "the independent cursor oracle must actually advance"
    );
    assert!(list_names_prefix_bounded(&directory, 1).unwrap().1);
    assert!(!list_names_prefix_bounded(&directory, 2).unwrap().1);
    // SAFETY: the same live fd is observed after both listings.
    let after = unsafe { libc::lseek(directory.as_raw_fd(), 0, libc::SEEK_CUR) };
    assert_eq!(
        after, before,
        "prefix listing changed the caller's directory position"
    );
}
