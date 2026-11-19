use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use khive_db::StorageBackend;
use khive_storage::{
    BatchWriteSummary, Edge, Entity, EntityStore, Event, EventStore, GraphStore, Note, NoteStore,
};
use khive_types::{EdgeRelation, EventKind, EventOutcome, SubstrateKind};
use rand::{rngs::StdRng, RngExt, SeedableRng};
use tempfile::TempDir;
use tokio::runtime::Runtime;
use uuid::Uuid;

pub const NAMESPACE: &str = "bench_stores";
pub const CORPUS: usize = 10_000;
pub const EVENT_CORPUS: usize = 100_000;
pub const EPOCH: i64 = 1_700_000_000_000_000;
pub const FANOUT: usize = 8;

pub fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .enable_all()
        .build()
        .expect("benchmark runtime")
}

pub struct Fixture {
    pub entities: Arc<dyn EntityStore>,
    pub graph: Arc<dyn GraphStore>,
    pub notes: Arc<dyn NoteStore>,
    pub events: Arc<dyn EventStore>,
    backend: StorageBackend,
    dir: TempDir,
}

impl Fixture {
    pub fn new(rt: &Runtime) -> Self {
        let dir = tempfile::tempdir().expect("private benchmark directory");
        let _entered = rt.enter();
        let backend = StorageBackend::sqlite_for_test_with_journal_mode_in(
            dir.path().join("stores.db"),
            true,
            Duration::from_secs(30),
            dir.path().join("volume-locks"),
        )
        .expect("file-backed WAL backend with two readers");
        // Entity reads join the attachments table, which only the core migrations install.
        backend
            .pool()
            .run_migrations()
            .expect("core schema migrations");
        let entities = backend.entities_for_namespace(NAMESPACE).expect("entities");
        let notes = backend.notes_for_namespace(NAMESPACE).expect("notes");
        let graph = backend.graph_for_namespace(NAMESPACE).expect("graph");
        let events = backend.events_for_namespace(NAMESPACE).expect("events");
        // Never silently benchmark the fallback route after failed writer initialization.
        assert!(backend
            .pool()
            .writer_task_handle()
            .expect("writer task")
            .is_some());
        Self {
            entities,
            graph,
            notes,
            events,
            backend,
            dir,
        }
    }

    /// All leases and the writer task must finish before removing the fixture directory.
    /// Call after timing; no detached writer or accumulating database survives an iteration.
    pub fn close(self, rt: &Runtime) {
        let Self {
            entities,
            graph,
            notes,
            events,
            backend,
            dir,
        } = self;
        let writer = backend.pool().take_writer_task_join();
        drop((entities, graph, notes, events, backend));
        if let Some(writer) = writer {
            rt.block_on(writer).expect("drain benchmark writer");
        }
        dir.close().expect("remove benchmark database");
    }
}

pub fn complete(summary: BatchWriteSummary, count: usize) {
    assert_eq!(summary.attempted, count as u64, "{summary:?}");
    assert_eq!(summary.affected, count as u64, "{summary:?}");
    assert_eq!(summary.failed, 0, "{summary:?}");
}

fn id(family: u64, i: usize) -> Uuid {
    Uuid::from_u128((u128::from(family) << 64) | (i as u128 + 1))
}

pub fn node_id(i: usize) -> Uuid {
    id(1, i)
}

pub fn entities(count: usize) -> Vec<Entity> {
    let mut rng = StdRng::seed_from_u64(42);
    (0..count)
        .map(|i| {
            Entity::minimal(
                node_id(i),
                NAMESPACE,
                "concept",
                format!("entity_{i}"),
                EPOCH + i as i64,
                EPOCH + i as i64,
            )
            .with_description(format!("seeded payload {:016x}", rng.random::<u64>()))
        })
        .collect()
}

/// An eight-way tree, with one live entity per endpoint and exactly `count` edges.
pub fn edges(count: usize) -> Vec<Edge> {
    let time = DateTime::<Utc>::from_timestamp_micros(EPOCH).expect("fixture time");
    (0..count)
        .map(|i| Edge {
            id: id(2, i).into(),
            namespace: NAMESPACE.into(),
            source_id: node_id(i / FANOUT),
            target_id: node_id(i + 1),
            relation: EdgeRelation::Contains,
            weight: 1.0,
            created_at: time,
            updated_at: time,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        })
        .collect()
}

pub fn notes(count: usize) -> Vec<Note> {
    let mut rng = StdRng::seed_from_u64(43);
    (0..count)
        .map(|i| Note {
            id: id(3, i),
            namespace: NAMESPACE.into(),
            kind: "observation".into(),
            status: "active".into(),
            name: Some(format!("note_{i}")),
            content: format!("seeded payload {:016x}", rng.random::<u64>()),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(serde_json::json!({
                "p1": i % 100 < 1, "p10": i % 100 < 10, "p50": i % 100 < 50
            })),
            created_at: EPOCH + i as i64,
            updated_at: EPOCH + i as i64,
            deleted_at: None,
            key: None,
            version: 1,
        })
        .collect()
}

/// A range generator keeps the 100K-event setup bounded to 1000 inputs per append.
pub fn events(start: usize, count: usize) -> Vec<Event> {
    (start..start + count)
        .map(|i| Event {
            id: id(4, i),
            namespace: NAMESPACE.into(),
            verb: "create".into(),
            substrate: SubstrateKind::Entity,
            actor: "bench:generated".into(),
            kind: EventKind::Audit,
            outcome: EventOutcome::Success,
            payload: serde_json::json!({"fixture_index": i}),
            payload_schema_version: 1,
            profile_state_version: None,
            duration_us: 0,
            target_id: None,
            session_id: None,
            aggregate_kind: None,
            aggregate_id: None,
            created_at: EPOCH + i as i64,
            op_index: None,
            ref_resolution: None,
        })
        .collect()
}
