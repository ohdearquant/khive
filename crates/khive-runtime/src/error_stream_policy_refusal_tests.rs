use super::RuntimeError;
use khive_types::{Details, KhiveError};

#[test]
fn stream_policy_refusal_uses_structured_kind_and_marker() {
    let policy = RuntimeError::Khive(
        KhiveError::conflict("changed diagnostic wording")
            .with_details(Details::new([("reason", "stream_member")])),
    );
    assert!(policy.is_stream_policy_refusal());

    let wrong_kind = RuntimeError::Khive(
        KhiveError::invalid_input("stream entries are immutable")
            .with_details(Details::new([("reason", "stream_member")])),
    );
    assert!(!wrong_kind.is_stream_policy_refusal());
}

#[test]
fn stream_policy_refusal_excludes_other_conflicts_and_message_lookalikes() {
    for marker in [
        None,
        Some("seq_conflict"),
        Some("key_conflict"),
        Some("version_conflict"),
        Some("fence_conflict"),
        Some("identity_conflict"),
        Some("expired"),
        Some("live_until_unreadable"),
        Some("key_ambiguous"),
        Some("unknown_op"),
        Some("stream_member_extra"),
    ] {
        let mut error = KhiveError::conflict("stream entries are immutable");
        if let Some(marker) = marker {
            error = error.with_details(Details::new([("reason", marker)]));
        }
        assert!(
            !RuntimeError::Khive(error).is_stream_policy_refusal(),
            "unrelated conflict marker {marker:?}"
        );
    }

    let internal = RuntimeError::Internal("conflict: stream entries are immutable".into());
    assert!(!internal.is_stream_policy_refusal());
    let driver = RuntimeError::Storage(khive_storage::StorageError::driver(
        khive_storage::StorageCapability::Sql,
        "inline.execute",
        std::io::Error::other("stream_member"),
    ));
    assert!(!driver.is_stream_policy_refusal());
}
