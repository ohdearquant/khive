use super::*;
use serde_json::json;

#[test]
fn note_write_validator_rejects_transport_owned_quarantine_properties() {
    for (key, value) in [
        ("quarantined", json!(true)),
        ("channel_kind", json!("email")),
        ("channel_slug", json!("victim-account")),
    ] {
        let error = derive_message_identity("message", "lambda:caller", Some(json!({key: value})))
            .expect_err("covered generic/proposal writes must reject transport provenance");
        assert!(
            error.to_string().contains(key),
            "error must name {key}: {error}"
        );
    }
}

#[test]
fn note_write_refusal_names_the_owner_of_hold_and_ingest_properties() {
    let hold = derive_message_identity(
        "message",
        "lambda:caller",
        Some(json!({"delivery_hold": "external_id_unverifiable"})),
    )
    .expect_err("generic writes must not set an outbox owner-side hold")
    .to_string();
    assert!(hold.contains("delivery_hold"), "{hold}");
    assert!(hold.contains("internal outbox owner-side hold"), "{hold}");
    assert!(!hold.contains("only `comm.ingest`"), "{hold}");

    let ingest = derive_message_identity(
        "message",
        "lambda:caller",
        Some(json!({"channel_slug": "inbox"})),
    )
    .expect_err("generic writes must not set ingest provenance")
    .to_string();
    assert!(ingest.contains("channel_slug"), "{ingest}");
    assert!(ingest.contains("only `comm.ingest`"), "{ingest}");
    assert!(!ingest.contains("outbox owner-side hold"), "{ingest}");
}
