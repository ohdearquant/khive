use std::sync::Arc;

use khive_db::namespace_move::{move_namespace, MoveRequest, MoveRoute, SubjectClass};
use khive_gate::AllowAllGate;
use khive_storage::event::Event;
use khive_storage::note::Note;
use khive_types::{EventKind, Namespace, SubstrateKind};
use serde_json::json;
use uuid::Uuid;

use super::{KhiveRuntime, Resolved};
use crate::runtime::RuntimeConfig;

const SOURCE: &str = "event-lookup-source";
const TARGET: &str = "event-lookup-target";
const EVENT_TEXT: &str = "the source event remains after its observation moves";

struct MovedEventFixture {
    runtime: KhiveRuntime,
    expected: Event,
}

async fn moved_event_fixture() -> MovedEventFixture {
    let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: None,
        events_split: None,
        default_namespace: Namespace::parse(TARGET).unwrap(),
        visible_namespaces: Vec::new(),
        actor_id: Some("event-lookup-reader".into()),
        gate: Arc::new(AllowAllGate),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("real runtime without embeddings or an events daemon");
    let source_token = runtime
        .authorize(Namespace::parse(SOURCE).unwrap())
        .expect("source token");
    let note = Note::new(SOURCE, "observation", "the moved observation");
    let note_id = note.id;
    runtime
        .notes(&source_token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .expect("source note");
    let event = Event::new(
        SOURCE,
        "observation.created",
        EventKind::Audit,
        SubstrateKind::Note,
        "fixture actor before token attribution",
    )
    .with_target(note_id)
    .with_payload(json!({"text": EVENT_TEXT}));
    assert_ne!(
        event.id, note_id,
        "resolve must probe the event, not the moved note",
    );
    let event_id = event.id;
    let source_events = runtime.events(&source_token).unwrap();
    source_events
        .append_event(event)
        .await
        .expect("source event");
    let expected = source_events
        .get_event(event_id)
        .await
        .unwrap()
        .expect("the source event exists before the move");
    assert_eq!(expected.namespace, SOURCE);
    assert_eq!(expected.payload["text"], EVENT_TEXT);
    assert_eq!(expected.target_id, Some(note_id));

    let request = MoveRequest::new(
        SOURCE,
        vec![MoveRoute {
            class: SubjectClass::Note("observation".into()),
            target: TARGET.into(),
        }],
    );
    // The real pool owns BEGIN IMMEDIATE and settlement; the primitive remains DML-only.
    let counts = runtime
        .backend()
        .pool()
        .writer()
        .expect("move writer")
        .transaction(|connection| {
            move_namespace(connection, &request)
                .map_err(|error| khive_db::SqliteError::InvalidData(error.to_string()))
        })
        .expect("real namespace move commits");
    assert_eq!(counts.subjects.get("note:observation"), Some(&1));
    let target_token = runtime
        .authorize(Namespace::parse(TARGET).unwrap())
        .expect("target token");
    let moved = runtime
        .notes(&target_token)
        .unwrap()
        .get_note(note_id)
        .await
        .unwrap()
        .expect("the moved note exists");
    assert_eq!(moved.namespace, TARGET);
    assert_eq!(moved.id, note.id);
    assert_eq!(moved.kind, note.kind);
    assert_eq!(moved.content, note.content);
    assert_eq!(moved.properties, note.properties);
    assert_eq!(
        source_events.get_event(event_id).await.unwrap(),
        Some(expected.clone()),
        "the event remains byte-for-byte attributed to the source",
    );
    MovedEventFixture { runtime, expected }
}

#[tokio::test]
async fn resolve_moved_event_by_id_visibility() {
    let fixture = moved_event_fixture().await;
    let hidden = fixture
        .runtime
        .authorize(Namespace::parse(TARGET).unwrap())
        .expect("target-only token");
    assert!(!hidden
        .visible_namespaces()
        .iter()
        .any(|ns| ns.as_str() == SOURCE));
    assert!(fixture
        .runtime
        .resolve(&hidden, fixture.expected.id)
        .await
        .unwrap()
        .is_none());

    let visible = fixture
        .runtime
        .authorize_with_visibility(
            Namespace::parse(TARGET).unwrap(),
            vec![Namespace::parse(SOURCE).unwrap()],
        )
        .expect("target token with read visibility of source");
    assert!(visible
        .visible_namespaces()
        .iter()
        .any(|ns| ns.as_str() == SOURCE));
    let resolved = fixture
        .runtime
        .resolve(&visible, fixture.expected.id)
        .await
        .unwrap()
        .expect("the target resolver must find the source-visible event after the move");
    match resolved {
        Resolved::Event(event) => {
            assert_eq!(event.payload["text"], EVENT_TEXT);
            assert_eq!(event.namespace, SOURCE);
            assert_eq!(event, fixture.expected);
        }
        other => panic!("the event UUID resolved to the wrong substrate: {other:?}"),
    }
}

#[tokio::test]
async fn resolve_moved_event_by_id_hidden_source_stays_hidden() {
    let fixture = moved_event_fixture().await;
    let hidden = fixture
        .runtime
        .authorize(Namespace::parse(TARGET).unwrap())
        .expect("target-only token");
    assert!(!hidden
        .visible_namespaces()
        .iter()
        .any(|ns| ns.as_str() == SOURCE));
    assert!(fixture
        .runtime
        .resolve(&hidden, fixture.expected.id)
        .await
        .unwrap()
        .is_none());
    assert!(fixture
        .runtime
        .resolve(&hidden, Uuid::new_v4())
        .await
        .unwrap()
        .is_none());
}
