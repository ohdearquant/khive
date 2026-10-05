use super::*;
use crate::code_map_vfs::OpenAccess;

#[test]
fn deleted_members_do_not_deny_new_files_with_recycled_identities() {
    if run_in_child() {
        return;
    }
    let dir = fixture();
    for index in 0..4_096 {
        let target = dir.path().join(format!("recycled-{index}.db"));
        let guard = CodeMapHandleGuard::new(target.clone(), Mode::Rollback, Vec::new()).unwrap();
        let journal = guard
            .open(Role::Journal, OpenAccess::CreateNew)
            .unwrap_or_else(|error| panic!("fresh journal {index} must be admitted: {error}"));
        drop(journal);
        guard.delete(Role::Journal, false).unwrap();
        assert!(!companion(&target, "-journal").exists());

        let main = guard
            .open(Role::Main, OpenAccess::CreateNew)
            .unwrap_or_else(|error| panic!("fresh main {index} must be admitted: {error}"));
        drop(main);
        std::fs::remove_file(&target).unwrap();
    }
    assert_eq!(crate::code_map_vfs::quarantine_occupancy(), 0);
    #[cfg(unix)]
    assert!(
        crate::code_map_vfs::lock_ledger().guarded.is_empty(),
        "deleted objects must not accumulate retained descriptors"
    );
}

#[test]
fn renamed_closed_member_keeps_its_guarded_role() {
    if run_in_child() {
        return;
    }
    let dir = fixture();
    let original = dir.path().join("original.db");
    let first = CodeMapHandleGuard::new(original.clone(), Mode::Rollback, Vec::new()).unwrap();
    drop(first.open(Role::Main, OpenAccess::CreateNew).unwrap());

    let target = dir.path().join("other.db");
    let journal = companion(&target, "-journal");
    std::fs::rename(&original, &journal).unwrap();
    let second = CodeMapHandleGuard::new(target, Mode::Rollback, Vec::new()).unwrap();
    let error = second
        .open(Role::Journal, OpenAccess::ReadWrite)
        .expect_err("the same retained physical file cannot acquire a different role");
    assert!(
        error
            .to_string()
            .contains("aliases another guarded file role"),
        "{error}"
    );
    assert!(journal.exists());
}

#[cfg(unix)]
#[test]
fn surviving_hardlink_keeps_the_admitted_objects_role() {
    if run_in_child() {
        return;
    }
    let dir = fixture();
    let original = dir.path().join("linked-original.db");
    let first = CodeMapHandleGuard::new(original.clone(), Mode::Rollback, Vec::new()).unwrap();
    drop(first.open(Role::Main, OpenAccess::CreateNew).unwrap());

    let target = dir.path().join("linked-other.db");
    let journal = companion(&target, "-journal");
    std::fs::hard_link(&original, &journal).unwrap();
    std::fs::remove_file(&original).unwrap();
    let second = CodeMapHandleGuard::new(target, Mode::Rollback, Vec::new()).unwrap();
    let error = second
        .open(Role::Journal, OpenAccess::ReadWrite)
        .expect_err("a surviving name must retain the original admitted role");
    assert!(
        error
            .to_string()
            .contains("aliases another guarded file role"),
        "{error}"
    );
    assert!(journal.exists());
    assert_eq!(crate::code_map_vfs::quarantine_occupancy(), 0);
}
