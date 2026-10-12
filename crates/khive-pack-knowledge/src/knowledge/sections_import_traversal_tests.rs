use super::{collect_md_files_with_limits, ImportTraversalLimits};
use tempfile::TempDir;

#[test]
fn traversal_surfaces_root_read_errors() {
    let root = TempDir::new().expect("temp root");
    let missing = root.path().join("missing");
    let error = collect_md_files_with_limits(
        &missing,
        ImportTraversalLimits {
            max_depth: 8,
            max_entries: 100,
            max_files: 100,
        },
    )
    .expect_err("missing traversal root must be surfaced");
    assert!(error.to_string().contains("traversal failed"));
}

#[test]
fn traversal_enforces_depth_limit() {
    let root = TempDir::new().expect("temp root");
    let nested = root.path().join("nested");
    std::fs::create_dir_all(&nested).expect("nested directory fixture");
    let error = collect_md_files_with_limits(
        root.path(),
        ImportTraversalLimits {
            max_depth: 0,
            max_entries: 100,
            max_files: 100,
        },
    )
    .expect_err("depth over limit must fail closed");
    let message = error.to_string();
    assert!(message.contains("depth limit"), "{message}");
    assert!(
        message.contains(nested.to_string_lossy().as_ref()),
        "{message}"
    );
    assert!(message.contains("depth=1"), "{message}");
    assert!(message.contains("max_depth=0"), "{message}");
    assert!(message.contains("entries_visited=1"), "{message}");
    assert!(message.contains("max_entries=100"), "{message}");
    assert!(message.contains("files_discovered=0"), "{message}");
    assert!(message.contains("max_files=100"), "{message}");
}

#[test]
fn traversal_enforces_entry_limit() {
    let root = TempDir::new().expect("temp root");
    let first = root.path().join("a.md");
    std::fs::write(&first, "a").expect("fixture");
    let error = collect_md_files_with_limits(
        root.path(),
        ImportTraversalLimits {
            max_depth: 8,
            max_entries: 0,
            max_files: 100,
        },
    )
    .expect_err("entry over limit must fail closed");
    let message = error.to_string();
    assert!(message.contains("entry limit"), "{message}");
    assert!(
        message.contains(first.to_string_lossy().as_ref()),
        "{message}"
    );
    assert!(message.contains("entries_visited=1"), "{message}");
    assert!(message.contains("max_entries=0"), "{message}");
    assert!(message.contains("files_discovered=0"), "{message}");
    assert!(message.contains("max_files=100"), "{message}");
    assert!(message.contains("depth=0"), "{message}");
    assert!(message.contains("max_depth=8"), "{message}");
}

#[test]
fn traversal_enforces_markdown_file_limit() {
    let root = TempDir::new().expect("temp root");
    std::fs::write(root.path().join("a.md"), "a").expect("first fixture");
    let second = root.path().join("b.md");
    std::fs::write(&second, "b").expect("second fixture");
    let error = collect_md_files_with_limits(
        root.path(),
        ImportTraversalLimits {
            max_depth: 8,
            max_entries: 100,
            max_files: 1,
        },
    )
    .expect_err("file over limit must fail closed");
    let message = error.to_string();
    assert!(message.contains("markdown file limit"), "{message}");
    assert!(
        message.contains(second.to_string_lossy().as_ref()),
        "{message}"
    );
    assert!(message.contains("files_discovered=2"), "{message}");
    assert!(message.contains("max_files=1"), "{message}");
    assert!(message.contains("entries_visited=2"), "{message}");
    assert!(message.contains("max_entries=100"), "{message}");
    assert!(message.contains("depth=0"), "{message}");
    assert!(message.contains("max_depth=8"), "{message}");
}

#[cfg(unix)]
#[test]
fn traversal_does_not_follow_symlinks() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().expect("temp root");
    let markdown = root.path().join("source.md");
    std::fs::write(&markdown, "source").expect("source fixture");
    symlink(&markdown, root.path().join("alias.md")).expect("symlink fixture");
    let discovery = collect_md_files_with_limits(
        root.path(),
        ImportTraversalLimits {
            max_depth: 8,
            max_entries: 100,
            max_files: 100,
        },
    )
    .expect("traversal");

    assert_eq!(discovery.files, vec![markdown]);
    assert_eq!(discovery.files_skipped, 1);
}
