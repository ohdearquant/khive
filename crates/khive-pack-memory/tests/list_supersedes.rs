//! Opt-in memory list projection through the real KG and memory packs.

#[path = "../../khive-runtime/tests/support/receipt_credentials.rs"]
mod receipt_credentials;

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::presentation::{present, PresentationMode};
use khive_runtime::{
    BackendId, KhiveRuntime, Namespace, NamespaceToken, PackRegistry, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{Edge, EdgeRelation, Note, SqlStatement, StorageError};
use serde_json::{json, Value};
use uuid::Uuid;

const ACTOR: &str = "actor:memory-list-supersedes";
const QUERY: &str = "memory supersession telescope calibration";

fn runtime(backend: &str, visible: &[&str]) -> KhiveRuntime {
    receipt_credentials::with_receipt_credentials(
        KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            backend_id: BackendId::parse(backend).unwrap(),
            actor_id: Some(ACTOR.into()),
            visible_namespaces: visible
                .iter()
                .map(|name| Namespace::parse(name).unwrap())
                .collect(),
            brain_profile: None,
            events_split: None,
            packs: vec!["kg".into(), "memory".into()],
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap(),
    )
}

fn registry(runtime: &KhiveRuntime, visible: &[&str]) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(ACTOR.into()));
    builder.with_visible_namespaces(
        visible
            .iter()
            .map(|name| Namespace::parse(name).unwrap())
            .collect(),
    );
    builder.register(KgPack::new(runtime.clone()));
    builder.register(MemoryPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

fn fixture() -> (KhiveRuntime, VerbRegistry) {
    let runtime = runtime("main", &[]);
    let registry = registry(&runtime, &[]);
    (runtime, registry)
}

fn token(runtime: &KhiveRuntime, namespace: &str) -> NamespaceToken {
    runtime
        .authorize(Namespace::parse(namespace).unwrap())
        .unwrap()
}

fn rows(value: &Value) -> &[Value] {
    let rows = value.get("items").or_else(|| value.get("notes")).unwrap();
    rows.as_array().unwrap()
}

fn ids(values: &[Value]) -> Vec<Uuid> {
    values
        .iter()
        .map(|value| Uuid::parse_str(value["id"].as_str().unwrap()).unwrap())
        .collect()
}

fn id_set(values: &[Value]) -> BTreeSet<Uuid> {
    let result: BTreeSet<_> = ids(values).into_iter().collect();
    assert_eq!(result.len(), values.len(), "duplicate response IDs");
    result
}

async fn snapshot(runtime: &KhiveRuntime) -> Value {
    let mut reader = runtime.sql().reader().await.unwrap();
    let mut result = serde_json::Map::new();
    for table in ["notes", "graph_edges"] {
        let rows = reader
            .query_all(SqlStatement {
                sql: format!("SELECT * FROM {table} ORDER BY namespace, id"),
                params: vec![],
                label: Some("memory-list-supersedes-snapshot".into()),
            })
            .await
            .unwrap();
        result.insert(table.into(), serde_json::to_value(rows).unwrap());
    }
    Value::Object(result)
}

async fn sql(runtime: &KhiveRuntime, statement: &str) {
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement {
            sql: statement.into(),
            params: vec![],
            label: Some("memory-list-supersedes-fixture".into()),
        })
        .await
        .unwrap();
}

fn note(number: usize, namespace: &str, key: Option<String>, pinned: bool) -> Note {
    let mut note = Note::new(namespace, "memory", format!("{QUERY} record {number}"));
    note.id = Uuid::from_u128(number as u128 + 1);
    note.created_at = 1_800_000_000_000_000;
    note.updated_at = note.created_at;
    note.key = key;
    note.salience = Some(0.8);
    note.properties = Some(json!({
        "memory_type": "semantic", "created_by_actor": ACTOR,
        "tags": if pinned { vec!["pinned"] } else { vec!["other"] },
    }));
    note
}

fn edge(
    number: usize,
    namespace: &str,
    source: Uuid,
    target: Uuid,
    relation: EdgeRelation,
) -> Edge {
    let now = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
    Edge {
        id: Uuid::from_u128(1_000_000 + number as u128).into(),
        namespace: namespace.into(),
        source_id: source,
        target_id: target,
        relation,
        weight: 1.0,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: Some(json!({"fixture": "memory-list"})),
        target_backend: None,
    }
}

async fn seed(runtime: &KhiveRuntime, namespace: &str, notes: Vec<Note>, edges: Vec<Edge>) {
    let token = token(runtime, namespace);
    let note_count = notes.len() as u64;
    let result = runtime
        .notes(&token)
        .unwrap()
        .upsert_notes(notes)
        .await
        .unwrap();
    assert_eq!(result.attempted, note_count);
    assert_eq!(result.affected, note_count);
    assert_eq!(result.failed, 0, "{result:?}");
    if !edges.is_empty() {
        let edge_count = edges.len() as u64;
        let result = runtime
            .graph(&token)
            .unwrap()
            .upsert_edges(edges)
            .await
            .unwrap();
        assert_eq!(result.attempted, edge_count);
        assert_eq!(result.affected, edge_count);
        assert_eq!(result.failed, 0, "{result:?}");
    }
}

async fn seeded_walk(runtime: &KhiveRuntime, excluded: usize, retained: usize) -> Vec<Note> {
    let count = excluded + retained;
    // Equal timestamps make both created-at/ID and descending-key order agree
    // with the actual insertion-sequence order used by the cursor walk.
    let notes: Vec<_> = (0..count)
        .map(|index| {
            note(
                index,
                "local",
                Some(format!("walk/{:06}", count - index)),
                true,
            )
        })
        .collect();
    let source = note(500_000, "local", None, false);
    let edges = notes[..excluded]
        .iter()
        .enumerate()
        .map(|(index, target)| {
            edge(
                index,
                "local",
                source.id,
                target.id,
                EdgeRelation::Supersedes,
            )
        })
        .collect();
    let mut stored = notes.clone();
    stored.push(source);
    seed(runtime, "local", stored, edges).await;
    notes
}

fn filtered() -> Value {
    json!({"kind":"memory", "tags":["pinned"], "exclude_superseded":true})
}

async fn remember(registry: &VerbRegistry, suffix: &str) -> Uuid {
    remember_with_tags(registry, suffix, &["pinned"]).await
}

async fn remember_with_tags(registry: &VerbRegistry, suffix: &str, tags: &[&str]) -> Uuid {
    let value = registry
        .dispatch(
            "memory.remember",
            json!({
                "content":format!("{QUERY} {suffix}"), "memory_type":"semantic",
                "salience":0.8, "tags":tags, "namespace":"local",
            }),
        )
        .await
        .unwrap();
    let id = Uuid::parse_str(value["id"].as_str().unwrap()).unwrap();
    let stored = registry.dispatch("get", json!({"id":id})).await.unwrap();
    assert_eq!(stored["namespace"], "local");
    id
}

async fn link(registry: &VerbRegistry, source: Uuid, target: Uuid) -> Uuid {
    let value = registry
        .dispatch(
            "link",
            json!({"source_id":source,"target_id":target,"relation":"supersedes"}),
        )
        .await
        .unwrap();
    Uuid::parse_str(value["id"].as_str().unwrap()).unwrap()
}

async fn recall(registry: &VerbRegistry, suppress: bool) -> BTreeSet<Uuid> {
    let result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query":QUERY, "limit":20, "fusion_strategy":"keyword_only",
                "config":{"scoring":{"enable_supersedes_suppression":suppress,"mmr_penalty":0.0}},
            }),
        )
        .await
        .unwrap();
    id_set(result.as_array().unwrap())
}

async fn assert_list_and_recall(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    visible: &[Uuid],
    unsuppressed: &[Uuid],
) {
    let before = snapshot(runtime).await;
    let listed = registry.dispatch("list", filtered()).await.unwrap();
    assert_eq!(id_set(rows(&listed)), visible.iter().copied().collect());
    assert_eq!(
        recall(registry, true).await,
        visible.iter().copied().collect()
    );
    assert_eq!(
        recall(registry, false).await,
        unsuppressed.iter().copied().collect()
    );
    assert_eq!(snapshot(runtime).await, before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_list_link_is_an_opt_in_read_projection() {
    let (runtime, registry) = fixture();
    let old = remember(&registry, "old calibration").await;
    let new = remember_with_tags(&registry, "new calibration", &["other"]).await;
    let link_id = link(&registry, new, old).await;
    let before = snapshot(&runtime).await;
    let args = json!({"kind":"memory"});
    let default = registry.dispatch("list", args.clone()).await.unwrap();
    assert_eq!(id_set(rows(&default)), BTreeSet::from([old, new]));
    let mut disabled = args;
    disabled["exclude_superseded"] = json!(false);
    assert_eq!(registry.dispatch("list", disabled).await.unwrap(), default);
    let get_before = registry.dispatch("get", json!({"id":old})).await.unwrap();
    let pinned = registry
        .dispatch("list", json!({"kind":"memory","tags":["pinned"],"limit":1}))
        .await
        .unwrap();
    assert_eq!(ids(rows(&pinned)), vec![old]);
    let mut pinned_enabled = filtered();
    pinned_enabled["limit"] = json!(1);
    let current = registry.dispatch("list", pinned_enabled).await.unwrap();
    assert!(rows(&current).is_empty());
    assert_eq!(current["has_more"], false);
    let explicit = registry
        .dispatch(
            "list",
            json!({
                "kind":"note","note_kind":"memory","exclude_superseded":true,
            }),
        )
        .await
        .unwrap();
    assert_eq!(ids(rows(&explicit)), vec![new]);
    assert_eq!(
        registry.dispatch("get", json!({"id":old})).await.unwrap(),
        get_before
    );
    let edges = registry
        .dispatch("list", json!({"kind":"edge","relations":["supersedes"]}))
        .await
        .unwrap();
    assert!(id_set(rows(&edges)).contains(&link_id));
    assert_eq!(
        registry
            .dispatch("list", json!({"kind":"memory"}))
            .await
            .unwrap(),
        default
    );
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_list_and_recall_keep_deleted_superseder_suppression() {
    let (runtime, registry) = fixture();
    let old = remember(&registry, "older value").await;
    let new = remember(&registry, "newer value").await;
    let link_id = link(&registry, new, old).await;
    let edge_before = runtime
        .get_edge_including_deleted(&token(&runtime, "local"), link_id)
        .await
        .unwrap()
        .unwrap();
    assert!(edge_before.deleted_at.is_none());
    assert_list_and_recall(&runtime, &registry, &[new], &[old, new]).await;
    registry
        .dispatch("delete", json!({"id":new}))
        .await
        .unwrap();
    assert!(runtime
        .get_note_including_deleted(&token(&runtime, "local"), new)
        .await
        .unwrap()
        .unwrap()
        .deleted_at
        .is_some());
    let edge_after_delete = runtime
        .get_edge_including_deleted(&token(&runtime, "local"), link_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&edge_after_delete).unwrap(),
        serde_json::to_value(&edge_before).unwrap()
    );
    assert_list_and_recall(&runtime, &registry, &[], &[old]).await;
    let restored = registry
        .dispatch("restore", json!({"id":new,"kind":"memory"}))
        .await
        .unwrap();
    assert_eq!(restored["restored"], true);
    let edge_after_restore = runtime
        .get_edge_including_deleted(&token(&runtime, "local"), link_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&edge_after_restore).unwrap(),
        serde_json::to_value(&edge_before).unwrap()
    );
    assert_list_and_recall(&runtime, &registry, &[new], &[old, new]).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_list_and_recall_return_after_supersedes_edge_removed() {
    let (runtime, registry) = fixture();
    let old = remember(&registry, "older value").await;
    let new = remember(&registry, "newer value").await;
    let link_id = link(&registry, new, old).await;
    assert_list_and_recall(&runtime, &registry, &[new], &[old, new]).await;
    registry
        .dispatch("delete", json!({"id":new}))
        .await
        .unwrap();
    let edge_before_removal = runtime
        .get_edge_including_deleted(&token(&runtime, "local"), link_id)
        .await
        .unwrap()
        .unwrap();
    assert!(edge_before_removal.deleted_at.is_none());
    assert_list_and_recall(&runtime, &registry, &[], &[old]).await;
    registry
        .dispatch("delete", json!({"id":link_id,"kind":"edge"}))
        .await
        .unwrap();
    let removed = runtime
        .get_edge_including_deleted(&token(&runtime, "local"), link_id)
        .await
        .unwrap()
        .unwrap();
    assert!(removed.deleted_at.is_some());
    assert_eq!(removed.source_id, new);
    assert_eq!(removed.target_id, old);
    assert_eq!(removed.relation, EdgeRelation::Supersedes);
    assert!(runtime
        .get_note_including_deleted(&token(&runtime, "local"), new)
        .await
        .unwrap()
        .unwrap()
        .deleted_at
        .is_some());
    assert_list_and_recall(&runtime, &registry, &[old], &[old]).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_list_keeps_legacy_property_target_that_recall_suppresses() {
    let (runtime, registry) = fixture();
    let old = remember(&registry, "legacy older value").await;
    let new = remember(&registry, "legacy newer value").await;
    let token = token(&runtime, "local");
    let mut successor = runtime
        .get_note_including_deleted(&token, new)
        .await
        .unwrap()
        .unwrap();
    // This reserved historical field is fixture data, not an accepted remember input.
    successor.properties.as_mut().unwrap()["supersedes"] = json!(old.to_string());
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(successor)
        .await
        .unwrap();
    let stored = runtime
        .get_note_including_deleted(&token, new)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.properties.as_ref().unwrap()["supersedes"],
        old.to_string()
    );
    let before = snapshot(&runtime).await;
    assert!(before["graph_edges"].as_array().unwrap().is_empty());
    let edges = registry
        .dispatch("list", json!({"kind":"edge"}))
        .await
        .unwrap();
    assert!(rows(&edges).is_empty());
    let listed = registry.dispatch("list", filtered()).await.unwrap();
    assert_eq!(id_set(rows(&listed)), BTreeSet::from([old, new]));
    assert_eq!(recall(&registry, true).await, BTreeSet::from([new]));
    assert_eq!(recall(&registry, false).await, BTreeSet::from([old, new]));
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_list_filters_before_offset_and_all_cursor_boundaries() {
    let (runtime, registry) = fixture();
    let notes = seeded_walk(&runtime, 205, 3).await;
    let expected: Vec<_> = notes[205..].iter().map(|note| note.id).collect();
    let before = snapshot(&runtime).await;
    for keyed in [false, true] {
        let mut args = filtered();
        args["limit"] = json!(1);
        args["offset"] = json!(1);
        if keyed {
            args["key_prefix"] = json!("walk/");
        }
        let middle = registry.dispatch("list", args.clone()).await.unwrap();
        assert_eq!(ids(rows(&middle)), expected[1..2]);
        assert_eq!(middle["has_more"], true);
        args["offset"] = json!(2);
        let last = registry.dispatch("list", args).await.unwrap();
        assert_eq!(ids(rows(&last)), expected[2..]);
        assert_eq!(last["has_more"], false);

        let mut args = filtered();
        args["limit"] = json!(1);
        args["after"] = json!("");
        if keyed {
            args["key_prefix"] = json!("walk/");
        }
        let first = registry.dispatch("list", args.clone()).await.unwrap();
        assert_eq!(ids(rows(&first)), expected[..1]);
        assert_eq!(first["has_more"], true);
        assert!(first["next_after"].as_str().is_some());
        args["after"] = first["next_after"].clone();
        args["limit"] = json!(2);
        let terminal = registry.dispatch("list", args).await.unwrap();
        assert_eq!(ids(rows(&terminal)), expected[1..]);
        assert_eq!(terminal["has_more"], false);
        assert!(terminal["next_after"].is_null());

        let mut zero = filtered();
        zero["limit"] = json!(0);
        zero["offset"] = json!(0);
        if keyed {
            zero["key_prefix"] = json!("walk/");
        }
        let empty = registry.dispatch("list", zero.clone()).await.unwrap();
        assert!(rows(&empty).is_empty());
        assert_eq!(empty["has_more"], true);
        assert_eq!(empty["requested_limit"], 0);
        assert_eq!(empty["effective_limit"], 0);
        zero.as_object_mut().unwrap().remove("offset");
        zero["after"] = json!("");
        let error = registry.dispatch("list", zero).await.unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");

        let mut clamped = filtered();
        clamped["offset"] = json!(0);
        clamped["limit"] = json!(201);
        if keyed {
            clamped["key_prefix"] = json!("walk/");
        }
        let enabled = registry.dispatch("list", clamped.clone()).await.unwrap();
        assert_eq!(ids(rows(&enabled)), expected);
        assert_eq!(enabled["requested_limit"], 201);
        assert_eq!(enabled["effective_limit"], 200);
        assert_eq!(enabled["limit_clamped"], true);
        assert_eq!(enabled["has_more"], false);
        clamped["exclude_superseded"] = json!(false);
        let disabled = registry.dispatch("list", clamped.clone()).await.unwrap();
        clamped
            .as_object_mut()
            .unwrap()
            .remove("exclude_superseded");
        assert_eq!(registry.dispatch("list", clamped).await.unwrap(), disabled);
        assert_eq!(
            ids(rows(&disabled)),
            notes[..200].iter().map(|note| note.id).collect::<Vec<_>>()
        );
        assert_eq!(disabled["effective_limit"], 200);
        assert_eq!(disabled["limit_clamped"], true);
        assert_eq!(disabled["has_more"], true);
    }
    // A boundary on an excluded row still advances the real keyed store walk.
    let after_key = registry
        .dispatch(
            "list",
            json!({
                "kind":"memory", "tags":["pinned"], "exclude_superseded":true,
                "key_prefix":"walk/", "after_key":notes[204].key, "limit":3,
            }),
        )
        .await
        .unwrap();
    assert_eq!(ids(rows(&after_key)), expected);
    assert_eq!(after_key["has_more"], false);
    assert!(after_key["next_after"].is_null());
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_list_scan_cap_discloses_and_resumes_past_excluded_rows() {
    let (runtime, registry) = fixture();
    let notes = seeded_walk(&runtime, 10_001, 2).await;
    let expected: Vec<_> = notes[10_001..].iter().map(|note| note.id).collect();
    let before = snapshot(&runtime).await;
    for keyed in [false, true] {
        let mut args = filtered();
        args["limit"] = json!(2);
        args["offset"] = json!(0);
        if keyed {
            args["key_prefix"] = json!("walk/");
        }
        let offset = registry.dispatch("list", args.clone()).await.unwrap();
        assert!(rows(&offset).is_empty());
        assert_eq!(offset["scan_incomplete"], true);
        assert_eq!(offset["has_more"], true);
        args.as_object_mut().unwrap().remove("offset");
        args["after"] = json!("");
        let first = registry.dispatch("list", args.clone()).await.unwrap();
        assert!(rows(&first).is_empty());
        assert_eq!(first["scan_incomplete"], true);
        assert_eq!(first["has_more"], true);
        let cursor = first["next_after"].as_str().unwrap();
        assert!(!cursor.is_empty());
        if !keyed {
            assert_eq!(Uuid::parse_str(cursor).unwrap(), notes[9_999].id);
        }
        args["after"] = json!(cursor);
        let terminal = registry.dispatch("list", args).await.unwrap();
        assert_eq!(ids(rows(&terminal)), expected);
        assert_eq!(terminal["has_more"], false);
        assert!(terminal["next_after"].is_null());
        assert!(terminal.get("scan_incomplete").is_none());
        for mode in [PresentationMode::Agent, PresentationMode::Verbose] {
            let presented = present(first.clone(), mode, 1_800_000_000);
            assert_eq!(presented["next_after"], first["next_after"]);
            assert_eq!(presented["scan_incomplete"], true);
            assert_eq!(presented["has_more"], true);
            assert_eq!(presented["effective_limit"], 2);
        }
    }
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_list_uses_incoming_live_supersedes_with_chains_and_fan_in() {
    let (runtime, registry) = fixture();
    let mut targets: Vec<_> = (0..7)
        .map(|index| note(index, "local", None, true))
        .collect();
    let legacy_target = targets[4].id;
    targets[6].properties.as_mut().unwrap()["supersedes"] = json!(legacy_target);
    let sources: Vec<_> = (100..164)
        .map(|index| note(index, "local", None, false))
        .collect();
    let mut edges = vec![
        edge(
            0,
            "local",
            sources[0].id,
            targets[0].id,
            EdgeRelation::Supersedes,
        ),
        edge(
            1,
            "local",
            targets[0].id,
            targets[1].id,
            EdgeRelation::Supersedes,
        ),
        edge(
            2,
            "local",
            targets[3].id,
            sources[0].id,
            EdgeRelation::Supersedes,
        ),
        edge(
            3,
            "local",
            sources[0].id,
            targets[4].id,
            EdgeRelation::Supports,
        ),
        edge(
            4,
            "local",
            sources[0].id,
            targets[5].id,
            EdgeRelation::Supersedes,
        ),
    ];
    edges[4].deleted_at = Some(edges[4].updated_at);
    edges.extend(sources.iter().enumerate().map(|(index, source)| {
        edge(
            100 + index,
            "local",
            source.id,
            targets[2].id,
            EdgeRelation::Supersedes,
        )
    }));
    let mut stored = targets.clone();
    stored.extend(sources);
    seed(&runtime, "local", stored, edges).await;
    let before = snapshot(&runtime).await;
    let result = registry.dispatch("list", filtered()).await.unwrap();
    assert_eq!(
        ids(rows(&result)),
        targets[3..].iter().map(|note| note.id).collect::<Vec<_>>()
    );
    let all = registry
        .dispatch("list", json!({"kind":"memory","tags":["pinned"]}))
        .await
        .unwrap();
    assert_eq!(
        ids(rows(&all)),
        targets.iter().map(|note| note.id).collect::<Vec<_>>()
    );
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_list_uses_original_visible_edge_namespaces() {
    let runtime = runtime("main", &["shared"]);
    let registry = registry(&runtime, &["shared"]);
    let a = note(0, "local", Some("scope/a".into()), true);
    let b = note(1, "local", Some("scope/b".into()), true);
    let shared = note(2, "shared", None, true);
    let visible_source = note(100, "shared", None, false);
    let hidden_source = note(101, "hidden", None, false);
    seed(&runtime, "local", vec![a.clone(), b.clone()], vec![]).await;
    seed(
        &runtime,
        "shared",
        vec![shared.clone(), visible_source.clone()],
        vec![edge(
            0,
            "shared",
            visible_source.id,
            a.id,
            EdgeRelation::Supersedes,
        )],
    )
    .await;
    seed(
        &runtime,
        "hidden",
        vec![hidden_source.clone()],
        vec![edge(
            1,
            "hidden",
            hidden_source.id,
            b.id,
            EdgeRelation::Supersedes,
        )],
    )
    .await;
    let before = snapshot(&runtime).await;
    let assert_no_hidden_metadata = |value: &Value| {
        let text = serde_json::to_string(value).unwrap();
        for hidden in [
            "hidden".to_owned(),
            hidden_source.id.to_string(),
            Uuid::from_u128(1_000_001).to_string(),
        ] {
            assert!(!text.contains(&hidden), "hidden metadata disclosed: {text}");
        }
    };
    let normal = registry.dispatch("list", filtered()).await.unwrap();
    assert_eq!(id_set(rows(&normal)), BTreeSet::from([b.id, shared.id]));
    assert_no_hidden_metadata(&normal);
    for mode in [
        json!({"offset":0}),
        json!({"after":""}),
        json!({"key_prefix":"scope/"}),
    ] {
        let mut args = filtered();
        args.as_object_mut()
            .unwrap()
            .extend(mode.as_object().unwrap().clone());
        if args.get("key_prefix").is_none() {
            args["key_prefix"] = json!("scope/");
        }
        let result = registry.dispatch("list", args).await.unwrap();
        assert_eq!(ids(rows(&result)), vec![b.id]);
        assert_no_hidden_metadata(&result);
    }
    let mut explicit = filtered();
    explicit["namespace"] = json!("local");
    let result = registry.dispatch("list", explicit).await.unwrap();
    assert_eq!(id_set(rows(&result)), BTreeSet::from([a.id, b.id]));
    assert_no_hidden_metadata(&result);
    let mut explicit_shared = filtered();
    explicit_shared["namespace"] = json!("shared");
    let shared_result = registry.dispatch("list", explicit_shared).await.unwrap();
    assert_eq!(ids(rows(&shared_result)), vec![shared.id]);
    assert_no_hidden_metadata(&shared_result);
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_list_reads_graph_on_the_selected_memory_backend() {
    let kg = runtime("main", &[]);
    let memory = runtime("memory", &[]);
    assert!(!kg.shares_backend_storage_with(&memory));
    let a = note(0, "local", None, true);
    let b = note(1, "local", None, true);
    let source = note(100, "local", None, false);
    seed(
        &memory,
        "local",
        vec![a.clone(), b.clone(), source.clone()],
        vec![edge(0, "local", source.id, a.id, EdgeRelation::Supersedes)],
    )
    .await;
    // The same target IDs exist on both stores, but their incoming edges disagree.
    let mut sentinel = a.clone();
    sentinel.content = "KG sentinel must not replace the memory record".into();
    seed(
        &kg,
        "local",
        vec![sentinel, b.clone(), source.clone()],
        vec![edge(1, "local", source.id, b.id, EdgeRelation::Supersedes)],
    )
    .await;
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(ACTOR.into()));
    PackRegistry::register_packs_with_runtimes(
        &["kg".into(), "memory".into()],
        &HashMap::from([("memory".into(), memory.clone())]),
        &kg,
        &mut builder,
    )
    .unwrap();
    let registry = builder.build().unwrap();
    kg.install_edge_rules(registry.all_edge_rules());
    memory.install_edge_rules(registry.all_edge_rules());
    let before_kg = snapshot(&kg).await;
    let before_memory = snapshot(&memory).await;
    let result = registry.dispatch("list", filtered()).await.unwrap();
    assert_eq!(ids(rows(&result)), vec![b.id]);
    assert_eq!(rows(&result)[0]["content"], b.content);
    let all = registry
        .dispatch("list", json!({"kind":"memory","tags":["pinned"]}))
        .await
        .unwrap();
    assert_eq!(ids(rows(&all)), vec![a.id, b.id]);
    assert_eq!(rows(&all)[0]["content"], a.content);
    assert_eq!(snapshot(&kg).await, before_kg);
    assert_eq!(snapshot(&memory).await, before_memory);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_list_rejects_wrong_scope_and_non_boolean_flags_without_writes() {
    let (runtime, registry) = fixture();
    let notes = seeded_walk(&runtime, 1, 2).await;
    let before = snapshot(&runtime).await;
    for kind in [
        "entity",
        "concept",
        "edge",
        "event",
        "proposal",
        "observation",
        "note",
    ] {
        for flag in [json!(true), json!(false), Value::Null] {
            let boolean_flag = flag.is_boolean();
            let error = registry
                .dispatch(
                    "list",
                    json!({
                        "kind":kind, "exclude_superseded":flag,
                    }),
                )
                .await
                .unwrap_err();
            assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
            if boolean_flag {
                assert!(error.to_string().contains("exclude_superseded"), "{error}");
            }
        }
    }
    let error = registry
        .dispatch(
            "list",
            json!({
                "kind":"note", "note_kind":"observation", "exclude_superseded":false,
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
    for flag in [
        Value::Null,
        json!(0),
        json!(1),
        json!(1.0),
        json!("true"),
        json!([]),
        json!({}),
    ] {
        for scope in [
            json!({"kind":"memory"}),
            json!({"kind":"note","note_kind":"memory"}),
        ] {
            let mut args = scope;
            args["exclude_superseded"] = flag.clone();
            let error = registry.dispatch("list", args).await.unwrap_err();
            assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
        }
    }
    let valid = registry.dispatch("list", filtered()).await.unwrap();
    assert_eq!(
        ids(rows(&valid)),
        notes[1..].iter().map(|note| note.id).collect::<Vec<_>>()
    );
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_list_graph_read_errors_fail_closed_without_changing_rows() {
    let (runtime, registry) = fixture();
    let notes = seeded_walk(&runtime, 1, 2).await;
    let before = snapshot(&runtime).await;
    sql(
        &runtime,
        "ALTER TABLE graph_edges RENAME TO unavailable_graph_edges",
    )
    .await;
    for mode in [
        json!({}),
        json!({"after":""}),
        json!({"key_prefix":"walk/"}),
    ] {
        let mut args = filtered();
        args.as_object_mut()
            .unwrap()
            .extend(mode.as_object().unwrap().clone());
        let result = registry.dispatch("list", args.clone()).await;
        assert!(
            result.is_err(),
            "an unreadable graph must not admit superseded rows: {result:?}"
        );
        args["exclude_superseded"] = json!(false);
        let unfiltered = registry.dispatch("list", args).await.unwrap();
        assert_eq!(
            id_set(rows(&unfiltered)),
            notes.iter().map(|note| note.id).collect()
        );
    }
    sql(
        &runtime,
        "ALTER TABLE unavailable_graph_edges RENAME TO graph_edges",
    )
    .await;
    let restored = registry.dispatch("list", filtered()).await.unwrap();
    assert_eq!(
        ids(rows(&restored)),
        notes[1..].iter().map(|note| note.id).collect::<Vec<_>>()
    );
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_list_observes_cancellation_during_real_sql_work() {
    use std::time::Duration;

    let (runtime, registry) = fixture();
    let notes = seeded_walk(&runtime, 205, 3).await;
    let before = snapshot(&runtime).await;
    let pool = runtime.backend().pool();
    assert_eq!(pool.max_readers(), 0, "fixture uses the shared connection");

    // Preserve the graph rows while making the real adjacency query stay in
    // SQLite long enough to observe and cancel, even on a fast machine.
    sql(
        &runtime,
        "ALTER TABLE graph_edges RENAME TO cancellation_graph_edges",
    )
    .await;
    sql(
        &runtime,
        "CREATE VIEW graph_edges AS SELECT * FROM cancellation_graph_edges \
         WHERE (WITH RECURSIVE cancellation_work(n) AS (\
             VALUES(1) UNION ALL SELECT n + 1 FROM cancellation_work WHERE n < 100000000\
         ) SELECT SUM(n) FROM cancellation_work) > 0",
    )
    .await;

    let statements = pool.observe_test_statement_starts(4096).unwrap();
    assert!(statements.started_statements().unwrap().is_empty());
    let progress = Arc::new(AtomicUsize::new(0));
    let (cancel, receiver) = tokio::sync::watch::channel(false);
    let mut request = tokio_util::task::AbortOnDropHandle::new(tokio::spawn({
        let registry = registry.clone();
        let progress = Arc::clone(&progress);
        async move {
            khive_db::scope_test_read_progress(
                progress,
                khive_storage::scope_request_read_cancellation(receiver, async move {
                    let no_deadline = khive_storage::capture_request_read_context()
                        .deadline()
                        .is_none();
                    (no_deadline, registry.dispatch("list", filtered()).await)
                }),
            )
            .await
        }
    }));

    let ready = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if request.is_finished() {
                return Err("list finished before its graph read was observed".to_owned());
            }
            let started = statements
                .started_statements()
                .map_err(|error| error.to_string())?;
            if started.iter().any(|statement| {
                statement.readonly
                    && statement
                        .sql
                        .contains("FROM requested CROSS JOIN graph_edges AS edges")
            }) {
                let before_graph_progress = progress.load(Ordering::SeqCst);
                while progress.load(Ordering::SeqCst) == before_graph_progress {
                    if request.is_finished() {
                        return Err("graph read finished before further SQLite progress".to_owned());
                    }
                    tokio::task::yield_now().await;
                }
                return Ok(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let unfinished_at_cancel = !request.is_finished();
    let already_cancelled = cancel.send_replace(true);

    // Watcher failure and task panic also take the settlement path. Never
    // restore the table while a query could still be using the view.
    let (settled, within_bound) =
        match tokio::time::timeout(Duration::from_secs(5), &mut request).await {
            Ok(result) => (Some(result), true),
            Err(_) => {
                request.abort();
                (
                    tokio::time::timeout(Duration::from_secs(5), &mut request)
                        .await
                        .ok(),
                    false,
                )
            }
        };
    let released = tokio::time::timeout(Duration::from_secs(5), async {
        while pool.oldest_checkout_age().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    drop(statements);
    assert!(
        settled.is_some() && released.is_ok(),
        "request did not settle and release its reader: settled={settled:?}, released={released:?}"
    );
    sql(&runtime, "DROP VIEW graph_edges").await;
    sql(
        &runtime,
        "ALTER TABLE cancellation_graph_edges RENAME TO graph_edges",
    )
    .await;

    assert!(matches!(ready, Ok(Ok(()))), "{ready:?}");
    assert!(
        unfinished_at_cancel,
        "graph read finished before cancellation"
    );
    assert!(!already_cancelled, "request was already cancelled");
    assert!(within_bound, "request needed the abort fail-safe");
    let (no_deadline, result) = settled.unwrap().expect("list task panicked");
    assert!(
        no_deadline,
        "a deadline must not substitute for cancellation"
    );
    assert!(
        matches!(
            &result,
            Err(RuntimeError::Storage(StorageError::Timeout { operation }))
                if operation == "batch_neighbors"
        ),
        "{result:?}"
    );
    let stopped_progress = progress.load(Ordering::SeqCst);
    let retry = tokio::time::timeout(
        Duration::from_secs(5),
        registry.dispatch("list", filtered()),
    )
    .await
    .expect("reader did not recover after cancellation")
    .unwrap();
    assert_eq!(
        ids(rows(&retry)),
        notes[205..].iter().map(|note| note.id).collect::<Vec<_>>()
    );
    assert_eq!(progress.load(Ordering::SeqCst), stopped_progress);
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_list_catalog_and_presentation_preserve_continuation() {
    let (runtime, registry) = fixture();
    let notes = seeded_walk(&runtime, 1, 3).await;
    let before = snapshot(&runtime).await;
    let catalog = registry
        .dispatch("verbs", json!({"pack":"kg"}))
        .await
        .unwrap();
    let definition = catalog["verbs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["verb"] == "list")
        .unwrap();
    assert!(definition["signature"]
        .as_str()
        .unwrap()
        .contains("exclude_superseded"));
    let mut args = filtered();
    args["after"] = json!("");
    args["limit"] = json!(2);
    let page = registry.dispatch("list", args).await.unwrap();
    assert_eq!(ids(rows(&page)), vec![notes[1].id, notes[2].id]);
    assert_eq!(page["next_after"], notes[2].id.to_string());
    assert_eq!(page["has_more"], true);
    for mode in [PresentationMode::Agent, PresentationMode::Verbose] {
        let output = present(page.clone(), mode, 1_800_000_000);
        assert_eq!(rows(&output).len(), 2);
        for key in [
            "next_after",
            "has_more",
            "requested_limit",
            "effective_limit",
            "limit_clamped",
        ] {
            assert_eq!(output[key], page[key], "{mode:?}/{key}");
        }
    }
    assert_eq!(snapshot(&runtime).await, before);
}
