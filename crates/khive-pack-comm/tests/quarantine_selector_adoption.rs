//! Caller controls for the typed quarantine selectors and delivered-count adoption.
//! Physical-row writes below model legacy storage and deterministic concurrent changes.

use std::time::Duration;

use khive_pack_comm::CommPack;
use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{Note, SqlStatement, SqlValue};
use serde_json::{json, Value};
use uuid::Uuid;

const ACTOR: &str = "selector-sender";
const AS_OF: i64 = 1_000;

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    pack: CommPack,
}

impl Fixture {
    fn new() -> Self {
        // Match the isolated runtime used by thread_cursor_kind: no ambient paths,
        // routing, credentials, embedding peers, or process-wide environment edits.
        let mut config = RuntimeConfig::no_embeddings();
        config.db_path = None;
        config.wal_ceiling_bytes = 0;
        config.wal_ceiling_configured_bytes = 0;
        config.wal_ceiling_source = khive_runtime::WalCeilingSource::Default;
        config.wal_ceiling_env_raw = None;
        config.disk_guard_environment = Default::default();
        config.disk_guard_config = None;
        config.volume_lock_dir = None;
        config.credentials.clear();
        config.visibility_receipts = None;
        config.mounts.clear();
        config.events_split = None;
        config.actor_id = Some(ACTOR.into());
        config.brain_profile = None;
        config.brain = Default::default();
        config.packs = vec!["kg".into(), "comm".into()];
        let runtime = KhiveRuntime::new(config).expect("isolated memory runtime");
        assert!(runtime.backend_data_dir().is_none());
        assert!(runtime.backend_ann_root().is_none());
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some(ACTOR.into()));
        builder.with_default_namespace("local");
        builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
        builder.register(CommPack::new(runtime.clone()));
        let registry = builder.build().expect("real comm registry");
        let pack =
            CommPack::new(runtime.clone()).with_quarantine_retention(Duration::from_micros(100));
        Self {
            runtime,
            registry,
            pack,
        }
    }

    async fn seed(&self, note: Note) {
        self.runtime
            .backend()
            .notes()
            .unwrap()
            .upsert_note(note)
            .await
            .unwrap();
    }

    async fn stored(&self, id: Uuid) -> Option<Note> {
        self.runtime
            .backend()
            .notes()
            .unwrap()
            .get_note_including_deleted(id)
            .await
            .unwrap()
    }

    async fn cleanup(&self, legacy: bool, as_of: i64) -> Result<Value, RuntimeError> {
        let token = self.runtime.authorize(Namespace::local()).unwrap();
        // Cleanup is an internal daemon subhandler, not a public registry verb.
        PackRuntime::dispatch(
            &self.pack,
            "comm.cleanup_expired_quarantine",
            json!({
                "channel_kind": "email",
                "channel_slug": if legacy { "" } else { "inbox" },
                "mode": if legacy { "legacy_slugless" } else { "channel" },
                "as_of_micros": as_of,
            }),
            &self.registry,
            &token,
        )
        .await
    }
}

fn quarantine(id: u128, created: i64, expiry: Option<i64>, slug: Option<Value>) -> Note {
    let mut properties = json!({ "channel_kind": "email", "quarantined": true });
    if let Some(slug) = slug {
        properties["channel_slug"] = slug;
    }
    let mut note = Note::new("local", "message", "quarantine fixture");
    note.id = Uuid::from_u128(id);
    note.created_at = created;
    note.updated_at = created;
    note.expires_at = expiry;
    note.properties = Some(properties);
    note
}

#[tokio::test]
async fn cleanup_selects_only_the_first_128_in_time_then_uuid_order_in_both_modes() {
    for legacy in [false, true] {
        let fixture = Fixture::new();
        for id in (1..=130).rev() {
            let time = if id == 130 { 50 } else { 100 };
            let expiry = if legacy && id % 2 == 0 {
                None
            } else {
                Some(time)
            };
            let mut note = quarantine(id, time, expiry, (!legacy).then(|| json!("inbox")));
            if id % 2 == 0 {
                note.properties.as_mut().unwrap()["quarantined"] = json!("true");
            }
            fixture.seed(note).await;
        }
        // These would sort before every eligible row if filtering were applied
        // after LIMIT, and therefore must not consume the bounded page.
        let mut excluded = Vec::new();
        for id in 200..205 {
            let mut note = quarantine(id, 1, Some(1), (!legacy).then(|| json!("inbox")));
            match id {
                200 => note.namespace = "foreign".into(),
                201 => note.kind = "observation".into(),
                202 => note.properties.as_mut().unwrap()["channel_kind"] = json!("telegram"),
                203 => note.properties.as_mut().unwrap()["channel_slug"] = json!("other"),
                204 => note.properties.as_mut().unwrap()["quarantined"] = json!(false),
                _ => unreachable!(),
            }
            excluded.push(note.clone());
            fixture.seed(note).await;
        }
        if !legacy {
            let mut tombstone = quarantine(205, 1, Some(1), Some(json!("inbox")));
            tombstone.deleted_at = Some(2);
            excluded.push(tombstone.clone());
            fixture.seed(tombstone).await;
        }

        let result = fixture.cleanup(legacy, AS_OF).await.unwrap();
        assert_eq!(result["deleted"], 128, "legacy={legacy}");
        assert_eq!(result["routed_owners_detached"], 0);
        for id in 1..=130 {
            let retained = fixture.stored(Uuid::from_u128(id)).await.is_some();
            assert_eq!(
                retained,
                matches!(id, 128 | 129),
                "id={id}, legacy={legacy}"
            );
        }
        for note in excluded {
            assert_eq!(fixture.stored(note.id).await, Some(note));
        }
    }
}

#[tokio::test]
async fn legacy_cleanup_keeps_fallback_or_ascii_slug_and_true_value_semantics() {
    let fixture = Fixture::new();
    // Explicit expiry admits a recently created row; absent expiry uses the
    // inclusive creation cutoff (1000 - 100). Both JSON true spellings work.
    let mut text_true = quarantine(2, 900, None, Some(Value::Null));
    text_true.properties.as_mut().unwrap()["quarantined"] = json!("true");
    let mut tombstone = quarantine(5, 100, Some(AS_OF), None);
    tombstone.deleted_at = Some(500);
    let eligible = [
        quarantine(1, 999, Some(AS_OF), None),
        text_true,
        quarantine(3, 900, None, Some(json!(""))),
        quarantine(4, 900, None, Some(json!("   "))),
        tombstone,
    ];
    for note in &eligible {
        fixture.seed(note.clone()).await;
    }
    let mut excluded = vec![
        quarantine(10, 1, Some(AS_OF + 1), None),
        quarantine(11, 901, None, None),
        quarantine(12, 1, Some(1), Some(json!("\t"))),
        quarantine(13, 1, Some(1), Some(json!("\u{2003}"))),
        quarantine(14, 1, Some(1), Some(json!(0))),
    ];
    for (id, value) in [(15, json!(false)), (16, json!("True")), (17, json!(1))] {
        let mut note = quarantine(id, 1, Some(1), None);
        note.properties.as_mut().unwrap()["quarantined"] = value;
        excluded.push(note);
    }
    for note in &excluded {
        fixture.seed(note.clone()).await;
    }

    assert_eq!(
        fixture.cleanup(true, AS_OF).await.unwrap()["deleted"],
        eligible.len()
    );
    for note in eligible {
        assert!(fixture.stored(note.id).await.is_none(), "{}", note.id);
    }
    for note in excluded {
        assert_eq!(fixture.stored(note.id).await, Some(note));
    }
}

#[tokio::test]
async fn legacy_cleanup_page_is_not_consumed_by_rows_not_yet_due() {
    let fixture = Fixture::new();
    // Without an expiry, a legacy row is due only once it was created at or
    // before the cutoff (1000 - 100). These 128 rows were created after it and
    // sort ahead of the due row, so selecting them would fill the bounded page.
    let mut not_due = Vec::new();
    for id in 1..=128 {
        let note = quarantine(id, 950, None, None);
        not_due.push(note.clone());
        fixture.seed(note).await;
    }
    let due = quarantine(500, 999, Some(AS_OF), None);
    fixture.seed(due.clone()).await;

    assert_eq!(fixture.cleanup(true, AS_OF).await.unwrap()["deleted"], 1);
    assert!(fixture.stored(due.id).await.is_none());
    for note in not_due {
        assert_eq!(fixture.stored(note.id).await, Some(note));
    }
}

#[tokio::test]
async fn cleanup_rereads_each_selected_id_after_an_earlier_delete() {
    let fixture = Fixture::new();
    let first = quarantine(1, 1, Some(1), Some(json!("inbox")));
    let later = quarantine(2, 1, Some(2), Some(json!("inbox")));
    fixture.seed(first.clone()).await;
    fixture.seed(later.clone()).await;
    // The real hard-delete transaction mutates the later candidate after page
    // selection. No scheduling race or production-only hook is required.
    fixture
        .runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute_script(format!(
            "CREATE TRIGGER advance_later_quarantine AFTER DELETE ON notes \
         WHEN OLD.id = '{}' BEGIN \
         UPDATE notes SET expires_at = 2000 WHERE id = '{}'; END;",
            first.id, later.id,
        ))
        .await
        .unwrap();

    assert_eq!(fixture.cleanup(false, AS_OF).await.unwrap()["deleted"], 1);
    assert!(fixture.stored(first.id).await.is_none());
    assert_eq!(
        fixture.stored(later.id).await.unwrap().expires_at,
        Some(2000)
    );
    assert_eq!(fixture.cleanup(false, 2000).await.unwrap()["deleted"], 1);
    assert!(fixture.stored(later.id).await.is_none());
}

#[tokio::test]
async fn malformed_later_selected_note_refuses_the_page_before_any_deletion() {
    let fixture = Fixture::new();
    let healthy = quarantine(1, 1, Some(1), Some(json!("inbox")));
    let corrupt = quarantine(2, 1, Some(2), Some(json!("inbox")));
    fixture.seed(healthy.clone()).await;
    fixture.seed(corrupt.clone()).await;
    // Unlike malformed properties JSON, BLOB content does not participate in
    // the selector. It fails only when the typed page decodes Note.content.
    fixture
        .runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement::new(
            "UPDATE notes SET content = ?1 WHERE id = ?2",
            vec![
                SqlValue::Blob(vec![0x61]),
                SqlValue::Text(corrupt.id.to_string()),
            ],
        ))
        .await
        .unwrap();

    let error = fixture
        .cleanup(false, AS_OF)
        .await
        .expect_err("typed page must refuse BLOB content");
    assert!(
        matches!(error.refusal_source(), RuntimeError::Storage(_)),
        "{error:?}"
    );
    assert_eq!(fixture.stored(healthy.id).await, Some(healthy));
    let mut reader = fixture.runtime.sql().reader().await.unwrap();
    let row = reader
        .query_row(SqlStatement::new(
            "SELECT COUNT(*) AS present FROM notes WHERE id = ?1 AND typeof(content) = 'blob'",
            vec![SqlValue::Text(corrupt.id.to_string())],
        ))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(row.get("present"), Some(SqlValue::Integer(1))));
}

#[tokio::test]
async fn delivered_counts_exact_live_inbound_sender_correlations() {
    let fixture = Fixture::new();
    let outbound_id = Uuid::from_u128(500);
    let mut originals = Vec::new();
    for id in 1..=9 {
        let mut note = Note::new("local", "message", "delivery fixture");
        note.id = Uuid::from_u128(id);
        let mut properties = json!({
            "direction": "inbound", "from_actor": ACTOR,
            "to_actor": "different-recipient", "outbound_ref": outbound_id,
        });
        match id {
            1 => {}
            2 => properties["to_actor"] = json!("another-recipient"),
            3 => {
                properties["from_actor"] = json!("someone-else");
                properties["to_actor"] = json!(ACTOR);
            }
            4 => properties["direction"] = json!("outbound"),
            5 => properties["outbound_ref"] = json!(Uuid::from_u128(501)),
            6 => note.namespace = "foreign".into(),
            7 => note.kind = "observation".into(),
            8 => note.deleted_at = Some(note.created_at),
            9 => {
                properties.as_object_mut().unwrap().remove("from_actor");
            }
            _ => unreachable!(),
        }
        note.properties = Some(properties);
        originals.push(note.clone());
        fixture.seed(note).await;
    }

    let result = fixture
        .registry
        .dispatch("comm.delivered", json!({"id": outbound_id}))
        .await
        .unwrap();
    assert_eq!(
        result,
        json!({
            "id": outbound_id, "status": "delivered", "delivered": true, "inbound_count": 2,
        })
    );
    for note in originals {
        assert_eq!(
            fixture.stored(note.id).await,
            Some(note),
            "count is read-only"
        );
    }
}

#[tokio::test]
async fn cold_delivered_acquires_notes_and_repairs_sequence_only_once() {
    let fixture = Fixture::new();
    // No seed/stored helper has acquired a NoteStore on this backend yet.
    assert_eq!(fixture.runtime.backend().notes_seq_repair_run_count(), 0);
    let outbound_id = Uuid::from_u128(500);
    for _ in 0..2 {
        let result = fixture
            .registry
            .dispatch("comm.delivered", json!({ "id": outbound_id }))
            .await
            .unwrap();
        assert_eq!(
            result,
            json!({
                "id": outbound_id,
                "status": "undelivered",
                "delivered": false,
                "inbound_count": 0,
            })
        );
        assert_eq!(fixture.runtime.backend().notes_seq_repair_run_count(), 1);
    }
}
