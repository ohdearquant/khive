//! Behavior of the opened-file helpers against real directories.

#![cfg(unix)]

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use khive_fs::opened_file::{open_regular_file_within, opened_file_path, ContainedOpenError};

/// A scratch directory under the system temporary directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let name = format!("khive-fs-{label}-{}", std::process::id());
        let path = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn opened_file_path_reports_the_resolved_path_through_a_symlinked_directory() {
    let scratch = Scratch::new("resolved-path");
    let real = scratch.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("lib.rs"), b"data").unwrap();
    symlink(&real, scratch.path().join("link")).unwrap();

    let file = File::open(scratch.path().join("link").join("lib.rs")).unwrap();

    let resolved = real.join("lib.rs").canonicalize().unwrap();
    assert_eq!(opened_file_path(&file).unwrap(), resolved);
}

#[cfg(target_vendor = "apple")]
#[test]
fn opened_file_path_reports_a_renamed_open_directory_on_apple() {
    let scratch = Scratch::new("renamed-open-directory");
    let root = scratch.path().canonicalize().unwrap();
    let original = root.join("original");
    let renamed = root.join("renamed");
    std::fs::create_dir(&original).unwrap();
    let directory = File::open(&original).unwrap();

    std::fs::rename(&original, &renamed).unwrap();
    std::fs::create_dir(&original).unwrap();

    assert_eq!(opened_file_path(&directory).unwrap(), renamed);
}

#[test]
fn open_regular_file_within_refuses_a_source_swapped_to_an_outside_symlink() {
    let scratch = Scratch::new("swapped-source");
    let root = scratch.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let path = root.join("lib.rs");
    std::fs::write(&path, b"inside").unwrap();
    let outside = scratch.path().join("outside.rs");
    std::fs::write(&outside, b"outside").unwrap();

    let canonical_root = root.canonicalize().unwrap();
    let checked = path.canonicalize().unwrap();
    assert!(checked.starts_with(&canonical_root));
    std::fs::remove_file(&path).unwrap();
    symlink(&outside, &path).unwrap();

    match open_regular_file_within(&canonical_root, &path) {
        Err(ContainedOpenError::Escapes { opened }) => {
            assert_eq!(opened, outside.canonicalize().unwrap());
        }
        other => panic!("expected an escape refusal, got {other:?}"),
    }
}

#[test]
fn open_regular_file_within_reads_an_alias_that_stays_inside_the_root() {
    let scratch = Scratch::new("inside-alias");
    let path = scratch.path().join("lib.rs");
    std::fs::write(&path, b"inside").unwrap();
    let alias = scratch.path().join("alias.rs");
    symlink(&path, &alias).unwrap();
    let canonical_root = scratch.path().canonicalize().unwrap();

    for source in [&path, &alias] {
        let mut file = open_regular_file_within(&canonical_root, source).unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "inside");
    }
}
