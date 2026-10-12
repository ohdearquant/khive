use khive_pack_comm::CommPack;
use khive_pack_kg as _; // Retain the KG pack's inventory registration in this binary.
use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    KhiveRuntime, Namespace, PackRegistry, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::Note;
use serde_json::{json, Value};

const OLD: &str = "imap:mail.example.com:17:42";
const NEW_A: &str = "imap:mail.example.com:a@example.com:17:42";
const NEW_B: &str = "imap:mail.example.com:b@example.com:17:42";

fn fixture() -> (VerbRegistry, KhiveRuntime) {
    let rt = KhiveRuntime::memory().expect("synthetic in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(
        &["kg".to_string(), "comm".to_string()],
        rt.clone(),
        &mut builder,
    )
    .expect("register a separately granted seed composition");
    builder.with_default_namespace("local");
    (builder.build().expect("build seed registry"), rt)
}

fn input(id: &str, slug: &str) -> Value {
    json!({
        "from": "email:sender@example.com",
        "to": "email:maintainer@example.com",
        "content": "Test message body",
        "channel_kind": "email",
        "channel_slug": slug,
        "external_id": id
    })
}

fn migrated(id: &str, slug: &str) -> Value {
    let mut p = input(id, slug);
    p["legacy_external_id"] = json!(OLD);
    p
}

async fn read_note(rt: &KhiveRuntime, ack: &Value) -> Note {
    let token = rt.authorize(Namespace::local()).unwrap();
    let id = ack["full_id"]
        .as_str()
        .expect("new seed returns full_id")
        .parse()
        .expect("full UUID");
    rt.notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap()
}

async fn unchanged(rt: &KhiveRuntime, before: &Note) {
    let token = rt.authorize(Namespace::local()).unwrap();
    let after = rt
        .notes(&token)
        .unwrap()
        .get_note(before.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after, *before, "dedup must not rewrite the persisted note");
}

fn requires_grant(result: Result<Value, RuntimeError>) {
    match result {
        Err(RuntimeError::Unconfigured(message)) => {
            assert!(message.contains("channel-ingest capability"), "{message}");
        }
        other => panic!("ungranted ingest must return Unconfigured, got {other:?}"),
    }
}

#[tokio::test]
async fn ungranted_new_key_duplicate_is_refused() {
    let (registry, rt) = fixture();
    let seed = registry
        .dispatch("comm.ingest", input(NEW_A, "a@example.com"))
        .await
        .unwrap();
    let before = read_note(&rt, &seed).await;
    // The grant on the seed registry must not transfer to this new instance.
    let ungranted = CommPack::new(rt.clone());
    let token = rt.authorize(Namespace::local()).unwrap();
    let result = ungranted
        .dispatch(
            "comm.ingest",
            migrated(NEW_A, "a@example.com"),
            &registry,
            &token,
        )
        .await;
    unchanged(&rt, &before).await;
    requires_grant(result);
}

#[tokio::test]
async fn ungranted_legacy_duplicate_is_refused() {
    let (registry, rt) = fixture();
    let seed = registry
        .dispatch("comm.ingest", input(OLD, "a@example.com"))
        .await
        .unwrap();
    let before = read_note(&rt, &seed).await;
    let ungranted = CommPack::new(rt.clone());
    let token = rt.authorize(Namespace::local()).unwrap();
    let result = ungranted
        .dispatch(
            "comm.ingest",
            migrated(NEW_A, "a@example.com"),
            &registry,
            &token,
        )
        .await;
    unchanged(&rt, &before).await;
    requires_grant(result);
}

#[tokio::test]
async fn ungranted_new_delivery_is_refused() {
    let (registry, rt) = fixture();
    let ungranted = CommPack::new(rt.clone());
    let token = rt.authorize(Namespace::local()).unwrap();
    requires_grant(
        ungranted
            .dispatch(
                "comm.ingest",
                migrated(NEW_A, "a@example.com"),
                &registry,
                &token,
            )
            .await,
    );
    assert_eq!(
        rt.notes(&token)
            .unwrap()
            .count_notes("local", Some("message"))
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn ungranted_duplicate_without_migration_is_refused() {
    let (registry, rt) = fixture();
    let seed = registry
        .dispatch("comm.ingest", input(NEW_A, "a@example.com"))
        .await
        .unwrap();
    let before = read_note(&rt, &seed).await;
    let ungranted = CommPack::new(rt.clone());
    let token = rt.authorize(Namespace::local()).unwrap();
    let result = ungranted
        .dispatch(
            "comm.ingest",
            input(NEW_A, "a@example.com"),
            &registry,
            &token,
        )
        .await;
    unchanged(&rt, &before).await;
    requires_grant(result);
}

#[tokio::test]
async fn granted_legacy_replay_preserves_note_and_thread() {
    let (registry, rt) = fixture();
    let seed = registry
        .dispatch("comm.ingest", input(OLD, "a@example.com"))
        .await
        .unwrap();
    let before = read_note(&rt, &seed).await;
    let replay = registry
        .dispatch("comm.ingest", migrated(NEW_A, "a@example.com"))
        .await
        .unwrap();
    assert_eq!(replay["deduplicated"], true);
    assert_eq!(replay["thread_id"], seed["thread_id"]);
    assert_eq!(replay["external_id"], NEW_A);
    unchanged(&rt, &before).await;
    let token = rt.authorize(Namespace::local()).unwrap();
    assert_eq!(
        rt.notes(&token)
            .unwrap()
            .count_notes("local", Some("message"))
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn granted_new_key_precedes_same_account_legacy_key() {
    let (registry, rt) = fixture();
    let old_seed = registry
        .dispatch("comm.ingest", input(OLD, "a@example.com"))
        .await
        .unwrap();
    let new_seed = registry
        .dispatch("comm.ingest", input(NEW_A, "a@example.com"))
        .await
        .unwrap();
    let old_before = read_note(&rt, &old_seed).await;
    let new_before = read_note(&rt, &new_seed).await;
    assert_ne!(old_seed["thread_id"], new_seed["thread_id"]);
    let replay = registry
        .dispatch("comm.ingest", migrated(NEW_A, "a@example.com"))
        .await
        .unwrap();
    assert_eq!(replay["deduplicated"], true);
    assert_eq!(replay["thread_id"], new_seed["thread_id"]);
    unchanged(&rt, &old_before).await;
    unchanged(&rt, &new_before).await;
}

#[tokio::test]
async fn unrelated_or_unattributed_legacy_row_cannot_suppress_new_account() {
    for case in ["other-account", "other-channel", "missing-slug"] {
        let (registry, rt) = fixture();
        let mut legacy = input(OLD, "a@example.com");
        match case {
            "other-account" => {}
            "other-channel" => {
                legacy["channel_kind"] = json!("telegram");
                legacy["channel_slug"] = json!("b@example.com");
            }
            "missing-slug" => {
                legacy.as_object_mut().unwrap().remove("channel_slug");
            }
            _ => unreachable!(),
        }
        let old_seed = registry.dispatch("comm.ingest", legacy).await.unwrap();
        let before = read_note(&rt, &old_seed).await;
        let first = registry
            .dispatch("comm.ingest", migrated(NEW_B, "b@example.com"))
            .await
            .unwrap();
        assert_eq!(first["deduplicated"], false, "{case}");
        let new_note = read_note(&rt, &first).await;
        assert_eq!(new_note.properties.as_ref().unwrap()["external_id"], NEW_B);
        assert!(new_note
            .properties
            .as_ref()
            .unwrap()
            .get("legacy_external_id")
            .is_none());
        let retry = registry
            .dispatch("comm.ingest", migrated(NEW_B, "b@example.com"))
            .await
            .unwrap();
        assert_eq!(retry["deduplicated"], true, "{case}");
        assert_eq!(retry["thread_id"], first["thread_id"]);
        unchanged(&rt, &before).await;
        unchanged(&rt, &new_note).await;
    }
}

#[tokio::test]
async fn invalid_legacy_context_is_refused_without_rows() {
    for case in [
        "missing-slug",
        "missing-new-id",
        "other-channel",
        "empty-old-id",
    ] {
        let (registry, rt) = fixture();
        let mut p = migrated(NEW_A, "a@example.com");
        match case {
            "missing-slug" => {
                p.as_object_mut().unwrap().remove("channel_slug");
            }
            "missing-new-id" => {
                p.as_object_mut().unwrap().remove("external_id");
            }
            "other-channel" => {
                p["channel_kind"] = json!("telegram");
            }
            "empty-old-id" => {
                p["legacy_external_id"] = json!(" ");
            }
            _ => unreachable!(),
        }
        let result = registry.dispatch("comm.ingest", p).await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "{case}: {result:?}"
        );
        let token = rt.authorize(Namespace::local()).unwrap();
        assert_eq!(
            rt.notes(&token)
                .unwrap()
                .count_notes("local", Some("message"))
                .await
                .unwrap(),
            0,
            "{case}"
        );
    }
}

#[tokio::test]
async fn granted_ingest_skips_forwarded_but_preserves_metadata_and_delivery() {
    for forwarded in [json!({"from_actor": "forged:sender"}), Value::Null] {
        let (registry, rt) = fixture();
        let mut args = input(NEW_A, "a@example.com");
        let ordinary_details = json!({"labels": ["inbox", "important"], "attempt": 1});
        args["metadata"] = json!({
            "forwarded": forwarded,
            "adapter_tag": "ordinary metadata",
            "adapter_details": ordinary_details,
            "comm_schema_version": 999,
            "from": "forged:from",
            "to": "forged:to",
            "from_actor": "forged:sender",
            "to_actor": "forged:recipient",
            "direction": "outbound",
            "read": true,
            "thread_id": "forged-thread",
            "sent_at": "forged-time",
            "channel_kind": "telegram",
            "channel_slug": "forged-channel",
            "external_id": "forged-external",
            "subject": "forged subject",
            "outbound_ref": "forged-outbound",
            "sent_by_process": "forged-process"
        });
        let ack = registry
            .dispatch("comm.ingest", args.clone())
            .await
            .unwrap();
        assert_eq!(ack["deduplicated"], false);
        assert_eq!(ack["external_id"], NEW_A);
        let before = read_note(&rt, &ack).await;
        assert_eq!(before.kind, "message");
        assert_eq!(before.namespace, "local");
        assert_eq!(before.content, "Test message body");
        let props = before.properties.as_ref().unwrap();
        assert!(props.get("forwarded").is_none());
        assert_eq!(props["adapter_tag"], "ordinary metadata");
        assert_eq!(props["adapter_details"], ordinary_details);
        assert_eq!(props["comm_schema_version"], 1);
        assert_eq!(props["from"], "email:sender@example.com");
        assert_eq!(props["to"], "email:maintainer@example.com");
        assert_eq!(props["from_actor"], "email:sender@example.com");
        assert_eq!(props["to_actor"], "email:maintainer@example.com");
        assert_eq!(props["direction"], "inbound");
        assert_eq!(props["read"], false);
        assert_eq!(props["thread_id"], ack["thread_id"]);
        ack["thread_id"]
            .as_str()
            .unwrap()
            .parse::<uuid::Uuid>()
            .unwrap();
        assert_ne!(props["sent_at"], "forged-time");
        assert!(props["sent_at"].as_str().is_some());
        assert_eq!(props["channel_kind"], "email");
        assert_eq!(props["channel_slug"], "a@example.com");
        assert_eq!(props["external_id"], NEW_A);
        for absent in ["subject", "outbound_ref", "sent_by_process"] {
            assert!(props.get(absent).is_none(), "{absent}");
        }

        let replay = registry.dispatch("comm.ingest", args).await.unwrap();
        assert_eq!(replay["deduplicated"], true);
        assert_eq!(replay["thread_id"], ack["thread_id"]);
        unchanged(&rt, &before).await;
        let token = rt.authorize(Namespace::local()).unwrap();
        assert_eq!(
            rt.notes(&token)
                .unwrap()
                .count_notes("local", Some("message"))
                .await
                .unwrap(),
            1
        );
    }
}
