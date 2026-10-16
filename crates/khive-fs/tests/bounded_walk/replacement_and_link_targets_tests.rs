#[test]
fn directory_replaced_during_filter_is_not_entered() {
    for replace_with_link in [false, true] {
        let scratch = Scratch::new();
        scratch.file("root/child/original");
        scratch.file("replacement/wrong");
        let root = scratch.path().join("root");
        let result = walk_tree(&root, limits(1, 10, false), |path, _| {
            if path.as_path() == Path::new("child") {
                fs::rename(root.join("child"), scratch.path().join("moved-child")).unwrap();
                if replace_with_link {
                    symlink(scratch.path().join("replacement"), root.join("child")).unwrap();
                } else {
                    fs::rename(scratch.path().join("replacement"), root.join("child")).unwrap();
                }
            }
            true
        });
        if replace_with_link {
            assert!(matches!(result, Err(WalkError::Io { .. })));
        } else {
            assert!(matches!(result, Err(WalkError::EntryChanged { .. })));
        }
    }
}

#[test]
fn followed_directory_is_pinned_before_filter_can_replace_its_target() {
    let scratch = Scratch::new();
    scratch.file("root/target/original");
    scratch.file("replacement/wrong");
    let root = scratch.path().join("root");
    symlink("target", root.join("a-link")).unwrap();
    let result = walk_tree(&root, limits(1, 3, true), |path, _| {
        if path.as_path() == Path::new("a-link") {
            fs::rename(root.join("target"), scratch.path().join("moved-target")).unwrap();
            fs::rename(scratch.path().join("replacement"), root.join("target")).unwrap();
        }
        path.as_path() != Path::new("target")
    })
    .unwrap();
    assert_eq!(paths(result), expected(&["a-link/original"]));
}

#[test]
fn root_symlinks_and_non_directories_are_refused_including_trailing_spelling() {
    let scratch = Scratch::new();
    scratch.file("real/file");
    symlink("real", scratch.path().join("link")).unwrap();
    for spelling in ["link", "link/", "link/.", "real/file", "missing"] {
        assert!(
            matches!(
                walk_tree(
                    &scratch.path().join(spelling),
                    limits(1, 10, false),
                    |_, _| true
                ),
                Err(WalkError::Io { .. })
            ),
            "{spelling}"
        );
    }
    assert!(matches!(
        walk_tree(Path::new(""), limits(1, 10, false), |_, _| true),
        Err(WalkError::Io { .. })
    ));
    assert_eq!(
        paths(
            walk_tree(
                &scratch.path().join("real/."),
                limits(0, 1, false),
                |_, _| true
            )
            .unwrap()
        ),
        expected(&["file"])
    );
}

#[test]
fn link_target_trailing_separator_still_requires_a_directory() {
    let scratch = Scratch::new();
    scratch.file("file");
    for target in [
        PathBuf::from("file/"),
        PathBuf::from("file/."),
        scratch.path().join("file/"),
        scratch.path().join("file/."),
    ] {
        symlink(target, scratch.path().join("link")).unwrap();
        assert!(
            matches!(walk_tree(scratch.path(), limits(1, 2, true), |_, _| true),
            Err(WalkError::Io { source, .. }) if source.raw_os_error() == Some(libc::ENOTDIR))
        );
        fs::remove_file(scratch.path().join("link")).unwrap();
    }
}
