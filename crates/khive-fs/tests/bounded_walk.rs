#![cfg(unix)]

use std::ffi::OsStr;
use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use khive_fs::fd_relative::{list_names, list_names_bounded, ListNamesError};
use khive_fs::tree_walk::{
    walk_tree, RelPath, WalkEntryKind, WalkError, WalkLimits, TREE_LINK_BUDGET,
};

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

fn chain(scratch: &Scratch, count: usize) {
    scratch.file("chain/payload");
    symlink("chain/0", scratch.path().join("entry")).unwrap();
    for index in 0..count - 1 {
        let target = if index + 1 == count - 1 {
            "payload".into()
        } else {
            (index + 1).to_string()
        };
        symlink(target, scratch.path().join(format!("chain/{index}"))).unwrap();
    }
}

#[test]
fn link_chain_budget_is_exact_and_pure_cycles_refuse() {
    for count in [TREE_LINK_BUDGET, TREE_LINK_BUDGET + 1] {
        let scratch = Scratch::new();
        chain(&scratch, count);
        let result = walk_tree(scratch.path(), limits(0, 2, true), |path, _| {
            path.as_path() != Path::new("chain")
        });
        if count == TREE_LINK_BUDGET {
            assert_eq!(paths(result.unwrap()), expected(&["entry"]));
        } else {
            assert!(matches!(
                result,
                Err(WalkError::LinkBudgetExceeded {
                    max_links: TREE_LINK_BUDGET,
                    ..
                })
            ));
        }
    }
    let scratch = Scratch::new();
    symlink("b", scratch.path().join("a")).unwrap();
    symlink("a", scratch.path().join("b")).unwrap();
    assert!(matches!(
        walk_tree(scratch.path(), limits(1, 2, true), |_, _| true),
        Err(WalkError::LinkBudgetExceeded { .. })
    ));
}

#[test]
fn link_budget_is_global_across_separate_entries() {
    let scratch = Scratch::new();
    scratch.file("payload");
    for index in 0..TREE_LINK_BUDGET {
        symlink("payload", scratch.path().join(format!("link{index:02}"))).unwrap();
    }
    assert_eq!(
        walk_tree(scratch.path(), limits(0, 100, true), |_, _| true)
            .unwrap()
            .len(),
        TREE_LINK_BUDGET + 1
    );
    symlink("payload", scratch.path().join("one-more")).unwrap();
    assert!(matches!(
        walk_tree(scratch.path(), limits(0, 100, true), |_, _| true),
        Err(WalkError::LinkBudgetExceeded { .. })
    ));
}

#[test]
fn renaming_root_and_replacing_its_path_cannot_redirect_relative_descendants() {
    let scratch = Scratch::new();
    scratch.file("root/a-trigger");
    scratch.file("root/b-dir/original");
    scratch.file("outside/b-dir/wrong");
    let root = scratch.path().join("root");
    symlink("b-dir/original", root.join("c-relative")).unwrap();
    let result = walk_tree(&root, limits(2, 10, true), |path, _| {
        if path.as_path() == Path::new("a-trigger") {
            fs::rename(&root, scratch.path().join("moved")).unwrap();
            symlink(scratch.path().join("outside"), &root).unwrap();
        }
        true
    })
    .unwrap();
    assert_eq!(
        paths(result),
        expected(&["a-trigger", "b-dir/original", "c-relative"])
    );
}

#[test]
fn absolute_link_through_replaced_root_refuses_identity_mismatch() {
    let scratch = Scratch::new();
    scratch.file("root/a-trigger");
    scratch.file("root/payload");
    scratch.file("replacement/payload");
    let root = scratch.path().join("root");
    symlink(root.join("payload"), root.join("z-absolute")).unwrap();
    let result = walk_tree(&root, limits(1, 10, true), |path, _| {
        if path.as_path() == Path::new("a-trigger") {
            fs::rename(&root, scratch.path().join("moved")).unwrap();
            fs::rename(scratch.path().join("replacement"), &root).unwrap();
        }
        true
    });
    assert!(matches!(
        result,
        Err(WalkError::RootChanged { source: None, .. })
    ));
}

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
        symlink(&target, scratch.path().join("link")).unwrap();
        assert!(
            matches!(walk_tree(scratch.path(), limits(1, 2, true), |_, _| true),
            Err(WalkError::Io { source, .. }) if source.raw_os_error() == Some(libc::ENOTDIR))
        );
        fs::remove_file(scratch.path().join("link")).unwrap();
    }
}
