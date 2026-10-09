//! Tests for the configured-root walk and the link policy it runs under.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use tempfile::TempDir;

use super::*;
use crate::mirror::ingest::file_identity;

#[test]
fn source_link_length_preserves_non_utf8_bytes_and_nonlink_errno() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let temp = TempDir::new().expect("fixture directory");
    let target = OsString::from_vec(vec![b'a', 0xff, b'/', 0xc3, 0xa9]);
    std::os::unix::fs::symlink(&target, temp.path().join("link")).expect("link");
    let parent = File::open(temp.path()).expect("parent");
    assert_eq!(read_link_length(&parent, OsStr::new("link")).unwrap(), 5);

    std::fs::write(temp.path().join("ordinary"), b"file").expect("ordinary file");
    let error =
        read_link_length(&parent, OsStr::new("ordinary")).expect_err("ordinary file is not a link");
    assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
}

fn shared_refusal(error: &std::io::Error) -> &AncestorLinkRefusal {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref())
        .expect("shared ancestor policy refusal")
}

fn trusted_fixture_parent(path: &Path) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/bin/chmod")
            .arg("-N")
            .arg(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

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
/// uses the shared ancestor policy; this policy isolates generic handle/budget behavior.
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
fn policy_refuses_a_foreign_link_before_it_checks_the_parent() {
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
    let mut policy = MirrorAncestorLinks::new();

    let effective_uid = unsafe { libc::geteuid() };
    stat.st_uid = if effective_uid == 1 { 2 } else { 1 };
    let context = walk_context(&parent, name, stat);
    let refusal = policy.before_follow(&context).expect_err("a foreign link");
    assert_eq!(refusal.kind(), std::io::ErrorKind::Other);
    assert_eq!(
        shared_refusal(&refusal).condition,
        AncestorLinkCondition::LinkOwner
    );

    stat.st_uid = 0;
    let context = walk_context(&parent, name, stat);
    let refusal = policy.before_follow(&context).expect_err("a root link");
    assert_eq!(refusal.kind(), std::io::ErrorKind::Other);
    assert_eq!(
        shared_refusal(&refusal).condition,
        AncestorLinkCondition::ParentPermissions
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
    let mut policy = MirrorAncestorLinks::new();

    policy.after_read(&context).expect("an unchanged link");

    // Renaming a second link over the first gives it another inode: both exist at once.
    let replacement = fixture.join("replacement");
    std::os::unix::fs::symlink("second", &replacement).expect("replacement link");
    std::fs::rename(&replacement, &link).expect("replace link");
    let refusal = policy.after_read(&context).expect_err("a replaced link");
    assert_eq!(
        shared_refusal(&refusal).condition,
        AncestorLinkCondition::LinkChanged
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

#[test]
fn actual_mirror_root_accepts_current_user_ancestry_and_denies_unsafe_parent() {
    let (_temp, fixture) = physical_fixture();
    trusted_fixture_parent(&fixture);
    std::fs::create_dir_all(fixture.join("real/inner")).unwrap();
    std::os::unix::fs::symlink("real", fixture.join("link")).unwrap();
    let root = fixture.join("link/inner");
    let handles = open_source_root(&root).expect("shared policy permits the current-user ancestor");
    let physical = File::open(fixture.join("real/inner")).unwrap();
    assert_eq!(
        file_identity(handles.last().unwrap()).unwrap(),
        file_identity(&physical).unwrap()
    );
    println!("mirror ancestor policy current user: ALLOW");

    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o777)).unwrap();
    let error = open_source_root(&root).expect_err("nonsticky writable parent must deny");
    assert_eq!(
        shared_refusal(&error).condition,
        AncestorLinkCondition::ParentPermissions
    );
    assert_eq!(shared_refusal(&error).parent_mode.unwrap() & 0o7777, 0o777);
    println!("mirror ancestor policy unsafe parent: DENY parent_permissions");
}

#[test]
fn invalid_input_typed_policy_refusal_is_not_rewritten_as_a_nul_path() {
    let (_temp, fixture) = physical_fixture();
    std::os::unix::fs::symlink("missing", fixture.join("link")).unwrap();
    let parent = File::open(&fixture).unwrap();
    let name = OsStr::new("link");
    let mut stat = stat_at(&parent, name).unwrap();
    let effective_uid = unsafe { libc::geteuid() };
    stat.st_uid = if effective_uid == 1 { 2 } else { 1 };
    let context = walk_context(&parent, name, stat);
    let error = AncestorLinkPolicy::new(AncestorWalkEndpoint::FinalTarget)
        .before_follow(&context)
        .expect_err("actual shared evaluator refuses a foreign owner");
    let error = std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        error.into_inner().expect("typed policy cause"),
    );
    let error = root_walk_error(error);
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        shared_refusal(&error).condition,
        AncestorLinkCondition::LinkOwner
    );
    assert!(error.to_string().contains("link_owner"));
    assert!(!error.to_string().contains("NUL"));
}

#[cfg(target_os = "macos")]
#[test]
fn actual_mirror_root_denies_acl_grant_with_unchanged_mode() {
    use std::os::unix::fs::MetadataExt as _;
    let (_temp, fixture) = physical_fixture();
    trusted_fixture_parent(&fixture);
    std::fs::create_dir_all(fixture.join("real/inner")).unwrap();
    std::os::unix::fs::symlink("real", fixture.join("link")).unwrap();
    let root = fixture.join("link/inner");
    open_source_root(&root).expect("no ACL grants");
    let mode = std::fs::metadata(&fixture).unwrap().mode();
    let output = std::process::Command::new("/bin/chmod")
        .args(["+a", "everyone allow add_file,delete_child"])
        .arg(&fixture)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::metadata(&fixture).unwrap().mode(), mode);
    let error =
        open_source_root(&root).expect_err("shared ACL grant condition must deny the root walk");
    assert_eq!(
        shared_refusal(&error).condition,
        AncestorLinkCondition::ParentAclGrant
    );
    println!("mirror actual macOS ACL grant: DENY parent_acl_grant");
}
