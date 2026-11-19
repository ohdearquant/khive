use super::*;

const EFFECTIVE_UID: u32 = 42;
const FOREIGN_UID: u32 = 99;

fn decision(
    link_uid: u32,
    parent_uid: u32,
    mode: u32,
    grants: io::Result<bool>,
) -> Result<(), Failure> {
    evaluate_before(
        AncestorWalkEndpoint::FinalTarget,
        false,
        link_uid,
        EFFECTIVE_UID,
        || {
            Ok(ParentFacts {
                uid: parent_uid,
                mode,
            })
        },
        || grants,
    )
}

fn assert_decision(
    label: &str,
    result: Result<(), Failure>,
    expected: Option<AncestorLinkCondition>,
) {
    match (result, expected) {
        (Ok(()), None) => println!("ancestor-policy {label}: ALLOW"),
        (Err(failure), Some(condition)) => {
            assert_eq!(failure.condition, condition, "{label}");
            println!("ancestor-policy {label}: DENY {condition}");
        }
        (result, expected) => panic!("{label}: result {result:?}, expected {expected:?}"),
    }
}

#[test]
fn root_and_effective_uid_link_parent_hot_cold_pairs() {
    use AncestorLinkCondition::{LinkOwner, ParentOwner};
    for (label, link_uid, parent_uid, expected) in [
        ("root-link/root-parent", 0, 0, None),
        ("effective-link/root-parent", EFFECTIVE_UID, 0, None),
        ("root-link/effective-parent", 0, EFFECTIVE_UID, None),
        (
            "effective-link/effective-parent",
            EFFECTIVE_UID,
            EFFECTIVE_UID,
            None,
        ),
        (
            "foreign-link/effective-parent",
            FOREIGN_UID,
            EFFECTIVE_UID,
            Some(LinkOwner),
        ),
        (
            "effective-link/foreign-parent",
            EFFECTIVE_UID,
            FOREIGN_UID,
            Some(ParentOwner),
        ),
    ] {
        assert_decision(
            label,
            decision(link_uid, parent_uid, 0o755, Ok(false)),
            expected,
        );
    }
}

#[test]
fn foreign_link_and_parent_owners_must_deny() {
    assert_decision(
        "must-DENY foreign link",
        decision(FOREIGN_UID, EFFECTIVE_UID, 0o700, Ok(false)),
        Some(AncestorLinkCondition::LinkOwner),
    );
    assert_decision(
        "must-DENY foreign parent",
        decision(EFFECTIVE_UID, FOREIGN_UID, 0o700, Ok(false)),
        Some(AncestorLinkCondition::ParentOwner),
    );
}

#[test]
fn sticky_parent_permissions_hot_cold_pairs() {
    for (label, mode, expected) in [
        ("protected", 0o755, None),
        ("sticky-group-write", 0o1775, None),
        ("sticky-other-write", 0o1757, None),
        ("sticky-world-write", 0o1777, None),
        (
            "nonsticky-group-write",
            0o775,
            Some(AncestorLinkCondition::ParentPermissions),
        ),
        (
            "nonsticky-other-write",
            0o757,
            Some(AncestorLinkCondition::ParentPermissions),
        ),
        (
            "must-DENY nonsticky-world-write",
            0o777,
            Some(AncestorLinkCondition::ParentPermissions),
        ),
    ] {
        assert_decision(
            label,
            decision(EFFECTIVE_UID, EFFECTIVE_UID, mode, Ok(false)),
            expected,
        );
    }
}

#[test]
fn acl_grants_hot_cold_pairs_and_unknown_must_deny() {
    assert_decision(
        "no ACL",
        decision(EFFECTIVE_UID, EFFECTIVE_UID, 0o755, Ok(false)),
        None,
    );
    for (label, tag, mask, expected) in [
        ("deny-only ACL", 2, u64::MAX, None),
        ("allow entry with no rights", 1, 0, None),
        (
            "allow read right",
            1,
            1 << 1,
            Some(AncestorLinkCondition::ParentAclGrant),
        ),
        (
            "must-DENY high permission bit",
            1,
            1 << 40,
            Some(AncestorLinkCondition::ParentAclGrant),
        ),
        (
            "must-DENY unknown tag",
            17,
            0,
            Some(AncestorLinkCondition::ParentAclWitness),
        ),
    ] {
        assert_decision(
            label,
            decision(
                EFFECTIVE_UID,
                EFFECTIVE_UID,
                0o755,
                acl_entry_grants(tag, mask),
            ),
            expected,
        );
    }
    assert_decision(
        "must-DENY unreadable ACL",
        decision(
            EFFECTIVE_UID,
            EFFECTIVE_UID,
            0o755,
            Err(io::Error::from_raw_os_error(libc::EBADF)),
        ),
        Some(AncestorLinkCondition::ParentAclWitness),
    );
}

#[test]
fn darwin_acl_iterator_does_not_use_posix_status_conventions() {
    assert!(darwin_entry_present(0, io::Error::from_raw_os_error(libc::EINVAL)).unwrap());
    assert!(!darwin_entry_present(-1, io::Error::from_raw_os_error(libc::EINVAL)).unwrap());
    let failure = darwin_entry_present(-1, io::Error::from_raw_os_error(libc::EBADF)).unwrap_err();
    assert_eq!(failure.raw_os_error(), Some(libc::EBADF));
    assert_eq!(
        darwin_entry_present(1, io::Error::from_raw_os_error(libc::EINVAL))
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData,
        "a POSIX-style success value is an unknown Darwin witness, not an empty ACL"
    );
}

#[test]
fn final_target_and_target_parent_are_distinct() {
    let refusal = evaluate_before(
        AncestorWalkEndpoint::FinalTarget,
        true,
        EFFECTIVE_UID,
        EFFECTIVE_UID,
        || panic!("final target must refuse before parent inspection"),
        || panic!("final target must refuse before ACL inspection"),
    )
    .unwrap_err();
    assert_eq!(refusal.condition, AncestorLinkCondition::FinalComponent);
    evaluate_before(
        AncestorWalkEndpoint::TargetParent,
        true,
        EFFECTIVE_UID,
        EFFECTIVE_UID,
        || {
            Ok(ParentFacts {
                uid: EFFECTIVE_UID,
                mode: 0o755,
            })
        },
        || Ok(false),
    )
    .unwrap();
}

#[test]
fn metadata_and_acl_witness_failures_keep_sources() {
    let parent_failure = evaluate_before(
        AncestorWalkEndpoint::FinalTarget,
        false,
        EFFECTIVE_UID,
        EFFECTIVE_UID,
        || Err(io::Error::from_raw_os_error(libc::EBADF)),
        || panic!("failed metadata must not become a passing ACL witness"),
    )
    .unwrap_err();
    assert_eq!(
        parent_failure.condition,
        AncestorLinkCondition::ParentMetadata
    );
    assert_eq!(
        parent_failure.source.unwrap().raw_os_error(),
        Some(libc::EBADF)
    );
    let failure = decision(
        EFFECTIVE_UID,
        EFFECTIVE_UID,
        0o755,
        Err(io::Error::from_raw_os_error(libc::EACCES)),
    )
    .unwrap_err();
    assert_eq!(failure.condition, AncestorLinkCondition::ParentAclWitness);
    assert_eq!(
        failure.parent,
        Some(ParentFacts {
            uid: EFFECTIVE_UID,
            mode: 0o755
        })
    );
    assert_eq!(failure.source.unwrap().raw_os_error(), Some(libc::EACCES));
}

#[test]
fn identity_recheck_compares_each_field() {
    // SAFETY: stat is a C record of integer fields; all-zero fields are valid test witnesses.
    let mut before: libc::stat = unsafe { std::mem::zeroed() };
    before.st_dev = 7;
    before.st_ino = 19;
    before.st_mode = libc::S_IFLNK | 0o777;
    before.st_uid = EFFECTIVE_UID;
    assert!(identity_unchanged(&before, &before));
    for field in ["device", "inode", "mode", "owner"] {
        let mut after = before;
        match field {
            "device" => after.st_dev += 1,
            "inode" => after.st_ino += 1,
            "mode" => after.st_mode ^= 0o001,
            "owner" => after.st_uid = FOREIGN_UID,
            _ => unreachable!(),
        }
        assert!(
            !identity_unchanged(&before, &after),
            "must-DENY changed {field}"
        );
    }
}

#[test]
fn typed_refusal_retains_condition_metadata_and_io_cause() {
    use std::error::Error as _;
    let refusal = AncestorLinkRefusal {
        condition: AncestorLinkCondition::ParentAclWitness,
        component: OsString::from("link"),
        link_uid: EFFECTIVE_UID,
        parent_uid: Some(EFFECTIVE_UID),
        parent_mode: Some(0o755),
        source: Some(io::Error::from_raw_os_error(libc::EBADF)),
    };
    let error = io::Error::other(refusal);
    let refusal = error
        .get_ref()
        .unwrap()
        .downcast_ref::<AncestorLinkRefusal>()
        .unwrap();
    assert_eq!(refusal.condition, AncestorLinkCondition::ParentAclWitness);
    assert_eq!(refusal.parent_uid, Some(EFFECTIVE_UID));
    assert_eq!(refusal.parent_mode, Some(0o755));
    assert_eq!(
        refusal
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap()
            .raw_os_error(),
        Some(libc::EBADF)
    );
    assert!(error.to_string().contains("parent_acl_witness"));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_unreadable_acl_must_deny_real_fd_failure() {
    let witness = darwin_acl_grants_fd(-1).expect_err("an invalid FD cannot prove a no-grant ACL");
    assert_eq!(witness.raw_os_error(), Some(libc::EBADF));
    assert_decision(
        "must-DENY actual failed macOS ACL witness",
        decision(EFFECTIVE_UID, EFFECTIVE_UID, 0o755, Err(witness)),
        Some(AncestorLinkCondition::ParentAclWitness),
    );
}
