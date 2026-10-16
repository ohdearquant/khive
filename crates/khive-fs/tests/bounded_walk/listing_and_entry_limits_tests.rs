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
