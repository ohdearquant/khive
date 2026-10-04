use std::sync::Arc;

use khive_db::stores::blob::FsBlobStore;
use khive_pack_comm::CommPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, RequestIdentity, RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{
    Attachment, AttachmentSubstrate, BlobStore, ContentRef, Note, SqlStatement, SqlValue,
};
use serde_json::{json, Value};
use uuid::Uuid;

const SENDER: &str = "actor:sender";
const RECIPIENT: &str = "actor:recipient";

struct Fixture {
    registry: VerbRegistry,
    runtime: KhiveRuntime,
    blobs: Arc<FsBlobStore>,
    objects: Vec<(ContentRef, u64)>,
    _root: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            actor_id: Some(SENDER.into()),
            brain_profile: None,
            wal_ceiling_env_raw: None,
            events_split: None,
            packs: vec!["kg".into(), "comm".into()],
            ..RuntimeConfig::no_embeddings()
        })
        .expect("isolated migrated runtime without an embedding service");
        runtime.attachments().expect("canonical attachment storage");
        let root = tempfile::tempdir().expect("private object root");
        let blobs = Arc::new(FsBlobStore::new(root.path().to_path_buf(), 0).expect("object store"));
        let mut objects = Vec::new();
        for bytes in [
            b"first parent object".as_slice(),
            b"second parent object",
            b"reply object",
        ] {
            objects.push((
                blobs
                    .put(bytes.to_vec())
                    .await
                    .expect("publish real object"),
                bytes.len() as u64,
            ));
        }
        assert_ne!(objects[0].0, objects[1].0);
        assert_ne!(objects[0].0, objects[2].0);
        runtime
            .install_blob_store(blobs.clone())
            .expect("install object store");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(CommPack::new(runtime.clone()));
        builder.with_default_namespace("local");
        builder.with_actor_id(Some(SENDER.into()));
        Self {
            registry: builder.build().expect("real kg+comm registry"),
            runtime,
            blobs,
            objects,
            _root: root,
        }
    }

    async fn call(&self, actor: &str, verb: &str, args: Value) -> Value {
        self.registry
            .dispatch_with_identity(verb, args, Some(identity(actor)))
            .await
            .unwrap_or_else(|error| {
                panic!("{verb} must complete through its actual handler: {error}")
            })
    }

    async fn parent(&self) -> Value {
        let sent = self
            .call(
                SENDER,
                "comm.send",
                json!({
                    "to": RECIPIENT, "content": "parent carries two distinct objects",
                    "attachments": refs(&self.objects[..2]), "idempotency_key": "lifecycle-parent",
                }),
            )
            .await;
        for owner in pair(&sent) {
            self.assert_rows(owner, &self.objects[..2]).await;
        }
        sent
    }

    async fn rows(&self, owner: Uuid) -> Vec<Attachment> {
        self.runtime
            .attachments()
            .unwrap()
            .list_attachments(owner)
            .await
            .expect("read real ownership rows")
    }

    async fn note(&self, owner: Uuid) -> Option<Note> {
        let token = self.runtime.authorize(Namespace::local()).unwrap();
        self.runtime
            .notes(&token)
            .unwrap()
            .get_note_including_deleted(owner)
            .await
            .expect("read physical message copy")
    }

    async fn assert_rows(&self, owner: Uuid, expected: &[(ContentRef, u64)]) {
        let rows = self.rows(owner).await;
        assert_eq!(
            rows.len(),
            expected.len(),
            "each physical copy owns only its supplied attachment set"
        );
        for (position, (reference, size)) in expected.iter().enumerate() {
            let role = format!("message-attachment:{position}");
            let row = rows
                .iter()
                .find(|row| row.role == role)
                .expect("ordered role exists");
            assert_eq!(row.record_uuid, owner);
            assert_eq!(row.substrate, AttachmentSubstrate::Note);
            assert_eq!(&row.content_ref, reference);
            assert_eq!(row.size_bytes, Some(*size));
            assert_eq!(row.media_type, None);
        }
    }

    async fn assert_objects_exist(&self, expected: &[(ContentRef, u64)]) {
        for (reference, _) in expected {
            assert!(
                self.blobs.exists(reference).await.unwrap(),
                "message deletion preserves the real object still owned by its sibling"
            );
        }
    }

    async fn set_pair_time(&self, receipt: &Value, at: i64) {
        // Fix the fixture's sort keys after real publication; no scheduler or wall-clock assumption.
        let [outbound, inbound] = pair(receipt);
        let mut writer = self.runtime.sql().writer().await.expect("fixture writer");
        let changed = writer
            .execute(SqlStatement {
                sql: "UPDATE notes SET created_at = ?1 WHERE id IN (?2, ?3)".into(),
                params: vec![
                    SqlValue::Integer(at),
                    SqlValue::Text(outbound.to_string()),
                    SqlValue::Text(inbound.to_string()),
                ],
                label: Some("comm-attachment-lifecycle-sort-keys".into()),
            })
            .await
            .expect("set both physical copies' fixture times");
        assert_eq!(changed, 2);
    }
}

fn identity(actor: &str) -> RequestIdentity {
    RequestIdentity {
        namespace: "local".into(),
        actor_id: Some(actor.into()),
        ..Default::default()
    }
}

fn refs(objects: &[(ContentRef, u64)]) -> Vec<&str> {
    objects
        .iter()
        .map(|(reference, _)| reference.as_str())
        .collect()
}

fn pair(receipt: &Value) -> [Uuid; 2] {
    let ids = ["full_id", "recipient_id"].map(|field| {
        receipt[field]
            .as_str()
            .expect("keyed receipt carries a full copy identifier")
            .parse()
            .expect("canonical full UUID")
    });
    assert_ne!(
        ids[0], ids[1],
        "actual publication creates two distinct physical copies"
    );
    ids
}

fn metadata(objects: &[(ContentRef, u64)]) -> Value {
    Value::Array(objects.iter().map(|(reference, size)| json!({"content_ref": reference.as_str(), "size": size, "media_type": null})).collect())
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn thread_orders_keep_disjoint_message_attachment_sets() {
    let fixture = Fixture::new().await;
    let parent = fixture.parent().await;
    let parent_ids = pair(&parent);
    let reply = fixture
        .call(
            RECIPIENT,
            "comm.reply",
            json!({
                "id": parent_ids[1].to_string(), "content": "reply carries a separate object",
                "attachments": refs(&fixture.objects[2..]), "idempotency_key": "lifecycle-reply",
            }),
        )
        .await;
    let reply_ids = pair(&reply);
    assert_eq!(parent["thread_id"], reply["thread_id"]);
    for owner in reply_ids {
        fixture.assert_rows(owner, &fixture.objects[2..]).await;
    }
    fixture.set_pair_time(&parent, 10_000_000).await;
    fixture.set_pair_time(&reply, 20_000_000).await;

    for (actor, root) in [(SENDER, parent_ids[0]), (RECIPIENT, parent_ids[1])] {
        for order in ["asc", "desc"] {
            let thread = fixture.call(actor, "comm.thread", json!({
                "id": root.to_string(), "order": order, "fields": ["full_id", "content", "attachments"],
            })).await;
            assert_eq!(
                thread["count"], 2,
                "thread folds each published dual-write pair once"
            );
            let expected = if order == "asc" {
                [parent_ids[0], reply_ids[0]]
            } else {
                [reply_ids[0], parent_ids[0]]
            };
            let messages = thread["messages"].as_array().expect("actual thread rows");
            assert_eq!(
                messages
                    .iter()
                    .map(|row| row["full_id"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>(),
                expected.iter().map(Uuid::to_string).collect::<Vec<_>>(),
                "requested thread order is preserved for attached parent and reply"
            );
            for row in messages {
                let is_parent = row["full_id"] == parent_ids[0].to_string();
                let expected_objects = if is_parent {
                    &fixture.objects[..2]
                } else {
                    &fixture.objects[2..]
                };
                assert_eq!(
                    row["attachments"],
                    metadata(expected_objects),
                    "thread metadata belongs to this logical message's canonical owner"
                );
                assert_eq!(
                    row["content"],
                    if is_parent {
                        "parent carries two distinct objects"
                    } else {
                        "reply carries a separate object"
                    }
                );
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn reply_without_attachments_does_not_inherit_attached_parent() {
    for explicit_empty in [false, true] {
        let fixture = Fixture::new().await;
        let parent = fixture.parent().await;
        let parent_ids = pair(&parent);
        let parent_rows = [
            fixture.rows(parent_ids[0]).await,
            fixture.rows(parent_ids[1]).await,
        ];
        let mut args = json!({"id": parent_ids[1].to_string(), "content": "reply supplies no files", "idempotency_key": "no-inheritance"});
        if explicit_empty {
            args["attachments"] = json!([]);
        }
        let reply = fixture.call(RECIPIENT, "comm.reply", args).await;
        let reply_ids = pair(&reply);
        for owner in reply_ids {
            fixture.assert_rows(owner, &[]).await;
        }
        let read = fixture
            .call(
                SENDER,
                "comm.read",
                json!({"id": reply_ids[1].to_string(), "body": true}),
            )
            .await;
        assert_eq!(
            read["attachments"],
            json!([]),
            "attachment-free reply view cannot inherit parent files"
        );
        for (owner, before) in parent_ids.into_iter().zip(parent_rows) {
            assert_eq!(
                fixture.rows(owner).await,
                before,
                "reply never moves or replaces parent ownership rows"
            );
        }
        fixture.assert_objects_exist(&fixture.objects[..2]).await;
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn soft_delete_keeps_message_copy_attachment_owners() {
    for selected_copy in [0, 1] {
        let fixture = Fixture::new().await;
        let parent = fixture.parent().await;
        let ids = pair(&parent);
        let before = [fixture.rows(ids[0]).await, fixture.rows(ids[1]).await];
        let actor = if selected_copy == 0 {
            SENDER
        } else {
            RECIPIENT
        };
        let deleted = fixture
            .call(
                actor,
                "delete",
                json!({"id": ids[selected_copy].to_string(), "hard": false}),
            )
            .await;
        assert_eq!(deleted["deleted"], true);
        for (owner, expected) in ids.into_iter().zip(before) {
            assert_eq!(
                fixture.rows(owner).await,
                expected,
                "soft deletion retains both copies' exact ownership rows"
            );
        }
        assert!(fixture
            .note(ids[selected_copy])
            .await
            .unwrap()
            .deleted_at
            .is_some());
        assert!(fixture
            .note(ids[1 - selected_copy])
            .await
            .unwrap()
            .deleted_at
            .is_none());
        fixture.assert_objects_exist(&fixture.objects[..2]).await;
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn hard_delete_releases_only_target_copy_attachment_owners() {
    for selected_copy in [0, 1] {
        for soft_first in [false, true] {
            let fixture = Fixture::new().await;
            let parent = fixture.parent().await;
            let ids = pair(&parent);
            let target = ids[selected_copy];
            let sibling = ids[1 - selected_copy];
            let actor = if selected_copy == 0 {
                SENDER
            } else {
                RECIPIENT
            };
            let sibling_rows = fixture.rows(sibling).await;
            let sibling_note = serde_json::to_value(fixture.note(sibling).await.unwrap()).unwrap();
            if soft_first {
                fixture
                    .call(
                        actor,
                        "delete",
                        json!({"id": target.to_string(), "hard": false}),
                    )
                    .await;
                fixture.assert_rows(target, &fixture.objects[..2]).await;
            }
            let deleted = fixture
                .call(
                    actor,
                    "delete",
                    json!({"id": target.to_string(), "hard": true}),
                )
                .await;
            assert_eq!(deleted["deleted"], true);
            assert!(
                fixture.rows(target).await.is_empty(),
                "hard deletion releases the selected physical copy's attachment rows"
            );
            assert_eq!(
                fixture.rows(sibling).await,
                sibling_rows,
                "hard deletion preserves the sibling copy's exact attachment ownership"
            );
            assert_eq!(
                serde_json::to_value(fixture.note(sibling).await.unwrap()).unwrap(),
                sibling_note,
                "hard deletion leaves the other physical message unchanged"
            );
            assert!(
                fixture.note(target).await.is_none(),
                "hard deletion removes the selected note"
            );
            fixture.assert_objects_exist(&fixture.objects[..2]).await;
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn hard_delete_attachment_failure_rolls_back_note_and_owners() {
    for selected_copy in [0, 1] {
        let fixture = Fixture::new().await;
        let parent = fixture.parent().await;
        let ids = pair(&parent);
        let target = ids[selected_copy];
        let actor = if selected_copy == 0 {
            SENDER
        } else {
            RECIPIENT
        };
        let before_rows = [fixture.rows(ids[0]).await, fixture.rows(ids[1]).await];
        let before_notes = [
            serde_json::to_value(fixture.note(ids[0]).await.unwrap()).unwrap(),
            serde_json::to_value(fixture.note(ids[1]).await.unwrap()).unwrap(),
        ];
        {
            let mut writer = fixture
                .runtime
                .sql()
                .writer()
                .await
                .expect("isolated refusal fixture writer");
            writer.execute(SqlStatement {
                sql: format!("CREATE TRIGGER refuse_lifecycle_attachment_delete BEFORE DELETE ON attachments WHEN OLD.record_uuid = '{target}' BEGIN SELECT RAISE(ABORT, 'lifecycle_attachment_delete_refused'); END"),
                params: vec![],
                label: Some("comm-attachment-lifecycle-delete-refusal".into()),
            }).await.expect("install actual attachment-deletion refusal");
        }
        let error = fixture
            .registry
            .dispatch_with_identity(
                "delete",
                json!({"id": target.to_string(), "hard": true}),
                Some(identity(actor)),
            )
            .await
            .expect_err("hard delete must propagate the actual attachment deletion failure");
        assert!(
            error
                .to_string()
                .contains("lifecycle_attachment_delete_refused"),
            "failure comes from the reached attachment deletion statement"
        );
        for ((owner, rows), note) in ids.into_iter().zip(before_rows).zip(before_notes) {
            assert_eq!(
                fixture.rows(owner).await,
                rows,
                "refused hard delete rolls back attachment ownership changes"
            );
            assert_eq!(
                serde_json::to_value(fixture.note(owner).await.unwrap()).unwrap(),
                note,
                "refused attachment deletion rolls back the preceding note deletion"
            );
        }
        fixture.assert_objects_exist(&fixture.objects[..2]).await;
    }
}
