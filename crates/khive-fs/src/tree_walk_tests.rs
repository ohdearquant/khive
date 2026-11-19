use super::*;
use std::fs;

#[test]
fn dot_root_pins_the_current_directory() {
    let root = Root::open(Path::new(".")).unwrap();
    let current = File::open(".").unwrap();
    assert_eq!(root.identity, identity(&stat_fd(&current).unwrap()));
}

#[test]
fn checked_open_used_for_link_intermediates_refuses_a_replaced_directory() {
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let scratch = Scratch(
        std::env::temp_dir().join(format!("khive-tree-intermediate-{}", std::process::id())),
    );
    fs::create_dir(&scratch.0).unwrap();
    fs::create_dir(scratch.0.join("child")).unwrap();
    fs::create_dir(scratch.0.join("replacement")).unwrap();
    let parent = File::open(&scratch.0).unwrap();
    let name = OsStr::new("child");
    let path = RelPath(PathBuf::from("alias/file"));
    let before = stat_at(&parent, name).unwrap();
    let accepted = open_checked(&parent, name, &before, &path).unwrap();
    assert_eq!(identity(&stat_fd(&accepted).unwrap()), identity(&before));
    fs::rename(scratch.0.join("child"), scratch.0.join("original")).unwrap();
    fs::rename(scratch.0.join("replacement"), scratch.0.join("child")).unwrap();
    assert!(matches!(open_checked(&parent, name, &before, &path),
            Err(WalkError::EntryChanged { path: changed }) if changed == path));
    assert_eq!(identity(&stat_fd(&accepted).unwrap()), identity(&before));
}
