//! Link/event transaction contract through the existing public runtime APIs.
use std::fs::File;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use khive_db::StorageBackend;
use khive_runtime::{
    EdgeEndpointKind, KhiveRuntime, LinkSpec, Namespace, NamespaceToken, RuntimeConfig,
    RuntimeError,
};
use khive_storage::graph::{
    CommitAnnotationCursorValue, CommitAnnotationGuard, CommitAnnotationInsertOutcome,
};
use khive_storage::{
    DeleteMode, Edge, EdgeRelation, EdgeUpsertDisposition, Entity, Event, EventFilter, LinkId,
    Note, PageRequest, SqlStatement, SqlValue,
};
use khive_types::{EventKind, OperationAttribution, RefResolution, SubstrateKind};
use serde_json::{json, Value};
use uuid::Uuid;

const ROUTE_ENV: &str = "KHIVE_LINK_EVENT_FIXTURE_ROUTE";
const CASE_ENV: &str = "KHIVE_LINK_EVENT_FIXTURE_CASE";

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

struct Fixture {
    runtime: KhiveRuntime,
    token: NamespaceToken,
    _dir: tempfile::TempDir,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let route = std::env::var(ROUTE_ENV).unwrap();
        let path = dir.path().join("links.sqlite3");
        let backend = Arc::new(if route == "memory" {
            StorageBackend::memory().unwrap()
        } else {
            StorageBackend::sqlite_for_test(&path).unwrap()
        });
        backend.prepare_core_schema().unwrap();
        let runtime = KhiveRuntime::from_backend(
            backend,
            RuntimeConfig {
                db_path: (route != "memory").then_some(path),
                events_split: None,
                actor_id: Some("test:link-events".into()),
                packs: vec!["kg".into()],
                ..RuntimeConfig::no_embeddings()
            },
        );
        let token = runtime.authorize(Namespace::local()).unwrap();
        runtime.graph(&token).unwrap();
        runtime.events(&token).unwrap();
        for n in 1..=3 {
            let mut e = Entity::new("local", "concept", format!("endpoint {n}"));
            e.id = id(n);
            runtime
                .entities(&token)
                .unwrap()
                .upsert_entity(e)
                .await
                .unwrap();
        }
        if route != "memory" {
            let pool = runtime.backend().pool();
            assert_eq!(pool.config().write_routing_strict, route == "writer");
            assert_eq!(
                pool.writer_task_handle().unwrap().is_some(),
                route == "writer"
            );
        }
        Self {
            runtime,
            token,
            _dir: dir,
        }
    }
    async fn script(&self, script: impl Into<String>) {
        self.runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute_script(script.into())
            .await
            .unwrap();
    }
    async fn rows(&self, sql: &str) -> Value {
        let rows = self
            .runtime
            .sql()
            .reader()
            .await
            .unwrap()
            .query_all(SqlStatement {
                sql: sql.into(),
                params: vec![],
                label: Some("link-event-fixture-snapshot".into()),
            })
            .await
            .unwrap();
        serde_json::to_value(rows).unwrap()
    }
    async fn snapshot(&self) -> Value {
        let cursor = if self
            .rows("SELECT name FROM sqlite_master WHERE type='table' AND name='git_mirror_cursor'")
            .await
            .as_array()
            .unwrap()
            .is_empty()
        {
            Value::Null
        } else {
            self.rows("SELECT * FROM git_mirror_cursor ORDER BY project_id,kind")
                .await
        };
        json!({"cursor":cursor,"edges":self.rows("SELECT * FROM graph_edges ORDER BY namespace,id").await,
        "ledger":self.rows("SELECT * FROM graph_edges_seq ORDER BY seq").await,
        "sequence":self.rows("SELECT * FROM sqlite_sequence WHERE name='graph_edges_seq'").await,
        "events":self.rows("SELECT * FROM events ORDER BY id").await,
        "observations":self.rows("SELECT * FROM event_observations ORDER BY event_id,role,position").await})
    }
    async fn edge(&self, source: Uuid, target: Uuid, relation: EdgeRelation) -> Option<Edge> {
        self.runtime
            .get_edge_by_natural_key_including_deleted(
                &self.token,
                "local",
                source,
                target,
                relation,
            )
            .await
            .unwrap()
    }
    async fn link_events(&self) -> Vec<Event> {
        self.runtime
            .events(&self.token)
            .unwrap()
            .query_events(
                EventFilter {
                    verbs: vec!["link".into()],
                    ..Default::default()
                },
                PageRequest {
                    offset: 0,
                    limit: 100,
                },
            )
            .await
            .unwrap()
            .items
    }
    async fn observations(&self, event: Uuid) -> Vec<(String, String, String, i64)> {
        let rows = self.runtime.sql().reader().await.unwrap().query_all(SqlStatement {sql:"SELECT event_id,entity_id,referent_kind,role,position FROM event_observations WHERE event_id=?1 ORDER BY role,position".into(),params:vec![SqlValue::Text(event.to_string())],label:None}).await.unwrap();
        rows.into_iter().map(|row| {
            assert!(matches!(row.get("event_id"), Some(SqlValue::Text(value)) if value == &event.to_string()));
            let text = |key| match row.get(key) {Some(SqlValue::Text(value))=>value.clone(),other=>panic!("unexpected {key}: {other:?}")};
            let position=match row.get("position") {Some(SqlValue::Integer(value))=>*value,other=>panic!("bad position: {other:?}")};
            (text("entity_id"),text("referent_kind"),text("role"),position)
        }).collect()
    }
}
fn spec(target: Uuid) -> LinkSpec {
    LinkSpec {
        namespace: None,
        source_id: id(1),
        target_id: target,
        relation: EdgeRelation::Extends,
        weight: 0.5,
        metadata: Some(json!({"fixture":"new"})),
        resurrect: false,
    }
}

#[derive(Clone, Copy)]
enum Path {
    Single,
    Cross,
    Batch,
    Annotation,
}
async fn annotation_setup(f: &Fixture) -> CommitAnnotationGuard {
    let mut project = Entity::new("local", "project", "source project");
    project.id = id(2);
    project.properties = Some(json!({"repo_slug":"fixture/source"}));
    f.runtime
        .entities(&f.token)
        .unwrap()
        .upsert_entity(project)
        .await
        .unwrap();
    let mut commit = Note::new("local", "commit", "fixed commit");
    commit.id = id(1);
    commit.properties = Some(json!({"sha":"a".repeat(40)}));
    // A commit note, rather than the concept originally seeded at this UUID, owns the endpoint.
    f.runtime
        .entities(&f.token)
        .unwrap()
        .delete_entity(id(1), DeleteMode::Hard)
        .await
        .unwrap();
    f.runtime
        .notes(&f.token)
        .unwrap()
        .upsert_note(commit)
        .await
        .unwrap();
    f.script(format!("CREATE TABLE IF NOT EXISTS git_mirror_cursor(project_id TEXT NOT NULL,kind TEXT NOT NULL,cursor_value TEXT,updated_at INTEGER NOT NULL,PRIMARY KEY(project_id,kind)); INSERT INTO git_mirror_cursor VALUES ('{}','commits','cursor commits',41),('{}','commits_checkpoint','cursor checkpoint',42);",id(2),id(2))).await;
    CommitAnnotationGuard {
        expected_sha: "a".repeat(40),
        source_identity: "fixture/source".into(),
        commits: CommitAnnotationCursorValue {
            value: b"cursor commits".to_vec(),
            updated_at: 41,
        },
        checkpoint: CommitAnnotationCursorValue {
            value: b"cursor checkpoint".to_vec(),
            updated_at: 42,
        },
    }
}
async fn attempt(
    f: &Fixture,
    path: Path,
    guard: Option<CommitAnnotationGuard>,
) -> Result<Edge, RuntimeError> {
    match path {
        Path::Single => f
            .runtime
            .link_observed(
                &f.token,
                id(1),
                id(2),
                EdgeRelation::Extends,
                0.5,
                Some(json!({"fixture":"new"})),
                false,
            )
            .await
            .map(|r| {
                assert_eq!(r.disposition, EdgeUpsertDisposition::Created);
                r.edge
            }),
        Path::Cross => f
            .runtime
            .link_with_target_backend_observed(
                &f.token,
                id(1),
                id(2),
                EdgeEndpointKind::Entity,
                EdgeEndpointKind::Entity,
                EdgeRelation::Extends,
                0.5,
                None,
                Some("remote".into()),
                false,
            )
            .await
            .map(|r| {
                assert_eq!(r.disposition, EdgeUpsertDisposition::Created);
                r.edge
            }),
        Path::Batch => f
            .runtime
            .link_many_observed(&f.token, vec![spec(id(2))])
            .await
            .map(|mut r| {
                assert_eq!(r.len(), 1);
                let r = r.remove(0);
                assert_eq!(r.disposition, EdgeUpsertDisposition::Created);
                r.edge
            }),
        Path::Annotation => f
            .runtime
            .link_commit_annotation_if_absent(&f.token, id(1), id(2), guard.unwrap())
            .await
            .map(|r| match r {
                CommitAnnotationInsertOutcome::Created(edge) => edge,
                other => panic!("expected Created, got {other:?}"),
            }),
    }
}
async fn assert_created(f: &Fixture, edge: &Edge, source_kind: &str, target_kind: &str) {
    let events = f.link_events().await;
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.kind, EventKind::LinkCreated);
    assert_eq!(event.target_id, Some(Uuid::from(edge.id)));
    assert_eq!(
        event.payload,
        json!({"id":Uuid::from(edge.id),"namespace":"local","mutation":"created","source_id":edge.source_id,"target_id":edge.target_id,"relation":edge.relation,"weight":edge.weight,"metadata":edge.metadata,"previous":null,"source_kind":source_kind,"target_kind":target_kind})
    );
    assert_eq!(
        f.observations(event.id).await,
        vec![
            (
                edge.source_id.to_string(),
                source_kind.into(),
                "target".into(),
                0
            ),
            (
                edge.target_id.to_string(),
                target_kind.into(),
                "target".into(),
                1
            ),
            (
                Uuid::from(edge.id).to_string(),
                "edge".into(),
                "target".into(),
                2
            )
        ]
    );
}
async fn event_failure(path: Path) {
    let f = Fixture::new().await;
    let guard = if matches!(path, Path::Annotation) {
        Some(annotation_setup(&f).await)
    } else {
        None
    };
    if matches!(path, Path::Cross) {
        f.runtime
            .entities(&f.token)
            .unwrap()
            .delete_entity(id(2), DeleteMode::Hard)
            .await
            .unwrap();
    }
    let before = f.snapshot().await;
    f.script(format!("CREATE TRIGGER c9_event_fault BEFORE INSERT ON events WHEN NEW.verb='link' AND EXISTS(SELECT 1 FROM graph_edges WHERE source_id='{}' AND target_id='{}' AND deleted_at IS NULL) BEGIN SELECT RAISE(ABORT,'c9-event-after-edge'); END;",id(1),id(2))).await;
    let error = attempt(&f, path, guard.clone()).await.unwrap_err();
    assert!(
        error.to_string().contains("c9-event-after-edge"),
        "fault must run after graph DML: {error:?}"
    );
    assert!(
        f.edge(
            id(1),
            id(2),
            if matches!(path, Path::Annotation) {
                EdgeRelation::Annotates
            } else {
                EdgeRelation::Extends
            }
        )
        .await
        .is_none(),
        "event failure must roll back the new edge"
    );
    assert_eq!(
        f.snapshot().await,
        before,
        "edge, ledger, event and projection unit must roll back"
    );
    f.script("DROP TRIGGER c9_event_fault").await;
    let edge = attempt(&f, path, guard).await.unwrap();
    assert_created(
        &f,
        &edge,
        if matches!(path, Path::Annotation) {
            "note"
        } else {
            "entity"
        },
        "entity",
    )
    .await;
    if matches!(path, Path::Cross) {
        assert_eq!(edge.target_backend.as_deref(), Some("remote"));
    }
}
async fn batch_failure(projection: bool) {
    let f = Fixture::new().await;
    let before = f.snapshot().await;
    let script = if projection {
        format!("CREATE TRIGGER c9_second_fault BEFORE INSERT ON event_observations WHEN NEW.entity_id='{}' AND EXISTS(SELECT 1 FROM graph_edges WHERE source_id='{}' AND target_id='{}') AND EXISTS(SELECT 1 FROM events WHERE verb='link' AND json_extract(payload,'$.target_id')='{}') BEGIN SELECT RAISE(ABORT,'c9-second-projection'); END;",id(3),id(1),id(3),id(2))
    } else {
        format!("CREATE TRIGGER c9_second_fault BEFORE INSERT ON events WHEN NEW.verb='link' AND json_extract(NEW.payload,'$.target_id')='{}' AND EXISTS(SELECT 1 FROM graph_edges WHERE source_id='{}' AND target_id='{}') AND EXISTS(SELECT 1 FROM events WHERE verb='link' AND json_extract(payload,'$.target_id')='{}') BEGIN SELECT RAISE(ABORT,'c9-second-event'); END;",id(3),id(1),id(3),id(2))
    };
    f.script(script).await;
    let error = f
        .runtime
        .link_many_observed(&f.token, vec![spec(id(2)), spec(id(3))])
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains(if projection {
            "c9-second-projection"
        } else {
            "c9-second-event"
        }),
        "must reach row2 after row1 event: {error:?}"
    );
    assert_eq!(
        f.snapshot().await,
        before,
        "row2 failure must roll back every edge, event, projection and ledger row"
    );
    f.script("DROP TRIGGER c9_second_fault").await;
    let rows = f
        .runtime
        .link_many_observed(&f.token, vec![spec(id(2)), spec(id(3))])
        .await
        .unwrap();
    assert_eq!(
        rows.iter().map(|r| r.edge.target_id).collect::<Vec<_>>(),
        vec![id(2), id(3)]
    );
    assert!(rows
        .iter()
        .all(|r| r.disposition == EdgeUpsertDisposition::Created));
    assert_eq!(f.link_events().await.len(), 2);
}
async fn edge_failure_all() {
    for path in [Path::Single, Path::Cross, Path::Batch, Path::Annotation] {
        let f = Fixture::new().await;
        let guard = if matches!(path, Path::Annotation) {
            Some(annotation_setup(&f).await)
        } else {
            None
        };
        if matches!(path, Path::Cross) {
            f.runtime
                .entities(&f.token)
                .unwrap()
                .delete_entity(id(2), DeleteMode::Hard)
                .await
                .unwrap();
        }
        let before = f.snapshot().await;
        f.script(format!("CREATE TRIGGER c9_edge_fault BEFORE INSERT ON graph_edges WHEN NEW.source_id='{}' AND NEW.target_id='{}' BEGIN SELECT RAISE(ABORT,'c9-edge-insert'); END;",id(1),id(2))).await;
        let error = attempt(&f, path, guard.clone()).await.unwrap_err();
        assert!(
            error.to_string().contains("c9-edge-insert"),
            "actual graph DML fault: {error:?}"
        );
        assert!(
            f.link_events().await.is_empty(),
            "failed graph insert must emit no lifecycle event"
        );
        assert_eq!(f.snapshot().await, before);
        f.script("DROP TRIGGER c9_edge_fault").await;
        let edge = attempt(&f, path, guard).await.unwrap();
        assert_created(
            &f,
            &edge,
            if matches!(path, Path::Annotation) {
                "note"
            } else {
                "entity"
            },
            "entity",
        )
        .await;
    }
}
async fn replacement_and_resurrection() {
    for relation in [EdgeRelation::Extends, EdgeRelation::CompetesWith] {
        for resurrect in [false, true] {
            let f = Fixture::new().await;
            let now = Utc.timestamp_micros(1_000_000).single().unwrap();
            let seed = Edge {
                id: LinkId::from(id(50)),
                namespace: "local".into(),
                source_id: id(1),
                target_id: id(2),
                relation,
                weight: 0.25,
                created_at: now,
                updated_at: now,
                deleted_at: None,
                metadata: Some(json!({"fixture":"previous","nested":{"value":1}})),
                target_backend: None,
            };
            f.runtime
                .graph(&f.token)
                .unwrap()
                .upsert_edge(seed)
                .await
                .unwrap();
            if resurrect {
                f.runtime
                    .graph(&f.token)
                    .unwrap()
                    .delete_edge(LinkId::from(id(50)), DeleteMode::Soft)
                    .await
                    .unwrap();
            }
            let previous = f.edge(id(1), id(2), relation).await.unwrap();
            let before = f.snapshot().await;
            if resurrect {
                let (source, target) = if relation == EdgeRelation::CompetesWith {
                    (id(2), id(1))
                } else {
                    (id(1), id(2))
                };
                let refusal = f
                    .runtime
                    .link_observed(&f.token, source, target, relation, 0.75, None, false)
                    .await;
                assert!(matches!(refusal, Err(RuntimeError::InvalidInput(_))));
                assert_eq!(f.snapshot().await, before);
            }
            f.script("CREATE TRIGGER c9_replace_fault BEFORE INSERT ON events WHEN NEW.verb='link' BEGIN SELECT RAISE(ABORT,'c9-replace-event'); END;").await;
            let (source, target) = if relation == EdgeRelation::CompetesWith {
                (id(2), id(1))
            } else {
                (id(1), id(2))
            };
            let error = f
                .runtime
                .link_observed(
                    &f.token,
                    source,
                    target,
                    relation,
                    0.75,
                    Some(json!({"fixture":"replacement"})),
                    resurrect,
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("c9-replace-event"));
            assert_eq!(
                serde_json::to_value(f.edge(id(1), id(2), relation).await.unwrap()).unwrap(),
                serde_json::to_value(&previous).unwrap(),
                "failed update must retain every preimage field"
            );
            assert_eq!(f.snapshot().await, before);
            f.script("DROP TRIGGER c9_replace_fault").await;
            let result = f
                .runtime
                .link_observed(
                    &f.token,
                    source,
                    target,
                    relation,
                    0.75,
                    Some(json!({"fixture":"replacement"})),
                    resurrect,
                )
                .await
                .unwrap();
            assert_eq!(
                result.disposition,
                if resurrect {
                    EdgeUpsertDisposition::Resurrected
                } else {
                    EdgeUpsertDisposition::Updated
                }
            );
            assert_eq!(result.edge.id, previous.id);
            assert_eq!(result.edge.created_at, previous.created_at);
            assert_eq!(result.edge.source_id, id(1));
            assert_eq!(result.edge.target_id, id(2));
            assert_eq!(
                serde_json::to_value(result.previous.as_ref().unwrap()).unwrap(),
                serde_json::to_value(&previous).unwrap()
            );
            let events = f.link_events().await;
            assert_eq!(events.len(), 1);
            assert_eq!(
                events[0].kind,
                EventKind::EdgeUpdated,
                "resurrection is an update lifecycle event"
            );
            assert_eq!(
                events[0].payload["previous"],
                serde_json::to_value(&previous).unwrap()
            );
        }
    }
    let f = Fixture::new().await;
    f.runtime
        .entities(&f.token)
        .unwrap()
        .delete_entity(id(2), DeleteMode::Hard)
        .await
        .unwrap();
    let before = f.snapshot().await;
    assert!(f
        .runtime
        .link_observed(
            &f.token,
            id(1),
            id(2),
            EdgeRelation::Extends,
            1.0,
            None,
            false
        )
        .await
        .is_err());
    assert_eq!(f.snapshot().await, before);
}
async fn annotation_outcomes() {
    event_failure(Path::Annotation).await;
    let f = Fixture::new().await;
    let guard = annotation_setup(&f).await;
    let cursor_before = f
        .rows("SELECT * FROM git_mirror_cursor ORDER BY kind")
        .await;
    let edge = attempt(&f, Path::Annotation, Some(guard.clone()))
        .await
        .unwrap();
    assert_created(&f, &edge, "note", "entity").await;
    assert!(matches!(
        f.runtime
            .link_commit_annotation_if_absent(&f.token, id(1), id(2), guard.clone())
            .await
            .unwrap(),
        CommitAnnotationInsertOutcome::ExistingLive
    ));
    f.runtime
        .graph(&f.token)
        .unwrap()
        .delete_edge(edge.id, DeleteMode::Soft)
        .await
        .unwrap();
    assert!(matches!(
        f.runtime
            .link_commit_annotation_if_absent(&f.token, id(1), id(2), guard.clone())
            .await
            .unwrap(),
        CommitAnnotationInsertOutcome::Tombstoned
    ));
    let mut changed = guard.clone();
    changed.commits.updated_at += 1;
    assert!(matches!(
        f.runtime
            .link_commit_annotation_if_absent(&f.token, id(1), id(2), changed)
            .await
            .unwrap(),
        CommitAnnotationInsertOutcome::CursorChanged
    ));
    let mut changed = guard.clone();
    changed.source_identity = "fixture/other".into();
    assert!(matches!(
        f.runtime
            .link_commit_annotation_if_absent(&f.token, id(1), id(2), changed)
            .await
            .unwrap(),
        CommitAnnotationInsertOutcome::TargetChanged
    ));
    let mut changed = guard;
    changed.expected_sha = "b".repeat(40);
    assert!(matches!(
        f.runtime
            .link_commit_annotation_if_absent(&f.token, id(1), id(2), changed)
            .await
            .unwrap(),
        CommitAnnotationInsertOutcome::SourceChanged
    ));
    assert_eq!(
        f.link_events().await.len(),
        1,
        "every create-only no-write outcome is event-free"
    );
    assert_eq!(
        f.rows("SELECT * FROM git_mirror_cursor ORDER BY kind")
            .await,
        cursor_before
    );
}
async fn attribution_and_note_event() {
    let f = Fixture::new().await;
    let mut note = Note::new("local", "observation", "event annotation");
    note.id = id(70);
    f.runtime
        .notes(&f.token)
        .unwrap()
        .upsert_note(note)
        .await
        .unwrap();
    let target = Event::new(
        "local",
        "fixture",
        EventKind::Audit,
        SubstrateKind::Entity,
        "untrusted:stamp",
    );
    let target_id = target.id;
    f.runtime
        .events(&f.token)
        .unwrap()
        .append_event(target)
        .await
        .unwrap();
    let operation = OperationAttribution {
        op_index: 7,
        ref_resolution: RefResolution::Resolved,
    };
    let edge = khive_storage::operation_context::scope_operation_attribution(
        operation,
        f.runtime.link_observed(
            &f.token,
            id(70),
            target_id,
            EdgeRelation::Annotates,
            1.0,
            None,
            false,
        ),
    )
    .await
    .unwrap()
    .edge;
    let events = f.link_events().await;
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.namespace, "local");
    assert_eq!(
        event.actor,
        format!("{}:{}", f.token.actor().kind, f.token.actor().id)
    );
    assert_eq!(event.op_index, Some(7));
    assert_eq!(event.ref_resolution, Some(RefResolution::Resolved));
    assert_eq!(event.payload["source_kind"], "note");
    assert_eq!(event.payload["target_kind"], "event");
    assert_eq!(
        f.observations(event.id).await,
        vec![
            (id(70).to_string(), "note".into(), "target".into(), 0),
            (
                Uuid::from(edge.id).to_string(),
                "edge".into(),
                "target".into(),
                2
            )
        ],
        "event endpoints are omitted, note/edge positions retained"
    );
}
async fn exercise(name: &str) {
    match name {
        "link_observed_event_failure_rolls_back_and_retry_creates_once" => {
            event_failure(Path::Single).await
        }
        "cross_backend_event_failure_rolls_back_on_source" => {
            let target = Fixture::new().await;
            let before = target.snapshot().await;
            event_failure(Path::Cross).await;
            assert_eq!(
                target.snapshot().await,
                before,
                "cross-backend target store is outside and unchanged"
            );
        }
        "link_many_second_event_failure_rolls_back_every_row" => batch_failure(false).await,
        "link_many_second_projection_failure_rolls_back_every_row" => batch_failure(true).await,
        "link_paths_edge_failure_emits_no_event" => edge_failure_all().await,
        "replacement_and_resurrection_failure_preserve_preimage_then_retry" => {
            replacement_and_resurrection().await
        }
        "commit_annotation_event_failure_rolls_back_and_create_only_outcomes_keep_cursors" => {
            annotation_outcomes().await
        }
        "link_attribution_and_note_event_projection_stay_on_source" => {
            attribution_and_note_event().await
        }
        _ => panic!("unknown fixture case {name}"),
    }
}
fn run_case(name: &str) {
    if std::env::var(CASE_ENV).as_deref() == Ok(name) {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(exercise(name));
        return;
    }
    for route in ["memory", "compat", "writer"] {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("child-output.txt");
        let output = File::create(&output_path).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("KHIVE_") {
                command.env_remove(key);
            }
        }
        let mut child = command
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .current_dir(dir.path())
            .env("KHIVE_TEST_HARNESS", "1")
            .env(CASE_ENV, name)
            .env(ROUTE_ENV, route)
            .env(
                "KHIVE_WRITE_QUEUE",
                if route == "writer" { "1" } else { "0" },
            )
            .env(
                "KHIVE_WRITE_ROUTING",
                if route == "writer" {
                    "strict"
                } else {
                    "compat"
                },
            )
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("{route} watchdog expired; no semantic result");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let output = std::fs::read_to_string(output_path).unwrap();
        assert!(status.success(), "{route} exact child failed:\n{output}");
        assert!(
            output.contains("1 passed; 0 failed"),
            "nonzero exact child selection required: {output}"
        );
    }
}
#[test]
fn link_observed_event_failure_rolls_back_and_retry_creates_once() {
    run_case("link_observed_event_failure_rolls_back_and_retry_creates_once")
}
#[test]
fn cross_backend_event_failure_rolls_back_on_source() {
    run_case("cross_backend_event_failure_rolls_back_on_source")
}
#[test]
fn link_many_second_event_failure_rolls_back_every_row() {
    run_case("link_many_second_event_failure_rolls_back_every_row")
}
#[test]
fn link_many_second_projection_failure_rolls_back_every_row() {
    run_case("link_many_second_projection_failure_rolls_back_every_row")
}
#[test]
fn link_paths_edge_failure_emits_no_event() {
    run_case("link_paths_edge_failure_emits_no_event")
}
#[test]
fn replacement_and_resurrection_failure_preserve_preimage_then_retry() {
    run_case("replacement_and_resurrection_failure_preserve_preimage_then_retry")
}
#[test]
fn commit_annotation_event_failure_rolls_back_and_create_only_outcomes_keep_cursors() {
    run_case("commit_annotation_event_failure_rolls_back_and_create_only_outcomes_keep_cursors")
}
#[test]
fn link_attribution_and_note_event_projection_stay_on_source() {
    run_case("link_attribution_and_note_event_projection_stay_on_source")
}
