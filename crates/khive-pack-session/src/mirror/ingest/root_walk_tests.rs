//! Tests for the configured-root walk and the link policy it runs under.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use tempfile::TempDir;

use super::*;
use crate::mirror::ingest::file_identity;

/// A physical fixture directory; the returned guard removes it when the test ends.
fn physical_fixture() -> (TempDir, PathBuf) {
    let temp = TempDir::new().expect("fixture directory");
    let fixture = std::fs::canonicalize(temp.path()).expect("physical fixture directory");
    (temp, fixture)
}

/// The directories a physical absolute path names, from `/` down to the path itself.
fn physical_prefixes(path: &Path) -> Vec<PathBuf> {
    let mut prefixes = vec![PathBuf::from("/")];
    for component in path.components().skip(1) {
        let parent = prefixes.last().expect("the anchor is always present");
        let next = parent.join(component.as_os_str());
        prefixes.push(next);
    }
    prefixes
}

/// The handles pin one directory per prefix of `path`, the anchor first and `path` last.
fn assert_handles_pin_the_directories_of(handles: &[File], path: &Path) {
    let prefixes = physical_prefixes(path);
    assert_eq!(handles.len(), prefixes.len());
    for (handle, prefix) in handles.iter().zip(&prefixes) {
        let pinned = file_identity(handle).expect("handle identity");
        let directory = File::open(prefix).expect("open prefix directory");
        let named = file_identity(&directory).expect("prefix identity");
        assert_eq!(pinned, named, "{prefix:?}");
    }
}

/// Follows every link and counts the links the walk offers it. The mirror's own policy follows
/// only links that root owns, which a test run by another user cannot create, so the walk's
/// handles and its budget are exercised under this policy over links the test builds itself.
struct FollowEveryLink {
    offered: u32,
}

impl LinkPolicy for FollowEveryLink {
    fn before_follow(&mut self, _ctx: &LinkContext<'_>) -> std::io::Result<()> {
        self.offered += 1;
        Ok(())
    }
}

/// Builds `link1` through `link<count>` in `fixture`, each a relative link to the next, the last
/// one to the directory `real`, and returns the root `link1/inner` that resolves to `real/inner`.
fn root_behind_a_link_chain(fixture: &Path, count: u32) -> PathBuf {
    std::fs::create_dir_all(fixture.join("real").join("inner")).expect("real directories");
    for index in 1..=count {
        let target = if index == count {
            String::from("real")
        } else {
            format!("link{}", index + 1)
        };
        let link = fixture.join(format!("link{index}"));
        std::os::unix::fs::symlink(target, link).expect("chain link");
    }
    fixture.join("link1").join("inner")
}

/// A link as the walk shows it to a policy: not the last component, with the given metadata.
fn walk_context<'a>(parent: &'a File, name: &'a OsStr, stat: libc::stat) -> LinkContext<'a> {
    LinkContext {
        name,
        is_last: false,
        link_stat: stat,
        parent,
    }
}

#[test]
fn plain_root_pins_the_anchor_and_every_component_in_order() {
    let (_temp, fixture) = physical_fixture();
    let root = fixture.join("alpha").join("beta");
    std::fs::create_dir_all(&root).expect("root directories");

    let handles = open_source_root(&root).expect("plain root");

    assert_handles_pin_the_directories_of(&handles, &root);
}

#[test]
fn a_root_behind_eight_followed_links_pins_the_resolved_directories() {
    let (_temp, fixture) = physical_fixture();
    let root = root_behind_a_link_chain(&fixture, 8);
    let physical = fixture.join("real").join("inner");
    let mut policy = FollowEveryLink { offered: 0 };

    let handles = open_root_with(&root, &mut policy).expect("root behind eight links");

    assert_eq!(policy.offered, 8);
    // Relative links add no handle of their own, so the handles are those of the physical path.
    assert_handles_pin_the_directories_of(&handles, &physical);
}

#[test]
fn a_ninth_link_exhausts_the_ancestor_symlink_budget_with_its_message() {
    let (_temp, fixture) = physical_fixture();
    let root = root_behind_a_link_chain(&fixture, 9);
    let mut policy = FollowEveryLink { offered: 0 };

    let refusal = open_root_with(&root, &mut policy).expect_err("a root behind nine links");

    // The walk offers the ninth link to the policy, then finds the budget of eight spent.
    assert_eq!(policy.offered, 9);
    assert_eq!(refusal.kind(), std::io::ErrorKind::Other);
    let message = refusal.to_string();
    assert_eq!(
        message,
        "mirror source root exceeds the ancestor symlink limit"
    );
}

#[test]
fn a_link_as_the_final_root_component_reports_the_kernel_refusal() {
    let (_temp, fixture) = physical_fixture();
    let real = fixture.join("real");
    let linked = fixture.join("linked");
    std::fs::create_dir(&real).expect("real root");
    std::os::unix::fs::symlink(&real, &linked).expect("root link");

    let refusal = open_source_root(&linked).expect_err("a linked root");

    let parent = File::open(&fixture).expect("fixture handle");
    let name = OsStr::new("linked");
    let kernel = open_dir_at(&parent, name).expect_err("kernel refusal");
    assert!(kernel.raw_os_error().is_some());
    assert_eq!(refusal.raw_os_error(), kernel.raw_os_error());
    assert_eq!(refusal.to_string(), kernel.to_string());
}

#[test]
fn policy_refuses_a_non_root_link_before_it_checks_the_parent() {
    let (_temp, fixture) = physical_fixture();
    // Writable by group and others without the sticky bit: no owner makes this parent trusted.
    let open_parent = fixture.join("open");
    std::fs::create_dir(&open_parent).expect("parent directory");
    let permissions = std::fs::Permissions::from_mode(0o777);
    std::fs::set_permissions(&open_parent, permissions).expect("open permissions");
    std::os::unix::fs::symlink("elsewhere", open_parent.join("link")).expect("link");
    let parent = File::open(&open_parent).expect("parent handle");
    let name = OsStr::new("link");
    let mut stat = stat_at(&parent, name).expect("link metadata");
    let mut policy = RootOwnedAncestorLinks;

    stat.st_uid = 1;
    let context = walk_context(&parent, name, stat);
    let refusal = policy.before_follow(&context).expect_err("a non-root link");
    assert_eq!(refusal.kind(), std::io::ErrorKind::Other);
    let message = refusal.to_string();
    assert_eq!(
        message,
        "mirror source root has a non-root-owned ancestor symlink"
    );

    stat.st_uid = 0;
    let context = walk_context(&parent, name, stat);
    let refusal = policy.before_follow(&context).expect_err("a root link");
    assert_eq!(refusal.kind(), std::io::ErrorKind::Other);
    let message = refusal.to_string();
    assert_eq!(
        message,
        "mirror source root ancestor symlink parent permits non-root entry replacement"
    );
}

#[test]
fn after_read_refuses_a_link_replaced_while_it_resolved() {
    let (_temp, fixture) = physical_fixture();
    let link = fixture.join("link");
    std::os::unix::fs::symlink("first", &link).expect("link");
    let parent = File::open(&fixture).expect("parent handle");
    let name = OsStr::new("link");
    let stat = stat_at(&parent, name).expect("link metadata");
    let context = walk_context(&parent, name, stat);
    let mut policy = RootOwnedAncestorLinks;

    policy.after_read(&context).expect("an unchanged link");

    // Renaming a second link over the first gives it another inode: both exist at once.
    let replacement = fixture.join("replacement");
    std::os::unix::fs::symlink("second", &replacement).expect("replacement link");
    std::fs::rename(&replacement, &link).expect("replace link");
    let refusal = policy.after_read(&context).expect_err("a replaced link");
    let message = refusal.to_string();
    assert_eq!(
        message,
        "mirror source root ancestor symlink changed while resolving"
    );
}

#[test]
fn a_root_component_with_a_nul_byte_keeps_its_message() {
    let (_temp, fixture) = physical_fixture();
    let root = fixture.join(OsStr::from_bytes(b"before\0after"));

    let refusal = open_source_root(&root).expect_err("a root with a NUL byte");

    assert_eq!(refusal.kind(), std::io::ErrorKind::InvalidInput);
    let message = refusal.to_string();
    assert_eq!(message, "mirror source root contains a NUL byte");
}
