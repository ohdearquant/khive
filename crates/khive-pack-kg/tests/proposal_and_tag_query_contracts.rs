//! Proposal identifier spellings and SQL-filtered note pagination.

use std::collections::HashSet;

use async_trait::async_trait;

use khive_pack_kg::{projection_worker::ProposalsProjectionWorker, KgPack};
use khive_runtime::{
    HandlerDef, KhiveRuntime, Namespace, NamespaceToken, PackRuntime, RuntimeError, VerbRegistry,
    VerbRegistryBuilder, VerifiedActor,
};
use khive_storage::{event::Event, Note, SubstrateKind};
use khive_types::{EventKind, Id128, Pack, ProposalChangeset, ProposalCreatedPayload};
use serde_json::{json, Value};
use uuid::Uuid;

const NOTE_ID_BASE: u128 = 0xbbbb_0000_0000_4000_8000_0000_0000_0000;
const NOTE_TIME: i64 = 1_700_000_000_000_000;

fn registry(runtime: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder
        .with_runtime_event_store(runtime)
        .expect("configure runtime event store");
    builder.register(KgPack::new(runtime.clone()));
    builder.build().expect("registry builds")
}

struct MessageKindFixture;

impl Pack for MessageKindFixture {
    const NAME: &'static str = "message_kind_fixture";
    const NOTE_KINDS: &'static [&'static str] = &["message"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
    const REQUIRES: &'static [&'static str] = &["kg"];
}

#[async_trait]
impl PackRuntime for MessageKindFixture {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }

    fn requires(&self) -> &'static [&'static str] {
        Self::REQUIRES
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "MessageKindFixture does not handle {verb:?}"
        )))
    }
}

fn mailbox_registry(runtime: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder
        .with_runtime_event_store(runtime)
        .expect("configure runtime event store");
    builder.register(KgPack::new(runtime.clone()));
    builder.register(MessageKindFixture);
    builder.build().expect("mailbox fixture registry builds")
}

fn changeset() -> Value {
    json!({"kind": "add_entity", "entity": {"kind": "concept", "name": "Query contract"}})
}

async fn seed_proposal(runtime: &KhiveRuntime, namespace: &str, id: Uuid) {
    let token = runtime
        .authorize(Namespace::parse(namespace).expect("fixture namespace"))
        .expect("authorize namespace");
    let payload = ProposalCreatedPayload {
        proposal_id: Id128::from_u128(id.as_u128()),
        proposer: "auditor".to_string(),
        title: "Identifier spellings".to_string(),
        description: "All complete spellings identify one proposal".to_string(),
        changeset: serde_json::from_value::<ProposalChangeset>(changeset())
            .expect("valid changeset"),
        reviewers: vec![],
        expiry: None,
        parent_id: None,
    };
    let mut event = Event::new(
        namespace,
        "propose",
        EventKind::ProposalCreated,
        SubstrateKind::Entity,
        "auditor",
    );
    event.payload = serde_json::to_value(payload).expect("serialize proposal payload");
    event.aggregate_kind = Some("proposal".to_string());
    event.aggregate_id = Some(id);
    runtime
        .events(&token)
        .expect("event store")
        .append_event(event)
        .await
        .expect("append proposal event");
    ProposalsProjectionWorker::new(runtime.clone())
        .on_proposal_created(&token, id, "auditor", "Identifier spellings", None)
        .await
        .expect("project proposal event");
}

async fn get_proposal(registry: &VerbRegistry, id: &str) -> Result<Value, RuntimeError> {
    registry
        .dispatch("get", json!({"id": id, "namespace": "local"}))
        .await
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn proposal_get_normalizes_complete_uuid_spellings() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let registry = registry(&runtime);
    let id = Uuid::parse_str("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee").expect("fixed UUID");
    seed_proposal(&runtime, "local", id).await;
    let canonical = id.to_string();
    let expected = get_proposal(&registry, &canonical)
        .await
        .expect("canonical proposal lookup");
    assert_eq!(expected["id"], canonical);
    assert!(expected.get("proposal_id").is_none());
    let upper = canonical.to_ascii_uppercase();
    assert_ne!(upper, canonical, "uppercase witness must change letters");
    for spelling in [
        upper,
        id.simple().to_string(),
        id.simple().to_string().to_ascii_uppercase(),
        format!("{{{canonical}}}"),
        format!("urn:uuid:{canonical}"),
    ] {
        assert_eq!(
            get_proposal(&registry, &spelling)
                .await
                .unwrap_or_else(|error| panic!("complete spelling {spelling}: {error}")),
            expected,
            "same proposal payload for {spelling}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn proposal_get_retains_public_creation_prefix_and_namespace_contracts() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let registry = registry(&runtime);
    let created = registry
        .dispatch(
            "propose",
            json!({"title": "Public creation", "description": "Public proposal round trip", "changeset": changeset(), "namespace": "local"}),
        )
        .await
        .expect("public propose");
    let public_id = created["id"].as_str().expect("proposal id");
    let expected = get_proposal(&registry, public_id)
        .await
        .expect("public canonical get");
    assert_eq!(
        get_proposal(&registry, &public_id.replace('-', ""))
            .await
            .expect("public compact get"),
        expected
    );

    let fixed = Uuid::parse_str("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee").expect("fixed UUID");
    seed_proposal(&runtime, "local", fixed).await;
    let fixed_compact = fixed.simple().to_string();
    let prefix = &fixed_compact[..16];
    assert_eq!(
        get_proposal(&registry, prefix)
            .await
            .expect("unique prefix")["id"],
        fixed.to_string()
    );
    let second = Uuid::parse_str("aaaaaaaa-bbbb-4ccc-8eee-dddddddddddd").expect("second UUID");
    seed_proposal(&runtime, "local", second).await;
    assert!(matches!(
        get_proposal(&registry, prefix).await,
        Err(RuntimeError::NotFound(_))
    ));

    let foreign = Uuid::parse_str("cccccccc-dddd-4eee-8fff-aaaaaaaaaaaa").expect("foreign UUID");
    seed_proposal(&runtime, "audit-foreign", foreign).await;
    for spelling in [foreign.to_string(), foreign.simple().to_string()] {
        assert!(matches!(
            get_proposal(&registry, &spelling).await,
            Err(RuntimeError::NotFound(_))
        ));
    }
    assert!(
        get_proposal(&registry, "ffffffff-ffff-4fff-8fff-ffffffffffff")
            .await
            .is_err()
    );
    assert!(get_proposal(&registry, "not-a-uuid").await.is_err());
}

fn note(index: usize, kind: &str, properties: Value) -> Note {
    let mut note = Note::new("local", kind, format!("row {index}")).with_properties(properties);
    note.id = Uuid::from_u128(NOTE_ID_BASE + index as u128);
    note.created_at = NOTE_TIME;
    note.updated_at = NOTE_TIME;
    note
}

async fn seed_notes(runtime: &KhiveRuntime, notes: Vec<Note>) {
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local");
    let store = runtime.notes(&token).expect("note store");
    let mut batch = Vec::with_capacity(1000);
    for note in notes {
        batch.push(note);
        if batch.len() == 1000 {
            store
                .upsert_notes(std::mem::take(&mut batch))
                .await
                .expect("upsert batch");
        }
    }
    if !batch.is_empty() {
        store.upsert_notes(batch).await.expect("upsert tail");
    }
}

async fn list(registry: &VerbRegistry, mut args: Value, actor: Option<&str>) -> Value {
    args["kind"] = json!("note");
    args["namespace"] = json!("local");
    match actor {
        Some(actor) => {
            registry
                .dispatch_as(
                    "list",
                    args,
                    VerifiedActor::new(actor).expect("fixture actor"),
                )
                .await
        }
        None => registry.dispatch("list", args).await,
    }
    .expect("public note list")
}

fn ids(page: &Value, field: &str) -> Vec<String> {
    page[field]
        .as_array()
        .expect("page rows")
        .iter()
        .map(|row| row["id"].as_str().expect("note id").to_string())
        .collect()
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn tag_only_note_list_reaches_past_scan_ceiling() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let registry = registry(&runtime);
    let mut notes: Vec<_> = (0..10_002)
        .map(|index| note(index, "observation", json!({"tags": ["audit"]})))
        .collect();
    let mut decoy = note(20_000, "observation", json!({"tags": ["other"]}));
    decoy.created_at += 1;
    decoy.updated_at += 1;
    notes.push(decoy);
    let mut deleted = note(20_001, "observation", json!({"tags": ["audit"]}));
    deleted.created_at += 2;
    deleted.deleted_at = Some(NOTE_TIME + 3);
    notes.push(deleted);
    seed_notes(&runtime, notes).await;
    for (offset, expected, more) in [
        (0, Some(0), true),
        (10_000, Some(10_000), true),
        (10_001, Some(10_001), false),
        (10_002, None, false),
    ] {
        let page = list(
            &registry,
            json!({"note_kind": "observation", "tags": ["AUDIT"], "offset": offset, "limit": 1}),
            None,
        )
        .await;
        let wanted: Vec<_> = expected
            .map(|index| vec![Uuid::from_u128(NOTE_ID_BASE + index).to_string()])
            .unwrap_or_default();
        assert_eq!(ids(&page, "items"), wanted, "offset {offset}");
        assert_eq!(page["has_more"], more, "offset {offset}");
        assert!(
            page.get("scan_incomplete").is_none(),
            "offset {offset}: {page}"
        );
    }
    let empty = list(&registry, json!({"tags": ["absent"], "limit": 1}), None).await;
    assert!(ids(&empty, "items").is_empty());
    assert_eq!(empty["has_more"], false);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn tag_only_lists_keep_typed_ascii_matching_and_cursor_continuation() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let registry = registry(&runtime);
    let shapes = [
        json!({"tags": ["Alpha", "beta"]}),
        json!({"tags": ["alpha"]}),
        json!({"tags": ["beta"]}),
        json!({"tags": [7, "ALPHA"]}),
        json!({"tags": "Alpha"}),
        json!({"tags": {"Alpha": true}}),
        json!({"tags": ["Ä"]}),
        json!({"tags": null}),
    ];
    seed_notes(
        &runtime,
        shapes
            .into_iter()
            .enumerate()
            .map(|(index, properties)| note(index, "observation", properties))
            .collect(),
    )
    .await;
    let expected = |indices: &[usize]| {
        indices
            .iter()
            .map(|index| Uuid::from_u128(NOTE_ID_BASE + *index as u128).to_string())
            .collect::<Vec<_>>()
    };
    for (tags, mode, indices) in [
        (json!(["ALPHA", "BETA"]), "any", vec![0, 1, 2, 3]),
        (json!(["ALPHA", "BETA"]), "all", vec![0]),
        (json!(["ä"]), "any", vec![]),
        (json!(["Ä"]), "all", vec![6]),
    ] {
        let page = list(
            &registry,
            json!({"tags": tags, "tag_mode": mode, "limit": 20}),
            None,
        )
        .await;
        assert_eq!(ids(&page, "items"), expected(&indices));
        assert_eq!(page["has_more"], false);
    }

    let mut after = String::new();
    let mut seen = Vec::new();
    for iteration in 0..8 {
        let page = list(
            &registry,
            json!({"tags": ["alpha"], "after": after, "limit": 1}),
            None,
        )
        .await;
        seen.extend(ids(&page, "notes"));
        if iteration == 0 {
            let mut inserted = note(99, "observation", json!({"tags": ["alpha"]}));
            inserted.created_at += 10;
            seed_notes(&runtime, vec![inserted]).await;
        }
        if let Some(cursor) = page["next_after"].as_str() {
            after = cursor.to_string();
        } else {
            let seen_set: HashSet<_> = seen.iter().cloned().collect();
            assert_eq!(seen.len(), seen_set.len(), "cursor repeats a note");
            assert_eq!(seen_set, expected(&[0, 1, 3, 99]).into_iter().collect());
            assert_eq!(page["has_more"], false);
            return;
        }
    }
    panic!("tag cursor did not finish");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn tagged_message_lists_retain_named_and_legacy_mailbox_rules() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let registry = mailbox_registry(&runtime);
    let shapes = [
        json!({"direction": "inbound", "to_actor": "auditor"}),
        json!({"direction": "outbound", "from_actor": "auditor"}),
        json!({"direction": "inbound", "to_actor": "local"}),
        json!({"direction": "inbound", "to_actor": null}),
        json!({"direction": "outbound"}),
        json!({}),
        json!({"direction": null, "from_actor": 7, "to_actor": []}),
        json!({"direction": "inbound", "to_actor": ["auditor"]}),
        json!({"direction": "inbound", "to_actor": "someone-else"}),
        json!({"direction": 7}),
    ];
    let mut notes: Vec<_> = shapes
        .into_iter()
        .enumerate()
        .map(|(index, mut properties)| {
            properties["tags"] = json!(["audit"]);
            note(index, "message", properties)
        })
        .collect();
    notes.push(note(10, "observation", json!({"tags": ["audit"]})));
    seed_notes(&runtime, notes).await;
    for (actor, indices) in [
        (Some("auditor"), vec![0, 1]),
        (Some("local"), vec![2]),
        (None, vec![2, 3, 4, 5, 6]),
    ] {
        for only_messages in [false, true] {
            let mut wanted = indices.clone();
            if !only_messages {
                wanted.push(10);
            }
            let expected: Vec<_> = wanted
                .iter()
                .map(|index| Uuid::from_u128(NOTE_ID_BASE + *index as u128).to_string())
                .collect();
            let mut args = json!({"tags": ["AUDIT"], "limit": 20});
            if only_messages {
                args["note_kind"] = json!("message");
            }
            let page = list(&registry, args.clone(), actor).await;
            assert_eq!(ids(&page, "items"), expected, "actor {actor:?}");
            args["after"] = json!("");
            let cursor_page = list(&registry, args, actor).await;
            assert_eq!(ids(&cursor_page, "notes"), expected, "actor {actor:?}");
            assert_eq!(cursor_page["has_more"], false);
            assert!(cursor_page["next_after"].is_null());
        }
    }
}
