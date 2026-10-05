use std::sync::Arc;

use khive_runtime::{
    AllowAllGate, KhiveRuntime, Namespace, NamespaceToken, OutboundEmailPolicy, RuntimeConfig,
    RuntimeError,
};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::handlers::{handle_reply, handle_send};
use crate::inbox_signal::InboxSignal;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Boundary {
    BeforeHolderLookup,
    AfterHolderLookup,
}

struct PausePoint {
    boundary: Boundary,
    reached: Arc<Notify>,
    resume: Arc<Notify>,
}

tokio::task_local! {
    static PAUSE: PausePoint;
}

pub(crate) async fn pause(boundary: Boundary) {
    let point = PAUSE
        .try_with(|point| {
            (point.boundary == boundary).then(|| (point.reached.clone(), point.resume.clone()))
        })
        .ok()
        .flatten();
    if let Some((reached, resume)) = point {
        reached.notify_one();
        resume.notified().await;
    }
}

fn runtime() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        brain_profile: None,
        packs: vec!["kg".into(), "comm".into()],
        actor_id: Some("actor:sender".into()),
        gate: Arc::new(AllowAllGate),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap()
}

fn deny(runtime: &KhiveRuntime) -> KhiveRuntime {
    runtime.clone().with_outbound_email_policy(
        OutboundEmailPolicy::configured(vec!["allowed@example.com".into()]).unwrap(),
    )
}

fn args() -> Value {
    json!({"to":"email:denied@example.com", "content":"body", "subject":"subject", "idempotency_key":"email-operation"})
}

fn denied(error: RuntimeError, expected_verb: &str) {
    let RuntimeError::PermissionDenied { verb, reason, .. } = error else {
        panic!("expected typed permission denial, got {error:?}");
    };
    assert_eq!(verb, expected_verb);
    assert!(reason.contains("no message was committed by this request"));
    assert!(!reason.contains("allowed@example.com"));
    assert!(reason.contains("denied@example.com"));
    assert!(!reason.contains("DENIED@EXAMPLE.COM"));
}

fn conflict(error: RuntimeError, id: &Value) {
    let RuntimeError::Khive(error) = error else {
        panic!("expected key conflict: {error:?}");
    };
    assert_eq!(error.kind(), khive_types::ErrorKind::Conflict);
    assert_eq!(error.details().unwrap().get("reason"), Some("key_conflict"));
    assert_eq!(error.details().unwrap().get("existing_id"), id.as_str());
}

async fn snapshot(runtime: &KhiveRuntime) -> Value {
    let value = runtime.sql().reader().await.unwrap().query_scalar(SqlStatement {
        sql: "SELECT json_object(\
              'messages',(SELECT json_group_array(json_object('id',id,'key',key,'content',content,'properties',properties,'updated_at',updated_at,'deleted_at',deleted_at)) FROM (SELECT * FROM notes WHERE kind='message' ORDER BY id)),\
              'notes',(SELECT count(*) FROM notes),\
              'keys',(SELECT count(*) FROM notes WHERE key IS NOT NULL),\
              'fts',(SELECT count(*) FROM fts_notes),\
              'rowids',(SELECT count(*) FROM fts_notes_rowids),\
              'attachments',(SELECT count(*) FROM attachments))".into(),
        params: vec![], label: Some("email-admission-write-oracle".into()),
    }).await.unwrap();
    let Some(SqlValue::Text(value)) = value else {
        panic!("unexpected snapshot: {value:?}");
    };
    serde_json::from_str(&value).unwrap()
}

async fn send(
    runtime: &KhiveRuntime,
    signal: &InboxSignal,
    token: &NamespaceToken,
    args: Value,
) -> Result<Value, RuntimeError> {
    handle_send(runtime, signal, token, args).await
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn email_fresh_send_refuses_before_any_note_key_index_attachment_or_wake() {
    let runtime = deny(&runtime());
    let token = runtime.authorize(Namespace::local()).unwrap();
    let signal = InboxSignal::new();
    let before = snapshot(&runtime).await;
    assert_eq!(before["notes"], 0);
    for keyed in [false, true] {
        let mut request = args();
        if !keyed {
            request.as_object_mut().unwrap().remove("idempotency_key");
        }
        denied(
            send(&runtime, &signal, &token, request).await.unwrap_err(),
            "comm.send",
        );
        assert_eq!(snapshot(&runtime).await, before);
        assert_eq!(signal.snapshot(), 0);
    }
    let mut allowed = args();
    allowed["to"] = json!("email:allowed@example.com");
    let first = send(&runtime, &signal, &token, allowed).await.unwrap();
    assert_eq!(first["replayed"], false);
    let after = snapshot(&runtime).await;
    assert_eq!(after["notes"], 2);
    assert_eq!(after["keys"], 1);
    assert_eq!(after["fts"], 2);
    assert_eq!(after["attachments"], 0);
    assert_eq!(signal.snapshot(), 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn email_fresh_reply_refusal_preserves_parent_and_validation_precedence() {
    let runtime = runtime();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let signal = InboxSignal::new();
    let original = send(&runtime, &signal, &token, args()).await.unwrap();
    let denied_runtime = deny(&runtime);
    let before = snapshot(&runtime).await;
    let reply = json!({"id":original["full_id"],"content":"reply", "idempotency_key":"reply-key"});
    denied(
        handle_reply(&denied_runtime, &signal, &Ok(None), &token, reply)
            .await
            .unwrap_err(),
        "comm.reply",
    );
    assert_eq!(snapshot(&runtime).await, before);
    assert_eq!(signal.snapshot(), 1);
    let mut invalid = args();
    invalid["content"] = json!("  ");
    assert!(matches!(
        send(&denied_runtime, &signal, &token, invalid).await,
        Err(RuntimeError::InvalidInput(_))
    ));
    let mut attached = args();
    attached["attachments"] = json!(["a".repeat(64)]);
    let error = send(&denied_runtime, &signal, &token, attached)
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::InvalidInput(_)));
    assert!(error
        .to_string()
        .contains("attachments require a local recipient"));
    assert_eq!(snapshot(&runtime).await, before);
    assert_eq!(signal.snapshot(), 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn normalized_email_policy_preserves_stored_recipient_and_exact_key_identity() {
    let runtime = runtime().with_outbound_email_policy(
        OutboundEmailPolicy::configured(vec!["Owner <Allowed@Example.com>".into()]).unwrap(),
    );
    let token = runtime.authorize(Namespace::local()).unwrap();
    let signal = InboxSignal::new();
    let mut request = args();
    request["to"] = json!("email:ALLOWED@EXAMPLE.COM");
    let original = send(&runtime, &signal, &token, request.clone())
        .await
        .unwrap();
    let reply = handle_reply(
        &runtime,
        &signal,
        &Ok(None),
        &token,
        json!({"id":original["full_id"],"content":"reply","idempotency_key":"normalized-reply"}),
    )
    .await
    .unwrap();
    for sent in [&original, &reply] {
        let note = runtime
            .notes(&token)
            .unwrap()
            .get_note(sent["full_id"].as_str().unwrap().parse().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            note.properties.as_ref().unwrap()["to_actor"],
            "email:ALLOWED@EXAMPLE.COM"
        );
    }
    let before = snapshot(&runtime).await;
    request["to"] = json!("email:allowed@example.com");
    conflict(
        send(&runtime, &signal, &token, request).await.unwrap_err(),
        &original["full_id"],
    );
    let mut refused = args();
    refused["to"] = json!("email:Denied <DENIED@EXAMPLE.COM>");
    refused["idempotency_key"] = json!("fresh-denied");
    denied(
        send(&runtime, &signal, &token, refused).await.unwrap_err(),
        "comm.send",
    );
    let error = send(
        &runtime,
        &signal,
        &token,
        json!({"to":"email:not-an-address","content":"invalid recipient"}),
    )
    .await
    .unwrap_err();
    let RuntimeError::PermissionDenied { reason, .. } = error else {
        panic!("unparseable configured recipient must refuse: {error:?}");
    };
    assert!(reason.contains("invalid email recipient"));
    assert!(!reason.contains("allowed@example.com"));
    assert_eq!(snapshot(&runtime).await, before);
    assert_eq!(signal.snapshot(), 2);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn absent_policy_preserves_backlog_and_non_email_recipients_ignore_email_policy() {
    let runtime = runtime();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let signal = InboxSignal::new();
    send(&runtime, &signal, &token, args()).await.unwrap();
    let denied_runtime = runtime
        .clone()
        .with_outbound_email_policy(OutboundEmailPolicy::configured(vec![]).unwrap());
    for recipient in ["actor:recipient", "telegram:recipient", "khive1:recipient"] {
        send(
            &denied_runtime,
            &signal,
            &token,
            json!({"to":recipient,"content":"non-email"}),
        )
        .await
        .unwrap();
    }
    assert_eq!(snapshot(&runtime).await["notes"], 8);
    assert_eq!(signal.snapshot(), 4);
    assert!(runtime.outbound_email_policy().allows("denied@example.com"));
    assert!(!denied_runtime
        .core()
        .outbound_email_policy()
        .allows("denied@example.com"));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn committed_email_send_and_reply_replay_after_revocation_without_mutation() {
    let runtime = runtime();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let signal = InboxSignal::new();
    let original = send(&runtime, &signal, &token, args()).await.unwrap();
    let reply_args =
        json!({"id":original["full_id"],"content":"reply","idempotency_key":"reply-key"});
    let reply = handle_reply(&runtime, &signal, &Ok(None), &token, reply_args.clone())
        .await
        .unwrap();
    for sent in [&original, &reply] {
        runtime
            .mark_outbound_message_failed(
                &token,
                sent["full_id"].as_str().unwrap().parse().unwrap(),
                "2026-10-05T00:00:00Z".into(),
                "historical delivery failure".into(),
            )
            .await
            .unwrap();
    }
    let denied_runtime = deny(&runtime);
    let before = snapshot(&runtime).await;
    for (verb, request, first) in [
        ("comm.send", args(), original),
        ("comm.reply", reply_args, reply),
    ] {
        let replay = if verb == "comm.send" {
            send(&denied_runtime, &signal, &token, request)
                .await
                .unwrap()
        } else {
            handle_reply(&denied_runtime, &signal, &Ok(None), &token, request)
                .await
                .unwrap()
        };
        assert_eq!(replay["replayed"], true);
        for field in [
            "full_id",
            "recipient_id",
            "sent_at",
            "thread_id",
            "idempotency_key",
        ] {
            assert_eq!(replay[field], first[field], "{verb} {field}");
        }
        assert_eq!(snapshot(&runtime).await, before);
        assert_eq!(signal.snapshot(), 2);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn email_replay_rejects_changed_payload_broken_pair_and_attachment_mismatch() {
    for damage in ["payload", "missing-sibling", "attachment"] {
        let runtime = runtime();
        let token = runtime.authorize(Namespace::local()).unwrap();
        let signal = InboxSignal::new();
        let first = send(&runtime, &signal, &token, args()).await.unwrap();
        let mut request = args();
        match damage {
            "payload" => request["content"] = json!("changed"),
            "missing-sibling" => {
                runtime
                    .notes(&token)
                    .unwrap()
                    .delete_note(
                        first["recipient_id"].as_str().unwrap().parse().unwrap(),
                        khive_storage::types::DeleteMode::Hard,
                    )
                    .await
                    .unwrap();
            }
            "attachment" => {
                runtime.sql().writer().await.unwrap().execute(SqlStatement {
                    sql: "INSERT INTO attachments(record_uuid,substrate,role,content_ref,created_at) VALUES (?1,'note','message-attachment:0',?2,1)".into(),
                    params: vec![SqlValue::Text(first["full_id"].as_str().unwrap().into()), SqlValue::Text("a".repeat(64))],
                    label: Some("email-replay-corrupt-attachment-fixture".into()),
                }).await.unwrap();
            }
            _ => unreachable!(),
        }
        let before = snapshot(&runtime).await;
        conflict(
            send(&deny(&runtime), &signal, &token, request)
                .await
                .unwrap_err(),
            &first["full_id"],
        );
        assert_eq!(snapshot(&runtime).await, before, "{damage}");
        assert_eq!(signal.snapshot(), 1);
    }
}

async fn competing_holder(boundary: Boundary) {
    let runtime = runtime();
    let denied_runtime = deny(&runtime);
    let token = runtime.authorize(Namespace::local()).unwrap();
    let denied_signal = InboxSignal::new();
    let winner_signal = InboxSignal::new();
    let reached = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    let observed = PAUSE.scope(
        PausePoint {
            boundary,
            reached: reached.clone(),
            resume: resume.clone(),
        },
        send(&denied_runtime, &denied_signal, &token, args()),
    );
    let creator = async {
        reached.notified().await;
        let result = send(&runtime, &winner_signal, &token, args())
            .await
            .unwrap();
        resume.notify_one();
        result
    };
    let (observed, winner) = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        tokio::join!(observed, creator)
    })
    .await
    .expect("deterministic holder boundary must finish");
    if boundary == Boundary::BeforeHolderLookup {
        let replay = observed.expect("holder committed before final observation wins");
        assert_eq!(replay["full_id"], winner["full_id"]);
        assert_eq!(replay["replayed"], true);
    } else {
        denied(observed.unwrap_err(), "comm.send");
    }
    assert_eq!(denied_signal.snapshot(), 0);
    assert_eq!(winner_signal.snapshot(), 1);
    let before = snapshot(&runtime).await;
    assert_eq!(before["notes"], 2);
    assert_eq!(before["keys"], 1);
    let retry = send(&denied_runtime, &denied_signal, &token, args())
        .await
        .unwrap();
    assert_eq!(retry["full_id"], winner["full_id"]);
    assert_eq!(retry["recipient_id"], winner["recipient_id"]);
    assert_eq!(retry["replayed"], true);
    assert_eq!(snapshot(&runtime).await, before);
    assert_eq!(denied_signal.snapshot(), 0);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn email_holder_committed_before_final_observation_is_reconciled() {
    competing_holder(Boundary::BeforeHolderLookup).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn email_holder_committed_after_final_observation_is_available_on_retry() {
    competing_holder(Boundary::AfterHolderLookup).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn allowed_email_competing_creators_reconcile_one_pair() {
    let runtime = runtime().with_outbound_email_policy(
        OutboundEmailPolicy::configured(vec!["denied@example.com".into()]).unwrap(),
    );
    let token = runtime.authorize(Namespace::local()).unwrap();
    let signal = InboxSignal::new();
    let (left, right) = tokio::join!(
        send(&runtime, &signal, &token, args()),
        send(&runtime, &signal, &token, args())
    );
    let (left, right) = (left.unwrap(), right.unwrap());
    assert_eq!(left["full_id"], right["full_id"]);
    assert_eq!(left["recipient_id"], right["recipient_id"]);
    assert_ne!(left["replayed"], right["replayed"]);
    let after = snapshot(&runtime).await;
    assert_eq!(after["notes"], 2);
    assert_eq!(after["keys"], 1);
    assert_eq!(after["fts"], 2);
    assert_eq!(after["rowids"], 2);
    assert_eq!(signal.snapshot(), 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn email_key_holder_remains_scoped_to_actor_and_namespace() {
    let runtime = runtime();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let signal = InboxSignal::new();
    send(&runtime, &signal, &token, args()).await.unwrap();
    let other = runtime
        .authorize(Namespace::parse("other").unwrap())
        .unwrap();
    let before = snapshot(&runtime).await;
    denied(
        send(&deny(&runtime), &signal, &other, args())
            .await
            .unwrap_err(),
        "comm.send",
    );
    let other_runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        brain_profile: None,
        actor_id: Some("actor:other".into()),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let other_actor = other_runtime.authorize(Namespace::local()).unwrap();
    assert_ne!(token.actor().id, other_actor.actor().id);
    denied(
        send(&deny(&runtime), &signal, &other_actor, args())
            .await
            .unwrap_err(),
        "comm.send",
    );
    assert_eq!(snapshot(&runtime).await, before);
    assert_eq!(signal.snapshot(), 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn email_exact_replay_does_not_bypass_dispatch_authorization() {
    #[derive(Debug)]
    struct Refuse;
    impl khive_runtime::Gate for Refuse {
        fn check(
            &self,
            _: &khive_runtime::GateRequest,
        ) -> Result<khive_runtime::GateDecision, khive_runtime::GateError> {
            Ok(khive_runtime::GateDecision::Deny {
                reason: "fixture-caller-denied".into(),
            })
        }
    }
    let runtime = runtime();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let signal = InboxSignal::new();
    send(&runtime, &signal, &token, args()).await.unwrap();
    let before = snapshot(&runtime).await;
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(crate::CommPack::new(deny(&runtime)));
    builder.with_actor_id(Some("actor:sender".into()));
    builder.with_default_namespace("local");
    builder.with_runtime_event_store(&runtime).unwrap();
    builder.with_gate(Arc::new(Refuse));
    let registry = builder.build().unwrap();
    let error = registry.dispatch("comm.send", args()).await.unwrap_err();
    let RuntimeError::PermissionDenied { reason, .. } = error else {
        panic!("expected the caller gate to refuse before replay: {error:?}");
    };
    assert_eq!(reason, "fixture-caller-denied");
    assert_eq!(snapshot(&runtime).await, before);
    assert_eq!(signal.snapshot(), 1);
}
