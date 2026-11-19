//! Cache directory handles preserve ancestor resolution and refuse final symlinks.
use super::*;

#[test]
fn owned_slot_operations_accept_a_symlinked_writable_ancestor() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("shared-parent");
    let root = parent.join("cache");
    let name = "aaaaaaaaaaaaaaaa";
    let owned = root.join(name);
    std::fs::create_dir_all(owned.join(".git")).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o770)).unwrap();
    std::fs::write(owned.join(MARKER_FILE), b"").unwrap();
    std::fs::write(owned.join("payload.txt"), b"owned clone").unwrap();
    let alias = dir.path().join("parent-alias");
    symlink(&parent, &alias).unwrap();
    let alias_root = alias.join("cache");
    let alias_slot = alias_root.join(name);

    revalidate_owned_slot(&alias_slot).expect("ancestor links and writable parents stay accepted");
    remove_owned_entry(&alias_root, &alias_slot).expect("remove through the same ancestor alias");

    assert!(!owned.exists());
    assert!(alias.is_symlink());
    assert!(root.is_dir());
}

#[test]
fn owned_slot_deletion_refuses_a_symlinked_root_final_component() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("cache");
    let name = "bbbbbbbbbbbbbbbb";
    let owned = root.join(name);
    std::fs::create_dir_all(owned.join(".git")).unwrap();
    std::fs::write(owned.join(MARKER_FILE), b"").unwrap();
    std::fs::write(owned.join("payload.txt"), b"keep owned clone").unwrap();
    let alias = dir.path().join("cache-alias");
    std::os::unix::fs::symlink(&root, &alias).unwrap();

    let err = delete_verified_owned_entry(&alias, &alias.join(name))
        .expect_err("the root itself must not be followed");

    assert!(matches!(err, CacheError::Io(_)), "{err:?}");
    assert!(alias.is_symlink());
    assert_eq!(
        std::fs::read(owned.join("payload.txt")).unwrap(),
        b"keep owned clone"
    );
    assert!(!root.join(STAGING_NAMESPACE).exists());
}

/// The link points at a fully owned slot (`.git` directory plus ownership marker), so the
/// ownership re-check cannot be what turns the deletion away: only the directory open can.
/// A refusal at the open leaves the link at the cache-key name, while a deletion that followed
/// the link would already have moved it into the staging namespace before noticing.
#[test]
fn delete_verified_owned_entry_leaves_a_symlink_to_an_owned_slot_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let target = tempfile::tempdir().expect("symlink target");
    let owned = target.path().join("owned-slot");
    std::fs::create_dir_all(owned.join(".git")).unwrap();
    std::fs::write(owned.join(MARKER_FILE), b"").unwrap();
    std::fs::write(owned.join("payload.txt"), b"do not delete me").unwrap();

    let link = root.join("cccccccccccccccc");
    std::os::unix::fs::symlink(&owned, &link).expect("plant symlink");

    let err = delete_verified_owned_entry(root, &link)
        .expect_err("a symlink at the cache-key path must be refused");
    assert!(
        matches!(err, CacheError::UnsafeToReplace(_)),
        "expected UnsafeToReplace, got {err:?}"
    );
    assert!(
        link.is_symlink(),
        "a refused deletion must leave the symlink at the cache-key path"
    );
    assert!(
        owned.join("payload.txt").exists(),
        "the symlink target's contents must survive a refused deletion"
    );
}
