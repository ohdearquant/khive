#[test]
fn depth_zero_permits_root_files_and_filter_prunes_before_depth_admission() {
    let scratch = Scratch::new();
    scratch.file("file");
    assert_eq!(
        paths(walk_tree(scratch.path(), limits(0, 1, false), |_, _| true).unwrap()),
        expected(&["file"])
    );
    scratch.file("dir/inner/deep");
    assert!(matches!(
        walk_tree(scratch.path(), limits(0, 10, false), |_, _| true),
        Err(WalkError::DepthExceeded { max_depth: 0, .. })
    ));
    assert_eq!(
        paths(
            walk_tree(scratch.path(), limits(0, 2, false), |_, kind| kind
                != WalkEntryKind::Directory)
            .unwrap()
        ),
        expected(&["file"])
    );
    assert!(matches!(
        walk_tree(scratch.path(), limits(1, 10, false), |_, _| true),
        Err(WalkError::DepthExceeded { max_depth: 1, .. })
    ));
    assert_eq!(
        paths(walk_tree(scratch.path(), limits(2, 4, false), |_, _| true).unwrap()),
        expected(&["dir/inner/deep", "file"])
    );
}

#[test]
fn never_follow_skips_directory_and_file_links_without_calling_filter() {
    let scratch = Scratch::new();
    let outside = Scratch::new();
    scratch.file("real/file");
    outside.file("outside");
    symlink("real", scratch.path().join("directory-link")).unwrap();
    symlink("real/file", scratch.path().join("file-link")).unwrap();
    symlink(outside.path(), scratch.path().join("outside-link")).unwrap();
    let mut seen = Vec::new();
    let result = walk_tree(scratch.path(), limits(1, 5, false), |path, kind| {
        seen.push((path.as_path().to_path_buf(), kind));
        true
    })
    .unwrap();
    assert_eq!(paths(result), expected(&["real/file"]));
    assert_eq!(
        seen,
        [
            (PathBuf::from("real"), WalkEntryKind::Directory),
            (PathBuf::from("real/file"), WalkEntryKind::RegularFile),
        ]
    );
}

#[test]
fn followed_relative_absolute_and_parent_links_are_confined_and_filtered_once() {
    let scratch = Scratch::new();
    scratch.file("target/file");
    scratch.file("nest/file");
    symlink("../target/file", scratch.path().join("nest/parent-link")).unwrap();
    symlink("target", scratch.path().join("a-alias")).unwrap();
    symlink(
        scratch.path().join("target/file"),
        scratch.path().join("absolute-file"),
    )
    .unwrap();
    let mut kinds = Vec::new();
    let result = walk_tree(scratch.path(), limits(2, 20, true), |path, kind| {
        kinds.push((path.as_path().to_path_buf(), kind));
        true
    })
    .unwrap();
    assert_eq!(
        paths(result),
        expected(&[
            "a-alias/file",
            "absolute-file",
            "nest/file",
            "nest/parent-link"
        ])
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|(path, _)| path == Path::new("a-alias"))
            .count(),
        1
    );
    assert!(kinds.contains(&(PathBuf::from("absolute-file"), WalkEntryKind::RegularFile)));
}

#[test]
fn rejected_alias_does_not_poison_later_directory_and_root_alias_does_not_recurse() {
    let scratch = Scratch::new();
    scratch.file("target/file");
    symlink("target", scratch.path().join("a-rejected")).unwrap();
    symlink("target", scratch.path().join("b-accepted")).unwrap();
    symlink(".", scratch.path().join("root-cycle")).unwrap();
    let result = walk_tree(scratch.path(), limits(2, 5, true), |path, _| {
        path.as_path() != Path::new("a-rejected")
    })
    .unwrap();
    assert_eq!(paths(result), expected(&["b-accepted/file"]));
}

#[test]
fn depth_admission_precedes_root_alias_deduplication() {
    let scratch = Scratch::new();
    symlink(".", scratch.path().join("root-cycle")).unwrap();
    assert!(
        matches!(walk_tree(scratch.path(), limits(0, 1, true), |_, _| true),
        Err(WalkError::DepthExceeded { max_depth: 0, path }) if path.as_path() == Path::new("root-cycle"))
    );
    assert!(walk_tree(scratch.path(), limits(1, 1, true), |_, _| true)
        .unwrap()
        .is_empty());
    assert!(walk_tree(scratch.path(), limits(0, 1, true), |_, _| false)
        .unwrap()
        .is_empty());
}

#[test]
fn outside_links_refuse_even_when_the_filter_would_reject_them() {
    let parent = Scratch::new();
    parent.file("root/file");
    parent.file("outside/marker");
    let root = parent.path().join("root");
    for target in [
        PathBuf::from("../outside"),
        parent.path().join("outside"),
        root.join("../outside"),
    ] {
        symlink(&target, root.join("escape")).unwrap();
        assert!(
            matches!(
                walk_tree(&root, limits(2, 10, true), |_, _| false),
                Err(WalkError::OutsideRoot { .. })
            ),
            "target {target:?}"
        );
        fs::remove_file(root.join("escape")).unwrap();
    }
}
