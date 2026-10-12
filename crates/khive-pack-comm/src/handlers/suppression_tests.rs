use chrono::{DateTime, Utc};
use khive_runtime::{AllowAllGate, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError};
use khive_storage::Note;
use khive_types::{EventKind, SubstrateKind};
use serde_json::{json, Value};

use super::*;

fn runtime() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        brain_profile: None,
        packs: vec!["kg".into(), "comm".into()],
        actor_id: Some("actor:maintainer".into()),
        gate: std::sync::Arc::new(AllowAllGate),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("runtime")
}

fn token(runtime: &KhiveRuntime) -> khive_runtime::NamespaceToken {
    runtime.authorize(Namespace::local()).expect("token")
}

async fn add(
    runtime: &KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
    scope: &str,
    kind: &str,
    value: &str,
    source: &str,
) -> Result<Value, RuntimeError> {
    handle_suppression_add(
        runtime,
        token,
        json!({
            "scope": scope,
            "kind": kind,
            "value": value,
            "source": source,
            "reason": "test fixture",
        }),
    )
    .await
}

async fn evidence(
    runtime: &KhiveRuntime,
    from: &str,
    created_at: i64,
    quarantined: bool,
    recipient_key: Option<&str>,
) -> uuid::Uuid {
    let mut properties = json!({
        "direction": "inbound",
        "from": format!("email:{from}"),
        "channel_kind": "email",
    });
    if quarantined {
        properties["quarantined"] = json!(true);
    }
    if let Some(key) = recipient_key {
        properties["recipient_key"] = json!(key);
    }
    let mut note = Note::new("local", "message", "authenticated reply").with_properties(properties);
    note.created_at = created_at;
    note.updated_at = created_at;
    let id = note.id;
    assert!(runtime
        .backend()
        .notes()
        .expect("notes")
        .try_insert_note(note)
        .await
        .expect("insert evidence"));
    id
}

fn timestamp(value: &Value) -> i64 {
    DateTime::parse_from_rfc3339(value["created_at"].as_str().expect("created_at"))
        .expect("valid timestamp")
        .with_timezone(&Utc)
        .timestamp_micros()
}

fn invalid(error: RuntimeError) {
    assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
}

fn condition(error: RuntimeError, expected: &str) {
    let RuntimeError::Khive(error) = error else {
        panic!("expected typed refusal, got {error:?}");
    };
    assert_eq!(error.kind(), khive_types::ErrorKind::Unauthorized);
    assert!(error.to_string().contains(expected), "{error}");
}

#[tokio::test]
async fn suppression_add_list_remove_round_trip_is_audited() {
    let runtime = runtime();
    let token = token(&runtime);
    let added = add(
        &runtime,
        &token,
        "outbound",
        "address",
        " Person@Example.COM ",
        "operator",
    )
    .await
    .expect("add");
    assert_eq!(added["value"], "person@example.com");
    assert_eq!(added["created_by"], "actor:maintainer");
    let duplicate = add(
        &runtime,
        &token,
        "outbound",
        "address",
        "PERSON@example.com",
        "operator",
    )
    .await
    .unwrap_err();
    let RuntimeError::Khive(duplicate) = duplicate else {
        panic!("expected duplicate suppression conflict, got {duplicate:?}");
    };
    assert_eq!(duplicate.kind(), khive_types::ErrorKind::Conflict);

    let listed = handle_suppression_list(&runtime, &token, json!({}))
        .await
        .expect("list");
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    assert_eq!(listed["items"][0]["id"], added["id"]);

    let removed = handle_suppression_remove(
        &runtime,
        &token,
        json!({"id": added["id"], "reason": "operator correction"}),
    )
    .await
    .expect("remove");
    assert_eq!(removed["removed"], true);
    let audit_id = removed["audit_event_id"]
        .as_str()
        .expect("audit event id")
        .parse()
        .expect("audit UUID");
    let audit = runtime
        .events(&token)
        .expect("events")
        .get_event(audit_id)
        .await
        .expect("audit read")
        .expect("audit record");
    assert_eq!(audit.kind, EventKind::Audit);
    assert_eq!(audit.substrate, SubstrateKind::Event);
    assert_eq!(audit.payload["reason"], "operator correction");
    assert_eq!(audit.payload["value"], "person@example.com");

    let listed = handle_suppression_list(&runtime, &token, json!({}))
        .await
        .expect("list after remove");
    assert!(listed["items"].as_array().unwrap().is_empty());
    add(
        &runtime,
        &token,
        "outbound",
        "address",
        "person@example.com",
        "operator",
    )
    .await
    .expect("removed value can be added again");
}

#[tokio::test]
async fn opt_out_survives_removal_of_an_operator_entry_for_the_same_value() {
    let runtime = runtime();
    let token = token(&runtime);
    let operator = add(
        &runtime,
        &token,
        "outbound",
        "address",
        "person@example.com",
        "operator",
    )
    .await
    .expect("operator add");
    let opt_out = add(
        &runtime,
        &token,
        "outbound",
        "address",
        "Person@Example.com",
        "reply_stop",
    )
    .await
    .expect("an opt-out for a value the operator already listed is still recorded");
    assert_ne!(operator["id"], opt_out["id"]);

    handle_suppression_remove(
        &runtime,
        &token,
        json!({"id": operator["id"], "reason": "operator correction"}),
    )
    .await
    .expect("operator entry removal needs only a reason");

    let listed = handle_suppression_list(&runtime, &token, json!({}))
        .await
        .expect("list");
    let items = listed["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], opt_out["id"]);
    assert_eq!(items[0]["source"], "reply_stop");
}

#[tokio::test]
async fn suppression_closed_values_accept_members_and_refuse_unknowns() {
    let runtime = runtime();
    let token = token(&runtime);

    for (index, scope) in ["outbound", "inbound"].into_iter().enumerate() {
        add(
            &runtime,
            &token,
            scope,
            "address",
            &format!("scope{index}@example.com"),
            "operator",
        )
        .await
        .expect("closed scope member");
    }
    for (index, kind) in ["address", "domain", "recipient_key"]
        .into_iter()
        .enumerate()
    {
        let value = match kind {
            "address" => format!("kind{index}@example.com"),
            "domain" => format!("kind{index}.example.com"),
            _ => format!("{:064x}", index + 1),
        };
        add(&runtime, &token, "outbound", kind, &value, "operator")
            .await
            .expect("closed kind member");
    }
    for (index, source) in ["operator", "mailto_token", "one_click", "reply_stop"]
        .into_iter()
        .enumerate()
    {
        add(
            &runtime,
            &token,
            "outbound",
            "recipient_key",
            &format!("{:064x}", index + 1),
            source,
        )
        .await
        .expect("closed source member");
    }

    for (field, value) in [
        ("scope", "sideways"),
        ("kind", "mailbox"),
        ("source", "imported"),
    ] {
        let mut params = json!({
            "scope": "outbound",
            "kind": "address",
            "value": format!("invalid-{field}@example.com"),
            "source": "operator",
        });
        params[field] = json!(value);
        invalid(
            handle_suppression_add(&runtime, &token, params)
                .await
                .unwrap_err(),
        );
    }
}

#[tokio::test]
async fn recipient_key_suppression_requires_exactly_64_lowercase_hex_characters() {
    let runtime = runtime();
    let token = token(&runtime);
    for value in [
        "ab".repeat(31),                 // too short
        format!("{}g", "ab".repeat(31)), // non-hex
        "AB".repeat(32),                 // uppercase
        String::new(),                   // empty
    ] {
        invalid(
            add(
                &runtime,
                &token,
                "outbound",
                "recipient_key",
                &value,
                "operator",
            )
            .await
            .unwrap_err(),
        );
    }
    add(
        &runtime,
        &token,
        "outbound",
        "recipient_key",
        &"ab".repeat(32),
        "operator",
    )
    .await
    .expect("exactly 64 lowercase hex characters is accepted");
}

#[tokio::test]
async fn opt_out_removal_requires_new_authenticated_same_address_evidence() {
    let runtime = runtime();
    let token = token(&runtime);
    let old = evidence(
        &runtime,
        "person@example.com",
        Utc::now().timestamp_micros() - 10_000_000,
        false,
        None,
    )
    .await;
    let added = add(
        &runtime,
        &token,
        "outbound",
        "address",
        "person@example.com",
        "reply_stop",
    )
    .await
    .expect("add opt-out");
    let entry_time = timestamp(&added);

    condition(
        handle_suppression_remove(
            &runtime,
            &token,
            json!({"id": added["id"], "reason": "person wrote back"}),
        )
        .await
        .unwrap_err(),
        "evidence_inbound_message_id is required",
    );

    let different_address =
        evidence(&runtime, "other@example.com", entry_time + 1, false, None).await;
    condition(
        handle_suppression_remove(
            &runtime,
            &token,
            json!({
                "id": added["id"],
                "reason": "person wrote back",
                "evidence_inbound_message_id": different_address,
            }),
        )
        .await
        .unwrap_err(),
        "same address",
    );

    let unauthenticated =
        evidence(&runtime, "person@example.com", entry_time + 2, true, None).await;
    condition(
        handle_suppression_remove(
            &runtime,
            &token,
            json!({
                "id": added["id"],
                "reason": "person wrote back",
                "evidence_inbound_message_id": unauthenticated,
            }),
        )
        .await
        .unwrap_err(),
        "authenticated email",
    );

    condition(
        handle_suppression_remove(
            &runtime,
            &token,
            json!({
                "id": added["id"],
                "reason": "person wrote back",
                "evidence_inbound_message_id": old,
            }),
        )
        .await
        .unwrap_err(),
        "created after the suppression entry",
    );

    let valid = evidence(&runtime, "person@example.com", entry_time + 3, false, None).await;
    let removed = handle_suppression_remove(
        &runtime,
        &token,
        json!({
            "id": added["id"],
            "reason": "person wrote back",
            "evidence_inbound_message_id": valid,
        }),
    )
    .await
    .expect("valid evidence removes opt-out");
    assert_eq!(removed["removed"], true);
}

#[tokio::test]
async fn opt_out_sources_all_require_evidence_and_operator_removal_requires_reason() {
    let runtime = runtime();
    let token = token(&runtime);
    for (index, source) in ["mailto_token", "one_click", "reply_stop"]
        .into_iter()
        .enumerate()
    {
        let added = add(
            &runtime,
            &token,
            "outbound",
            "recipient_key",
            &format!("{:064x}", index + 1),
            source,
        )
        .await
        .expect("add opt-out source");
        condition(
            handle_suppression_remove(
                &runtime,
                &token,
                json!({"id": added["id"], "reason": "undo"}),
            )
            .await
            .unwrap_err(),
            "evidence_inbound_message_id is required",
        );
    }

    let operator = add(
        &runtime,
        &token,
        "outbound",
        "address",
        "operator@example.com",
        "operator",
    )
    .await
    .expect("operator entry");
    for reason in ["", "  "] {
        invalid(
            handle_suppression_remove(
                &runtime,
                &token,
                json!({"id": operator["id"], "reason": reason}),
            )
            .await
            .unwrap_err(),
        );
    }
    handle_suppression_remove(
        &runtime,
        &token,
        json!({"id": operator["id"], "reason": "verified correction"}),
    )
    .await
    .expect("operator entry removed with reason");

    let keyed_opt_out = add(
        &runtime,
        &token,
        "outbound",
        "recipient_key",
        &"ab".repeat(32),
        "mailto_token",
    )
    .await
    .expect("keyed opt-out");
    let keyed_evidence = evidence(
        &runtime,
        "person@example.com",
        timestamp(&keyed_opt_out) + 1,
        false,
        Some(&"ab".repeat(32)),
    )
    .await;
    handle_suppression_remove(
        &runtime,
        &token,
        json!({
            "id": keyed_opt_out["id"],
            "reason": "recipient wrote back",
            "evidence_inbound_message_id": keyed_evidence,
        }),
    )
    .await
    .expect("trusted recipient-key evidence removes keyed opt-out");
}

#[test]
fn outbound_suppression_matches_exact_domains_and_recipient_keys() {
    use khive_runtime::{
        outbound_email_is_suppressed, EmailSuppression, EmailSuppressionKind, EmailSuppressionScope,
    };

    let domain = [EmailSuppression {
        scope: EmailSuppressionScope::Outbound,
        kind: EmailSuppressionKind::Domain,
        value: "x.com".into(),
    }];
    assert!(outbound_email_is_suppressed("a@x.com", None, &domain));
    assert!(!outbound_email_is_suppressed("a@sub.x.com", None, &domain));

    let key = [EmailSuppression {
        scope: EmailSuppressionScope::Outbound,
        kind: EmailSuppressionKind::RecipientKey,
        value: "key-1".into(),
    }];
    assert!(outbound_email_is_suppressed(
        "a@example.com",
        Some("key-1"),
        &key
    ));
    assert!(!outbound_email_is_suppressed(
        "a@example.com",
        Some("key-2"),
        &key
    ));
    assert!(!outbound_email_is_suppressed("a@example.com", None, &key));
}

#[test]
fn suppression_caller_scope_uses_the_runtime_actor_as_current_maintainer() {
    let runtime = runtime();
    assert!(ensure_suppression_caller_allowed(&runtime, "actor:maintainer").is_ok());
    condition(
        ensure_suppression_caller_allowed(&runtime, "actor:other").unwrap_err(),
        "not allowed for this caller actor",
    );
    let external = runtime.clone().with_outbound_email_policy(
        khive_runtime::OutboundEmailPolicy::configured_for_tests(
            vec!["maintainer@example.com".into()],
            vec!["actor:external".into()],
            5,
        )
        .unwrap(),
    );
    assert!(ensure_suppression_caller_allowed(&external, "actor:external").is_ok());
    assert!(ensure_suppression_caller_allowed(&external, "actor:other").is_err());
}
