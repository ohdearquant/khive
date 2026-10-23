//! Actual dispatch coverage for configured quarantine mailbox visibility.

use super::*;
use khive_runtime::{ActorRef, GateRef, MailboxReadGate, RuntimeError};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

const MAINTAINER: &str = "lambda:am6-maintainer";
const LANDING: &str = "lambda:am6-landing";
const OTHER: &str = "lambda:am6-other";
const HELPER: &str = "lambda:am6-reader";
const TERM: &str = "am6visibilitytoken";
const EPOCH: i64 = 1_700_000_000_000_000;

struct Am6Fixture {
    registry: VerbRegistry,
    runtime: KhiveRuntime,
}

impl Am6Fixture {
    fn new(maintainer: Option<&str>, delegated_owner: Option<&str>) -> Self {
        Self::from_backend(shared_backend(), maintainer, delegated_owner)
    }

    fn from_backend(
        backend: Arc<khive_db::StorageBackend>,
        maintainer: Option<&str>,
        delegated_owner: Option<&str>,
    ) -> Self {
        let inner: GateRef = Arc::new(AllowAllGate);
        let gate: GateRef = match delegated_owner {
            Some(owner) => Arc::new(
                MailboxReadGate::new(
                    inner,
                    ActorRef::new("actor", owner),
                    vec![ActorRef::new("actor", HELPER)],
                )
                .expect("explicit trusted-local mailbox grant"),
            ),
            None => inner,
        };
        let runtime = KhiveRuntime::from_backend(
            backend,
            RuntimeConfig {
                actor_id: maintainer.map(str::to_string),
                gate: gate.clone(),
                packs: vec!["kg".into(), "comm".into()],
                ..RuntimeConfig::no_embeddings()
            },
        );
        let mut builder = VerbRegistryBuilder::new();
        khive_runtime::PackRegistry::register_packs(
            &["kg".into(), "comm".into()],
            runtime.clone(),
            &mut builder,
        )
        .expect("real KG and comm factory registration");
        builder.with_gate(gate);
        builder.with_actor_id(maintainer.map(str::to_string));
        let registry = builder.build().expect("actual mailbox registry");
        registry
            .apply_schema_plans_with_map(&Default::default(), runtime.backend())
            .expect("comm schema bootstrap");
        runtime.install_edge_rules(registry.all_edge_rules());
        registry.call_register_entity_type_validators(&runtime);
        Self { registry, runtime }
    }

    async fn call(
        &self,
        actor: Option<&str>,
        verb: &str,
        args: Value,
    ) -> Result<Value, RuntimeError> {
        self.registry
            .dispatch_with_identity(
                verb,
                args,
                Some(RequestIdentity {
                    namespace: "local".into(),
                    actor_id: actor.map(str::to_string),
                    ..Default::default()
                }),
            )
            .await
    }

    async fn anchor(&self) -> Uuid {
        let result = self
            .call(
                Some(MAINTAINER),
                "create",
                json!({"kind":"concept", "name":"Quarantine visibility anchor", "skip_dedup_check":true}),
            )
            .await
            .expect("actual concept producer");
        result["id"].as_str().unwrap().parse().unwrap()
    }

    async fn seed(&self, anchor: Uuid, label: &str, mut properties: Value, time: i64) -> Note {
        let token = self.runtime.authorize(Namespace::local()).unwrap();
        let mut note = self
            .runtime
            .create_note(
                &token,
                "message",
                None,
                &format!("{TERM} {label}"),
                None,
                Some(json!({})),
                vec![anchor],
            )
            .await
            .expect("real indexed message and annotation producer");
        properties["subject"] = json!(label);
        properties["origin"] = json!("preexisting-channel-fixture");
        properties["content_ref"] = json!(format!("unchanged-reference-{label}"));
        properties["read"] = json!(false);
        if properties
            .get("quarantined")
            .is_some_and(|value| value == &json!(true) || value == &json!("true"))
        {
            // Preserve actual stored reference/retention fields without
            // requiring an original blob or claiming that one was fetched.
            properties["quarantine_content_ref"] = json!("00".repeat(32));
            properties["quarantine_original_retained"] = json!("true");
            note.expires_at = Some(2_000_000_000_000_000);
        }
        note.properties = Some(properties);
        note.updated_at = time;
        // This embedder store represents an already persisted channel row. The
        // ordinary create above, rather than this upsert, supplies its text index.
        self.runtime
            .backend()
            .notes()
            .unwrap()
            .upsert_note(note.clone())
            .await
            .expect("install preexisting routing and quarantine metadata");
        // NOTE_UPSERT_SQL preserves an existing created_at. Install this
        // preexisting fixture timestamp explicitly, then reflect the real row.
        let sql = self.runtime.backend().sql();
        let mut writer = sql.writer().await.expect("trusted fixture SQL writer");
        let changed = writer
            .execute(SqlStatement {
                sql: "UPDATE notes SET created_at = ?1 WHERE id = ?2 AND namespace = ?3 AND kind = 'message' AND deleted_at IS NULL".into(),
                params: vec![
                    SqlValue::Integer(time),
                    SqlValue::Text(note.id.to_string()),
                    SqlValue::Text("local".into()),
                ],
                label: Some("am6-fixture-creation-time".into()),
            })
            .await
            .expect("set actual persisted fixture creation time");
        assert_eq!(
            changed, 1,
            "timestamp update must target the actual seeded message"
        );
        drop(writer);
        let stored = self.stored(note.id).await;
        assert_eq!(
            stored.created_at, time,
            "reflect actual persisted creation time"
        );
        assert_eq!(stored.updated_at, time);
        stored
    }

    async fn stored(&self, id: Uuid) -> Note {
        self.runtime
            .backend()
            .notes()
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap()
    }

    async fn snapshots(&self, notes: &[Note]) -> BTreeMap<String, Value> {
        let mut snapshots = BTreeMap::new();
        for note in notes {
            snapshots.insert(
                note.id.to_string(),
                serde_json::to_value(self.stored(note.id).await).unwrap(),
            );
        }
        snapshots
    }
}

fn am6_props(recipient: &str, thread: Uuid, marker: Option<Value>) -> Value {
    let mut properties = json!({
        "direction":"inbound", "to_actor":recipient,
        "from_actor":"external:am6-sender", "thread_id":thread,
    });
    if let Some(marker) = marker {
        properties["quarantined"] = marker;
    }
    properties
}

struct Am6Rows {
    anchor: Uuid,
    thread: Uuid,
    all: Vec<Note>,
    ordinary: BTreeMap<String, Vec<Uuid>>,
    quarantine: Vec<Uuid>,
}

async fn am6_rows(fixture: &Am6Fixture) -> Am6Rows {
    let anchor = fixture.anchor().await;
    let thread = Uuid::new_v4();
    let mut all = Vec::new();
    let mut ordinary = BTreeMap::new();
    for (index, actor) in [MAINTAINER, LANDING, OTHER, "local"]
        .into_iter()
        .enumerate()
    {
        let mut ids = Vec::new();
        for (label, marker) in [("ordinary", None), ("numeric-one", Some(json!(1)))] {
            let note = fixture
                .seed(
                    anchor,
                    &format!("{actor}-{label}"),
                    am6_props(actor, thread, marker),
                    EPOCH + index as i64,
                )
                .await;
            ids.push(note.id);
            all.push(note);
        }
        ordinary.insert(actor.to_string(), ids);
    }
    let mut quarantine = Vec::new();
    for (label, marker) in [
        ("boolean-quarantine", json!(true)),
        ("string-quarantine", json!("true")),
    ] {
        let note = fixture
            .seed(
                anchor,
                label,
                am6_props(LANDING, thread, Some(marker)),
                EPOCH + 10,
            )
            .await;
        quarantine.push(note.id);
        all.push(note);
    }
    // comm.thread accepts an existing root note UUID and derives its stored
    // canonical thread_id; the canonical label itself is not a stored note ID.
    let root = all[0].id;
    Am6Rows {
        anchor,
        thread: root,
        all,
        ordinary,
        quarantine,
    }
}

fn am6_expected(rows: &Am6Rows, actor: &str, quarantine_owner: &str) -> BTreeSet<Uuid> {
    let mut ids: BTreeSet<_> = rows.ordinary[actor].iter().copied().collect();
    if actor == quarantine_owner {
        ids.extend(rows.quarantine.iter().copied());
    }
    ids
}

fn am6_ids(rows: &Value, key: &str) -> BTreeSet<Uuid> {
    rows.as_array()
        .expect("real reader returns an array")
        .iter()
        .map(|row| {
            row[key]
                .as_str()
                .expect("reader supplies its identifier")
                .parse()
                .unwrap()
        })
        .collect()
}

fn am6_message_ids(response: &Value) -> BTreeSet<Uuid> {
    am6_ids(&response["messages"], "full_id")
}

fn am6_assert_hidden(response: &Value, notes: &[Note], visible: &BTreeSet<Uuid>) {
    let encoded = response.to_string();
    for note in notes.iter().filter(|note| !visible.contains(&note.id)) {
        assert!(
            !encoded.contains(&note.id.to_string()),
            "hidden identifier in {response}"
        );
        assert!(
            !encoded.contains(&note.content),
            "hidden content in {response}"
        );
    }
}

async fn am6_assert_readers(
    fixture: &Am6Fixture,
    rows: &Am6Rows,
    actor: Option<&str>,
    visible: &BTreeSet<Uuid>,
) {
    // Every actor has both an ordinary and a numeric-one positive control.
    assert!(visible.len() >= 2);
    let inbox = fixture
        .call(actor, "comm.inbox", json!({"status":"all","limit":100}))
        .await
        .unwrap();
    assert_eq!(am6_message_ids(&inbox), *visible, "inbox actor={actor:?}");
    assert_eq!(inbox["count"].as_u64(), Some(visible.len() as u64));
    am6_assert_hidden(&inbox, &rows.all, visible);

    let mut unread = 0_u64;
    for id in visible {
        if fixture.stored(*id).await.properties.unwrap()["read"] != true {
            unread += 1;
        }
    }
    let count = fixture.call(actor, "comm.unread", json!({})).await.unwrap();
    assert_eq!(count["count"], unread, "unread actor={actor:?}");
    assert_eq!(inbox["unread_count"], unread);
    assert_eq!(count["count_saturated"], false);

    let thread = fixture
        .call(actor, "comm.thread", json!({"id":rows.thread,"limit":100}))
        .await
        .unwrap();
    assert_eq!(am6_message_ids(&thread), *visible, "thread actor={actor:?}");
    am6_assert_hidden(&thread, &rows.all, visible);

    let listed = fixture
        .call(actor, "list", json!({"kind":"message","limit":100}))
        .await
        .unwrap();
    assert_eq!(
        am6_ids(&listed["items"], "id"),
        *visible,
        "list actor={actor:?}"
    );
    am6_assert_hidden(&listed, &rows.all, visible);
    let searched = fixture
        .call(
            actor,
            "search",
            json!({"kind":"message","query":TERM,"limit":100}),
        )
        .await
        .unwrap();
    assert_eq!(
        am6_ids(&searched, "id"),
        *visible,
        "indexed search actor={actor:?}"
    );
    am6_assert_hidden(&searched, &rows.all, visible);
    let queried = fixture
        .call(
            actor,
            "query",
            json!({"query":"MATCH (m:message) RETURN m.id","page_size":100}),
        )
        .await
        .unwrap();
    assert_eq!(
        am6_ids(&queried["rows"], "m_id"),
        *visible,
        "query actor={actor:?}"
    );
    am6_assert_hidden(&queried, &rows.all, visible);

    let neighbors = fixture.call(actor, "neighbors", json!({"id":rows.anchor,"direction":"in","relations":["annotates"],"projection":"record","limit":100})).await.unwrap();
    assert_eq!(
        am6_ids(&neighbors["neighbors"], "id"),
        *visible,
        "neighbors actor={actor:?}"
    );
    am6_assert_hidden(&neighbors, &rows.all, visible);
    let context = fixture.call(actor, "context", json!({"entity_ids":[rows.anchor],"hops":1,"direction":"both","fanout":100,"budget":65536})).await.unwrap();
    assert_eq!(
        am6_ids(&context["edges"], "source_id"),
        *visible,
        "context actor={actor:?}"
    );
    am6_assert_hidden(&context, &rows.all, visible);

    for note in &rows.all {
        let result = fixture.call(actor, "get", json!({"id":note.id})).await;
        if visible.contains(&note.id) {
            let got = result.expect("visible direct get");
            assert_eq!(got["id"], note.id.to_string());
            assert_eq!(got["content"], note.content);
        } else {
            assert!(
                matches!(result, Err(RuntimeError::NotFound(_))),
                "hidden get actor={actor:?}, id={}: {result:?}",
                note.id
            );
        }
    }
}

async fn am6_assert_reads(
    fixture: &Am6Fixture,
    rows: &Am6Rows,
    actor: Option<&str>,
    visible: &BTreeSet<Uuid>,
) {
    for body in [false, true] {
        for note in &rows.all {
            let before = fixture.stored(note.id).await;
            let result = fixture
                .call(actor, "comm.read", json!({"id":note.id,"body":body}))
                .await;
            if visible.contains(&note.id) {
                let result = result.expect("visible inbound message can be marked read");
                assert_eq!(result["status"], "success");
                assert_eq!(result["read"], true);
                if body {
                    assert_eq!(result["content"], note.content);
                } else {
                    assert!(
                        result.get("content").is_none(),
                        "body=false must omit content"
                    );
                }
                let after = fixture.stored(note.id).await;
                let mut expected = before.properties.unwrap();
                expected["read"] = json!(true);
                assert_eq!(
                    after.properties,
                    Some(expected),
                    "read only changes the explicit read flag"
                );
                assert_eq!(after.id, before.id);
                assert_eq!(after.namespace, before.namespace);
                assert_eq!(after.kind, before.kind);
                assert_eq!(after.content, before.content);
                assert_eq!(after.name, before.name);
                assert_eq!(after.key, before.key);
                assert_eq!(after.expires_at, before.expires_at);
            } else {
                let error = result.expect_err("hidden quarantine and foreign rows refuse read");
                assert!(
                    error.to_string().contains("not addressed to caller actor"),
                    "{error:?}"
                );
                assert_eq!(
                    serde_json::to_value(fixture.stored(note.id).await).unwrap(),
                    serde_json::to_value(before).unwrap(),
                    "refused read must not mutate the note"
                );
            }
        }
    }
}

#[tokio::test]
async fn am6_fixed_maintainer_actual_dispatch_matrix() {
    let fixture = Am6Fixture::new(Some(MAINTAINER), None);
    let rows = am6_rows(&fixture).await;
    for actor in [Some(MAINTAINER), Some(LANDING), Some(OTHER), None] {
        let owner = actor.unwrap_or("local");
        let visible = am6_expected(&rows, owner, MAINTAINER);
        am6_assert_readers(&fixture, &rows, actor, &visible).await;
        am6_assert_reads(&fixture, &rows, actor, &visible).await;
    }
}

#[tokio::test]
async fn am6_maintainer_read_permission_does_not_authorize_reply() {
    let fixture = Am6Fixture::new(Some(MAINTAINER), None);
    let anchor = fixture.anchor().await;
    let sent = fixture
        .call(
            Some(OTHER),
            "comm.send",
            json!({"to":MAINTAINER,"content":"ordinary reply control","idempotency_key":"am6-ordinary-reply-control"}),
        )
        .await
        .unwrap();
    let outbound = fixture
        .stored(sent["full_id"].as_str().unwrap().parse().unwrap())
        .await;
    let inbound = outbound.properties.unwrap()["inbound_ref"]
        .as_str()
        .unwrap()
        .to_string();
    let replied = fixture
        .call(
            Some(MAINTAINER),
            "comm.reply",
            json!({"id":inbound,"content":"healthy ordinary reply"}),
        )
        .await
        .expect("ordinary participant can reply through the same registry");
    assert!(replied["full_id"].as_str().unwrap().parse::<Uuid>().is_ok());
    let token = fixture.runtime.authorize(Namespace::local()).unwrap();
    for (label, marker) in [
        ("reply-boolean", json!(true)),
        ("reply-string", json!("true")),
    ] {
        let q = fixture
            .seed(
                anchor,
                label,
                am6_props(LANDING, Uuid::new_v4(), Some(marker)),
                EPOCH,
            )
            .await;
        for body in [false, true] {
            let read = fixture
                .call(
                    Some(MAINTAINER),
                    "comm.read",
                    json!({"id":q.id,"body":body}),
                )
                .await
                .unwrap();
            assert_eq!(read["status"], "success");
        }
        let mut before = fixture
            .runtime
            .list_notes(&token, Some("message"), 200, 0)
            .await
            .unwrap();
        before.sort_by_key(|note| note.id);
        let error = fixture
            .call(
                Some(MAINTAINER),
                "comm.reply",
                json!({"id":q.id,"content":"must not be sent"}),
            )
            .await
            .expect_err("quarantine read access does not grant reply participation");
        assert!(
            error
                .to_string()
                .contains("not addressed to or from caller actor"),
            "{error:?}"
        );
        let mut after = fixture
            .runtime
            .list_notes(&token, Some("message"), 200, 0)
            .await
            .unwrap();
        after.sort_by_key(|note| note.id);
        assert_eq!(
            serde_json::to_value(after).unwrap(),
            serde_json::to_value(before).unwrap(),
            "refused reply creates or changes no message row"
        );
    }
}

fn am6_file_backend(path: &Path, locks: &Path) -> Arc<khive_db::StorageBackend> {
    let backend = khive_db::StorageBackend::sqlite_for_test_with_journal_mode_in(
        path,
        true,
        Duration::from_secs(2),
        locks.to_path_buf(),
    )
    .expect("private file-backed database");
    backend.prepare_core_schema().expect("current core schema");
    Arc::new(backend)
}

#[tokio::test]
async fn am6_no_configuration_preserves_reads_and_persisted_reopen_applies_maintainer() {
    let scratch = tempfile::tempdir().unwrap();
    let path = scratch.path().join("mailbox.db");
    let locks = scratch.path().join("locks");
    std::fs::create_dir(&locks).unwrap();
    let fixture = Am6Fixture::from_backend(am6_file_backend(&path, &locks), None, None);
    let rows = am6_rows(&fixture).await;
    for actor in [Some(MAINTAINER), Some(LANDING), Some(OTHER), None] {
        let visible = am6_expected(&rows, actor.unwrap_or("local"), LANDING);
        am6_assert_readers(&fixture, &rows, actor, &visible).await;
        am6_assert_reads(&fixture, &rows, actor, &visible).await;
    }
    let persisted = fixture.snapshots(&rows.all).await;
    drop(fixture);

    let reopened =
        Am6Fixture::from_backend(am6_file_backend(&path, &locks), Some(MAINTAINER), None);
    assert_eq!(
        reopened.snapshots(&rows.all).await,
        persisted,
        "reopening with M does not rewrite stored rows"
    );
    for actor in [Some(MAINTAINER), Some(LANDING), Some(OTHER), None] {
        let visible = am6_expected(&rows, actor.unwrap_or("local"), MAINTAINER);
        am6_assert_readers(&reopened, &rows, actor, &visible).await;
        am6_assert_reads(&reopened, &rows, actor, &visible).await;
    }
}

#[tokio::test]
async fn am6_delegated_views_follow_selected_owner_without_granting_read_or_generic_access() {
    for owner in [MAINTAINER, LANDING] {
        let fixture = Am6Fixture::new(Some(MAINTAINER), Some(owner));
        let rows = am6_rows(&fixture).await;
        let visible = am6_expected(&rows, owner, MAINTAINER);
        for (verb, args) in [
            (
                "comm.inbox",
                json!({"mailbox_actor":owner,"status":"all","limit":100}),
            ),
            (
                "comm.thread",
                json!({"mailbox_actor":owner,"id":rows.thread,"limit":100}),
            ),
        ] {
            let response = fixture
                .call(Some(HELPER), verb, args)
                .await
                .expect("actual mailbox Gate grants the selected owner");
            assert_eq!(
                am6_message_ids(&response),
                visible,
                "delegated owner={owner}, verb={verb}"
            );
            am6_assert_hidden(&response, &rows.all, &visible);
        }
        let own = fixture
            .call(Some(HELPER), "comm.inbox", json!({"status":"all"}))
            .await
            .unwrap();
        assert_eq!(
            own["count"], 0,
            "a grant does not replace the caller's own mailbox"
        );
        for id in &rows.quarantine {
            let before = fixture.stored(*id).await;
            assert!(matches!(
                fixture.call(Some(HELPER), "get", json!({"id":id})).await,
                Err(RuntimeError::NotFound(_))
            ));
            for body in [false, true] {
                let error = fixture
                    .call(Some(HELPER), "comm.read", json!({"id":id,"body":body}))
                    .await
                    .expect_err("delegated reads never grant mark-read");
                assert!(error.to_string().contains("not addressed to caller actor"));
            }
            assert_eq!(
                serde_json::to_value(fixture.stored(*id).await).unwrap(),
                serde_json::to_value(before).unwrap()
            );
        }
    }
}

#[tokio::test]
async fn am6_local_maintainer_counts_and_pages_merge_quarantine_once_before_offset() {
    let fixture = Am6Fixture::new(Some("local"), None);
    let anchor = fixture.anchor().await;
    let thread = Uuid::new_v4();
    let mut notes = Vec::new();
    for (index, (recipient, marker, direction)) in [
        (Some(json!("local")), None, "inbound"),
        (Some(json!("local")), Some(json!(1)), "inbound"),
        (None, Some(json!(true)), "inbound"),
        (Some(Value::Null), Some(json!("true")), "inbound"),
        (Some(json!(LANDING)), Some(json!(true)), "outbound"),
        (
            Some(json!({"malformed":"recipient"})),
            Some(json!("true")),
            "unknown",
        ),
        (None, None, "inbound"),
        (Some(Value::Null), None, "inbound"),
        (Some(json!(OTHER)), None, "inbound"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut p =
            json!({"from_actor":"external:am6-sender","direction":direction,"thread_id":thread});
        if let Some(recipient) = recipient {
            p["to_actor"] = recipient;
        }
        if let Some(marker) = marker {
            p["quarantined"] = marker;
        }
        notes.push(
            fixture
                .seed(
                    anchor,
                    &format!("page-{index}"),
                    p,
                    EPOCH + (index / 2) as i64,
                )
                .await,
        );
    }
    for (actor, included) in [(Some("local"), 6_usize), (None, 8_usize)] {
        let mut expected: Vec<_> = notes[..included].iter().collect();
        expected.sort_by_key(|note| (std::cmp::Reverse(note.created_at), note.id));
        let expected: Vec<_> = expected.into_iter().map(|note| note.id).collect();
        let count = fixture.call(actor, "comm.unread", json!({})).await.unwrap();
        assert_eq!(
            count["count"], included as u64,
            "each Q row belongs to exactly one count arm"
        );
        assert_eq!(count["count_saturated"], false);
        let count_only = fixture
            .call(actor, "comm.inbox", json!({"limit":0}))
            .await
            .unwrap();
        assert_eq!(count_only["unread_count"], included as u64);
        assert_eq!(count_only["messages"], json!([]));
        let mut actual = Vec::new();
        for offset in (0..=included).step_by(2) {
            let page = fixture
                .call(
                    actor,
                    "comm.inbox",
                    json!({"status":"all","limit":2,"offset":offset}),
                )
                .await
                .unwrap();
            assert_eq!(page["unread_count"], included as u64);
            let ids: Vec<Uuid> = page["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["full_id"].as_str().unwrap().parse().unwrap())
                .collect();
            assert_eq!(page["count"], ids.len() as u64);
            actual.extend(ids);
        }
        assert_eq!(
            actual, expected,
            "global created_at DESC/id ASC precedes offset and limit"
        );
    }
}

fn am6_normalize_thread_ids(value: &mut Value, ids: &[(Uuid, &str)]) {
    match value {
        Value::String(text) => {
            for (id, replacement) in ids {
                let full = id.to_string();
                if text == &full || text == &full[..8] {
                    *text = (*replacement).to_string();
                    break;
                }
            }
        }
        Value::Array(values) => values
            .iter_mut()
            .for_each(|value| am6_normalize_thread_ids(value, ids)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|value| am6_normalize_thread_ids(value, ids)),
        _ => {}
    }
}

#[tokio::test]
async fn am6_hidden_quarantine_thread_root_preserves_ordinary_foreign_root_parity() {
    let fixture = Am6Fixture::new(Some(MAINTAINER), None);
    let anchor = fixture.anchor().await;
    let mut pairs = Vec::new();
    for marker in [None, Some(json!(true))] {
        let canonical = Uuid::new_v4();
        let root = fixture
            .seed(
                anchor,
                "foreign-root",
                am6_props(LANDING, canonical, marker),
                EPOCH,
            )
            .await;
        assert_ne!(
            canonical, root.id,
            "stored canonical thread differs from root UUID"
        );
        let reply = fixture
            .seed(
                anchor,
                "caller-owned-reply",
                am6_props(OTHER, canonical, None),
                EPOCH + 1,
            )
            .await;
        pairs.push((canonical, root, reply));
    }
    for with_after in [false, true] {
        let mut normalized = Vec::new();
        for (canonical, root, reply) in &pairs {
            let mut args = json!({"id":root.id});
            if with_after {
                args["after"] = json!(root.id);
            }
            let mut response = fixture
                .call(Some(OTHER), "comm.thread", args)
                .await
                .expect("foreign roots retain their existing success shape");
            assert_eq!(response["thread_id"], canonical.to_string());
            assert_eq!(response["count"], 1);
            assert_eq!(am6_message_ids(&response), BTreeSet::from([reply.id]));
            assert!(!response.to_string().contains(&root.id.to_string()));
            am6_normalize_thread_ids(
                &mut response,
                &[
                    (*canonical, "THREAD"),
                    (root.id, "ROOT"),
                    (reply.id, "REPLY"),
                ],
            );
            normalized.push(serde_json::to_vec(&response).unwrap());
        }
        assert_eq!(normalized[0], normalized[1], "Q and ordinary foreign-root responses are byte-identical after ID normalization, after={with_after}");
    }
    let (_, q_root, _) = &pairs[1];
    let visible = fixture
        .call(Some(MAINTAINER), "comm.thread", json!({"id":q_root.id}))
        .await
        .unwrap();
    assert_eq!(am6_message_ids(&visible), BTreeSet::from([q_root.id]));
    let got = fixture
        .call(Some(MAINTAINER), "get", json!({"id":q_root.id}))
        .await
        .unwrap();
    assert_eq!(got["content"], q_root.content);
}
