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
