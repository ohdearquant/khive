//! The descriptor open under owned-slot deletion refuses a symlinked cache-key name.
use super::*;

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
