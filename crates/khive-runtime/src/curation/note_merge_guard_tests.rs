//! Witnesses for the guarded note merge: expected versions, assertions,
//! survivor property overrides and the history annotation.

use super::*;
use crate::{Namespace, RuntimeConfig};
use khive_storage::types::VectorRecord;
use serde_json::json;

const OTHER_NAMESPACE: &str = "guarded-merge-other";
const MODEL: &str = "guarded-merge-const";

struct Fixture {
    runtime: KhiveRuntime,
    token: NamespaceToken,
    foreign: NamespaceToken,
    _directory: Option<tempfile::TempDir>,
}

impl Fixture {
    fn new(file_backed: bool) -> Self {
        let directory = file_backed.then(|| tempfile::tempdir().expect("fixture directory"));
        let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
            db_path: directory
                .as_ref()
                .map(|dir| dir.path().join("guarded-merge.db")),
            actor_id: Some("test:guarded-merge-fixture".into()),
            brain_profile: None,
            events_split: None,
            ..RuntimeConfig::no_embeddings()
        })
        .expect("private model-less runtime");
        let token = runtime
            .authorize(Namespace::local())
            .expect("fixture actor");
        let foreign = runtime
            .authorize(Namespace::parse(OTHER_NAMESPACE).expect("namespace"))
            .expect("fixture actor in another namespace");
        // The module runs in separate queue=0 and queue=1 processes. Assert the
        // actual route rather than letting both runs cover one mode.
        if let Ok(queue) = std::env::var("KHIVE_WRITE_QUEUE") {
            if queue == "0" || queue == "1" {
                assert_eq!(
                    runtime
                        .backend()
                        .pool_arc()
                        .writer_task_handle()
                        .unwrap()
                        .is_some(),
                    file_backed && queue == "1",
                    "fixture must exercise the requested writer route"
                );
            }
        }
        Self {
            runtime,
            token,
            foreign,
            _directory: directory,
        }
    }

    fn namespace(&self) -> String {
        self.token.namespace().as_str().to_string()
    }

    async fn note_of(&self, kind: &str, content: &str, properties: Option<Value>) -> Note {
        let created = self
            .runtime
            .create_note(&self.token, kind, None, content, None, properties, vec![])
            .await
            .expect("seed note");
        self.current(created.id).await
    }

    async fn note(&self, content: &str) -> Note {
        self.note_of("observation", content, None).await
    }

    async fn current(&self, id: Uuid) -> Note {
        self.runtime
            .notes(&self.token)
            .expect("note store")
            .get_note(id)
            .await
            .expect("read note")
            .expect("note present")
    }

    async fn entity(&self, kind: &str, name: &str) -> Uuid {
        self.runtime
            .create_entity(&self.token, kind, None, name, None, None, vec![])
            .await
            .expect("seed entity")
            .id
    }

    async fn foreign_entity(&self, kind: &str, name: &str) -> Uuid {
        self.runtime
            .create_entity(&self.foreign, kind, None, name, None, None, vec![])
            .await
            .expect("seed entity in another namespace")
            .id
    }

    /// An edge on `note` that a successful merge of that note would rewire.
    async fn attach(&self, note: Uuid) -> Uuid {
        let anchor = self
            .entity("concept", &format!("Merge anchor {note}"))
            .await;
        self.link(note, anchor, EdgeRelation::Annotates).await;
        anchor
    }

    async fn link(&self, source: Uuid, target: Uuid, relation: EdgeRelation) -> Uuid {
        let edge = self
            .runtime
            .link(&self.token, source, target, relation, 1.0, None)
            .await
            .expect("seed edge");
        Uuid::from(edge.id)
    }

    async fn execute(&self, sql: &str, params: Vec<SqlValue>) {
        let access = self.runtime.sql();
        let mut writer = access.writer().await.expect("fixture writer");
        writer
            .execute(SqlStatement {
                sql: sql.to_string(),
                params,
                label: Some("test.guarded_merge.fixture".into()),
            })
            .await
            .expect("fixture statement");
    }

    async fn scalar(&self, sql: &str, params: Vec<SqlValue>) -> Option<SqlValue> {
        let access = self.runtime.sql();
        let mut reader = access.reader().await.expect("fixture reader");
        reader
            .query_scalar(SqlStatement {
                sql: sql.to_string(),
                params,
                label: Some("test.guarded_merge.read".into()),
            })
            .await
            .expect("fixture read")
    }

    /// A row the runtime would not write itself: any namespace, any liveness.
    async fn insert_edge(
        &self,
        namespace: &str,
        source: Uuid,
        target: Uuid,
        relation: EdgeRelation,
        live: bool,
    ) {
        self.execute(
            "INSERT INTO graph_edges (namespace, id, source_id, target_id, relation, weight, \
             created_at, updated_at, deleted_at) VALUES (?1, ?2, ?3, ?4, ?5, 1.0, 1, 1, ?6)",
            vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(Uuid::new_v4().to_string()),
                SqlValue::Text(source.to_string()),
                SqlValue::Text(target.to_string()),
                SqlValue::Text(relation.as_str().to_string()),
                if live {
                    SqlValue::Null
                } else {
                    SqlValue::Integer(1)
                },
            ],
        )
        .await;
    }

    /// Mark every edge from `source` to `target` as stored in another store.
    async fn move_edge_to_other_store(&self, source: Uuid, target: Uuid) {
        self.execute(
            "UPDATE graph_edges SET target_backend = 'remote-store' \
             WHERE source_id = ?1 AND target_id = ?2",
            vec![
                SqlValue::Text(source.to_string()),
                SqlValue::Text(target.to_string()),
            ],
        )
        .await;
    }

    /// Record `entity` as merged into `merged_into`, as an entity merge does.
    async fn tombstone(&self, entity: Uuid, merged_into: Uuid) {
        self.execute(
            "UPDATE entities SET deleted_at = 1, merged_into = ?1, version = version + 1 \
             WHERE id = ?2",
            vec![
                SqlValue::Text(merged_into.to_string()),
                SqlValue::Text(entity.to_string()),
            ],
        )
        .await;
    }

    async fn set_kind(&self, kind: &str, notes: &[&Note]) {
        for note in notes {
            self.execute(
                "UPDATE notes SET kind = ?1 WHERE id = ?2",
                vec![
                    SqlValue::Text(kind.to_string()),
                    SqlValue::Text(note.id.to_string()),
                ],
            )
            .await;
        }
    }

    /// Store `raw` (JSON text, any shape) as the note's properties.
    async fn set_properties(&self, note: Uuid, raw: &str) {
        self.execute(
            "UPDATE notes SET properties = ?1 WHERE id = ?2",
            vec![
                SqlValue::Text(raw.to_string()),
                SqlValue::Text(note.to_string()),
            ],
        )
        .await;
    }

    async fn tombstoned(&self, note: Uuid) -> bool {
        matches!(
            self.scalar(
                "SELECT deleted_at IS NOT NULL FROM notes WHERE id = ?1",
                vec![SqlValue::Text(note.to_string())],
            )
            .await,
            Some(SqlValue::Integer(1))
        )
    }

    async fn merge_with(
        &self,
        into: Uuid,
        from: Uuid,
        strategy: EntityDedupMergePolicy,
        guard: NoteMergeGuard,
        dry_run: bool,
    ) -> RuntimeResult<GuardedNoteMerge> {
        self.runtime
            .merge_note_guarded(
                &self.token,
                into,
                from,
                strategy,
                ContentMergeStrategy::Append,
                dry_run,
                None,
                guard,
            )
            .await
    }

    async fn merge(
        &self,
        into: Uuid,
        from: Uuid,
        guard: NoteMergeGuard,
        dry_run: bool,
    ) -> RuntimeResult<GuardedNoteMerge> {
        self.merge_with(
            into,
            from,
            EntityDedupMergePolicy::PreferInto,
            guard,
            dry_run,
        )
        .await
    }

    /// The assertion holds: a dry run evaluates the whole guard and writes
    /// nothing.
    async fn holds(&self, into: &Note, from: &Note, assertion: MergeAssertion) {
        let preview = self
            .merge(
                into.id,
                from.id,
                guard_for(into, from, vec![assertion]),
                true,
            )
            .await
            .expect("the assertion holds");
        assert!(preview.summary.dry_run);
    }

    async fn snapshot(&self) -> Value {
        // Prime each lazy schema before taking a snapshot, not in the assertion.
        self.runtime.entities(&self.token).unwrap();
        self.runtime.notes(&self.token).unwrap();
        self.runtime.graph(&self.token).unwrap();
        self.runtime.text(&self.token).unwrap();
        self.runtime.text_for_notes(&self.token).unwrap();
        self.runtime.events(&self.token).unwrap();
        let sql = self.runtime.sql();
        let mut reader = sql.reader().await.expect("snapshot reader");
        let mut snapshot = serde_json::Map::new();
        for table in [
            "entities",
            "notes",
            "graph_edges",
            "notes_seq",
            "attachments",
            "events",
            "fts_entities",
            "fts_notes",
        ] {
            let rows = reader
                .query_all(SqlStatement {
                    sql: format!("SELECT rowid, * FROM {table} ORDER BY rowid"),
                    params: vec![],
                    label: Some("test.guarded_merge.snapshot".into()),
                })
                .await
                .expect("domain and index snapshot");
            snapshot.insert(table.into(), serde_json::to_value(rows).unwrap());
        }
        Value::Object(snapshot)
    }

    /// Install (or remove) triggers that abort any write to the domain tables,
    /// so a refusal that returns its own error proves it came before the first
    /// write.
    async fn veto_domain_dml(&self, on: bool) {
        for table in ["entities", "notes", "graph_edges"] {
            for operation in ["INSERT", "UPDATE", "DELETE"] {
                let name = format!("test_guarded_merge_veto_{table}_{operation}");
                let sql = if on {
                    format!(
                        "CREATE TRIGGER IF NOT EXISTS {name} BEFORE {operation} ON {table} \
                         BEGIN SELECT RAISE(ABORT, 'fixture forbids domain writes'); END"
                    )
                } else {
                    format!("DROP TRIGGER IF EXISTS {name}")
                };
                self.execute(&sql, vec![]).await;
            }
        }
    }

    /// Run a real merge that must be refused with `expected` in its message,
    /// and leave every row, index entry, edge and event as it was.
    async fn refuses_merge(
        &self,
        into: &Note,
        from: &Note,
        strategy: EntityDedupMergePolicy,
        guard: NoteMergeGuard,
        expected: &str,
    ) -> RuntimeError {
        let before = self.snapshot().await;
        self.veto_domain_dml(true).await;
        let result = self
            .merge_with(into.id, from.id, strategy, guard, false)
            .await;
        self.veto_domain_dml(false).await;
        let error = result.expect_err("the merge must be refused");
        assert!(
            error.to_string().contains(expected),
            "expected {expected:?} in: {error}"
        );
        assert_eq!(
            self.snapshot().await,
            before,
            "a refusal must leave rows, indexes, edges and events unchanged"
        );
        error
    }

    async fn refuses(
        &self,
        into: &Note,
        from: &Note,
        guard: NoteMergeGuard,
        expected: &str,
    ) -> RuntimeError {
        self.refuses_merge(
            into,
            from,
            EntityDedupMergePolicy::PreferInto,
            guard,
            expected,
        )
        .await
    }

    /// The assertion does not hold and the refusal names it.
    async fn assertion_refuses(
        &self,
        into: &Note,
        from: &Note,
        assertion: MergeAssertion,
    ) -> RuntimeError {
        let name = assertion.name();
        let error = self
            .refuses(
                into,
                from,
                guard_for(into, from, vec![assertion]),
                &format!("assertion 0 ({name})"),
            )
            .await;
        assert_conflict(&error, "guarded note merge refused");
        error
    }

    async fn merge_event_payload(&self, survivor: Uuid) -> Value {
        let payload = self
            .scalar(
                "SELECT payload FROM events WHERE verb = 'merge' AND target_id = ?1",
                vec![SqlValue::Text(survivor.to_string())],
            )
            .await;
        match payload {
            Some(SqlValue::Text(text)) => {
                serde_json::from_str(&text).expect("event payload is JSON")
            }
            other => panic!("no merge event for {survivor}: {other:?}"),
        }
    }

    async fn vector_rows(&self, subject: Uuid) -> i64 {
        let table = format!("vec_{}", crate::config::sanitize_key(MODEL));
        match self
            .scalar(
                &format!("SELECT COUNT(*) FROM {table} WHERE subject_id = ?1"),
                vec![SqlValue::Text(subject.to_string())],
            )
            .await
        {
            Some(SqlValue::Integer(count)) => count,
            other => panic!("expected a vector row count, got {other:?}"),
        }
    }
}

fn guard_for(into: &Note, from: &Note, assertions: Vec<MergeAssertion>) -> NoteMergeGuard {
    NoteMergeGuard {
        into_version: into.version,
        from_version: from.version,
        assertions,
        survivor_properties: serde_json::Map::new(),
        annotation: None,
    }
}

fn assert_conflict(error: &RuntimeError, fragment: &str) {
    assert!(matches!(error, RuntimeError::Khive(_)), "{error:?}");
    assert!(error.to_string().contains(fragment), "{error}");
}

fn assert_invalid_input(error: &RuntimeError) {
    assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
}

fn secret_shaped_text() -> String {
    const ALPHANUMERIC: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let candidate: String = (0..48)
        .map(|index| char::from(ALPHANUMERIC[(index * 17 + 11) % ALPHANUMERIC.len()]))
        .collect();
    format!("secret value: {candidate}")
}

async fn history_of(f: &Fixture, survivor: Uuid) -> Vec<Value> {
    f.current(survivor).await.properties.expect("properties")["_merge_history"]
        .as_array()
        .expect("history is an array")
        .clone()
}

#[tokio::test]
async fn expected_versions_refuse_either_stale_note_and_leave_nothing_changed() {
    for file_backed in [false, true] {
        for stale in ["survivor", "duplicate"] {
            for dry_run in [false, true] {
                let f = Fixture::new(file_backed);
                let into = f.note("Into content").await;
                let from = f.note("From content").await;
                f.attach(from.id).await;
                let guard = guard_for(&into, &from, vec![]);
                let stale_id = if stale == "survivor" {
                    into.id
                } else {
                    from.id
                };
                f.runtime
                    .update_note_with_embedding_report(
                        &f.token,
                        stale_id,
                        NotePatch {
                            content: Some("edited after the caller read it".into()),
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("concurrent edit");
                let before = f.snapshot().await;
                f.veto_domain_dml(true).await;
                let result = f.merge(into.id, from.id, guard, dry_run).await;
                f.veto_domain_dml(false).await;
                let error = result.expect_err("a stale version must refuse");
                assert_conflict(&error, "changed concurrently after it was read");
                assert!(
                    error.to_string().contains(&stale_id.to_string()),
                    "{stale}/dry={dry_run}: {error}"
                );
                assert_eq!(
                    f.snapshot().await,
                    before,
                    "{stale}/file={file_backed}/dry={dry_run}: nothing may change"
                );
            }
        }
    }
}

#[tokio::test]
async fn matching_versions_merge_and_return_the_committed_survivor_version() {
    for file_backed in [false, true] {
        let f = Fixture::new(file_backed);
        let into = f.note("Survivor content").await;
        let first = f.note("First duplicate").await;
        let second = f.note("Second duplicate").await;
        let third = f.note("Third duplicate").await;

        let merged = f
            .merge(into.id, first.id, guard_for(&into, &first, vec![]), false)
            .await
            .expect("a matching guard merges");
        assert_eq!(merged.summary.kept_id, into.id);
        assert_eq!(merged.summary.removed_id, first.id);
        assert!(!merged.summary.dry_run);
        assert!(merged.kept_version > into.version);
        assert_eq!(f.current(into.id).await.version, merged.kept_version);
        assert!(f.tombstoned(first.id).await, "the duplicate is tombstoned");

        // The returned version chains: the next duplicate merges into the same
        // survivor without reading it again.
        let mut next = guard_for(&into, &second, vec![]);
        next.into_version = merged.kept_version;
        let again = f
            .merge(into.id, second.id, next, false)
            .await
            .expect("the returned version is the one to expect next");
        assert!(again.kept_version > merged.kept_version);
        assert_eq!(f.current(into.id).await.version, again.kept_version);

        // Control: the survivor version read before the first merge is stale now.
        let error = f
            .refuses(
                &into,
                &third,
                guard_for(&into, &third, vec![]),
                "changed concurrently",
            )
            .await;
        assert_conflict(&error, &into.id.to_string());
    }
}

#[tokio::test]
async fn a_dry_run_evaluates_the_guard_writes_nothing_and_reports_the_current_version() {
    let f = Fixture::new(false);
    let into = f.note("Into content").await;
    let from = f.note("From content").await;
    f.attach(from.id).await;
    let live = f.entity("concept", "Dry run live").await;
    let before = f.snapshot().await;

    let preview = f
        .merge(
            into.id,
            from.id,
            guard_for(
                &into,
                &from,
                vec![MergeAssertion::EntityLive { entity: live }],
            ),
            true,
        )
        .await
        .expect("dry run");
    assert!(preview.summary.dry_run);
    assert_eq!(preview.summary.edges_rewired, 1);
    assert_eq!(preview.kept_version, into.version);
    assert_eq!(f.snapshot().await, before, "a dry run writes nothing");

    // The same dry run refuses when an assertion does not hold.
    let guard = guard_for(
        &into,
        &from,
        vec![MergeAssertion::EntityLive {
            entity: Uuid::new_v4(),
        }],
    );
    let error = f
        .merge(into.id, from.id, guard, true)
        .await
        .expect_err("a dry run evaluates the assertions");
    assert_conflict(&error, "assertion 0 (EntityLive)");
    assert_eq!(f.snapshot().await, before);
}

#[tokio::test]
async fn entity_live_holds_and_refuses_after_deletion_merge_or_for_another_namespace() {
    for file_backed in [false, true] {
        let f = Fixture::new(file_backed);
        let into = f.note("Into content").await;
        let from = f.note("From content").await;
        let live = f.entity("concept", "Live entity").await;
        let doomed = f.entity("concept", "Doomed entity").await;
        let absorbed = f.entity("concept", "Absorbed entity").await;
        let foreign = f.foreign_entity("concept", "Foreign entity").await;
        f.tombstone(absorbed, live).await;

        for entity in [live, doomed] {
            f.holds(&into, &from, MergeAssertion::EntityLive { entity })
                .await;
        }
        // A change committed after the caller's read and before the merge.
        assert!(f
            .runtime
            .delete_entity(&f.token, doomed, false)
            .await
            .expect("soft delete"));
        for entity in [doomed, absorbed, foreign, Uuid::new_v4()] {
            f.assertion_refuses(&into, &from, MergeAssertion::EntityLive { entity })
                .await;
        }
    }
}

#[tokio::test]
async fn entity_lineage_reaches_follows_merge_records_to_a_live_canonical_entity() {
    for file_backed in [false, true] {
        let f = Fixture::new(file_backed);
        let into = f.note("Into content").await;
        let from = f.note("From content").await;
        let canonical = f.entity("concept", "Alpha").await;
        let middle = f.entity("concept", "Alpha b").await;
        let old = f.entity("concept", "Alpha b c").await;
        let unrelated = f.entity("concept", "Zeta").await;
        let deleted = f.entity("concept", "Eta").await;
        // old was merged into middle, and middle into canonical.
        for (kept, absorbed) in [(middle, old), (canonical, middle)] {
            f.runtime
                .merge_entity(
                    &f.token,
                    kept,
                    absorbed,
                    EntityDedupMergePolicy::PreferInto,
                    ContentMergeStrategy::Append,
                    false,
                )
                .await
                .expect("entity merge");
        }
        assert!(f
            .runtime
            .delete_entity(&f.token, deleted, false)
            .await
            .expect("soft delete"));

        for (entity, target) in [
            (old, canonical),
            (middle, canonical),
            (canonical, canonical),
        ] {
            f.holds(
                &into,
                &from,
                MergeAssertion::EntityLineageReaches {
                    entity,
                    canonical: target,
                },
            )
            .await;
        }
        for (entity, target) in [
            // The canonical entity was itself merged away, though the walk
            // passes through it.
            (old, middle),
            (middle, old),
            // A merged-away entity is not its own canonical.
            (old, old),
            // Another entity, the wrong direction, an entity that was only
            // deleted, and a record that does not exist.
            (canonical, unrelated),
            (old, unrelated),
            (unrelated, canonical),
            (deleted, canonical),
            (Uuid::new_v4(), canonical),
        ] {
            f.assertion_refuses(
                &into,
                &from,
                MergeAssertion::EntityLineageReaches {
                    entity,
                    canonical: target,
                },
            )
            .await;
        }

        // A change committed after the caller's read: the record of where `old`
        // went now points elsewhere.
        f.tombstone(old, unrelated).await;
        f.assertion_refuses(
            &into,
            &from,
            MergeAssertion::EntityLineageReaches {
                entity: old,
                canonical,
            },
        )
        .await;
        f.holds(
            &into,
            &from,
            MergeAssertion::EntityLineageReaches {
                entity: middle,
                canonical,
            },
        )
        .await;

        // And so does a canonical entity that is deleted after the read.
        assert!(f
            .runtime
            .delete_entity(&f.token, canonical, false)
            .await
            .expect("soft delete"));
        for entity in [middle, canonical] {
            f.assertion_refuses(
                &into,
                &from,
                MergeAssertion::EntityLineageReaches { entity, canonical },
            )
            .await;
        }
    }
}

#[tokio::test]
async fn entity_lineage_stops_at_cycles_other_namespaces_and_sixty_four_steps() {
    let f = Fixture::new(false);
    let into = f.note("Into content").await;
    let from = f.note("From content").await;
    let unrelated = f.entity("concept", "Unrelated entity").await;

    // chain[i] was merged into chain[i + 1]; the last one is live. The bound is
    // written out, not read from the constant, so a changed bound fails here.
    let mut chain = Vec::new();
    for index in 0..=65 {
        chain.push(f.entity("concept", &format!("Chain entity {index}")).await);
    }
    for pair in chain.windows(2) {
        f.tombstone(pair[0], pair[1]).await;
    }
    let last = chain[chain.len() - 1];
    // Exactly 64 records to follow.
    f.holds(
        &into,
        &from,
        MergeAssertion::EntityLineageReaches {
            entity: chain[1],
            canonical: last,
        },
    )
    .await;
    // 65 records to follow.
    f.assertion_refuses(
        &into,
        &from,
        MergeAssertion::EntityLineageReaches {
            entity: chain[0],
            canonical: last,
        },
    )
    .await;

    // A cycle of merge records never reaches a canonical entity outside it.
    let a = f.entity("concept", "Cycle entity a").await;
    let b = f.entity("concept", "Cycle entity b").await;
    f.tombstone(a, b).await;
    f.tombstone(b, a).await;
    f.assertion_refuses(
        &into,
        &from,
        MergeAssertion::EntityLineageReaches {
            entity: a,
            canonical: unrelated,
        },
    )
    .await;

    // Lineage is read in the caller's namespace only: an entity of another
    // namespace that records a merge into the caller's canonical entity does
    // not reach it, and neither does a lineage wholly in the other namespace.
    let foreign_old = f.foreign_entity("concept", "Foreign old").await;
    let foreign_canonical = f.foreign_entity("concept", "Foreign canonical").await;
    f.tombstone(foreign_old, unrelated).await;
    f.assertion_refuses(
        &into,
        &from,
        MergeAssertion::EntityLineageReaches {
            entity: foreign_old,
            canonical: unrelated,
        },
    )
    .await;
    f.tombstone(foreign_old, foreign_canonical).await;
    f.assertion_refuses(
        &into,
        &from,
        MergeAssertion::EntityLineageReaches {
            entity: foreign_old,
            canonical: foreign_canonical,
        },
    )
    .await;
}

#[tokio::test]
async fn note_edge_to_requires_a_live_edge_from_the_note_in_the_namespace() {
    for file_backed in [false, true] {
        let f = Fixture::new(file_backed);
        let into = f.note("Into content").await;
        let from = f.note("From content").await;
        let namespace = f.namespace();
        let target = f.entity("concept", "Edge target").await;
        let edge = f.link(into.id, target, EdgeRelation::Annotates).await;
        let assertion = MergeAssertion::NoteEdgeTo {
            note: into.id,
            relation: EdgeRelation::Annotates,
            target,
        };
        f.holds(&into, &from, assertion.clone()).await;

        // Control: a row the helper writes into the caller's namespace holds, so
        // the refusals below are the assertion and not the helper.
        let stored = f.entity("concept", "Stored target").await;
        f.insert_edge(&namespace, into.id, stored, EdgeRelation::Annotates, true)
            .await;
        f.holds(
            &into,
            &from,
            MergeAssertion::NoteEdgeTo {
                note: into.id,
                relation: EdgeRelation::Annotates,
                target: stored,
            },
        )
        .await;

        // Another relation, another target, the reverse direction, an edge row
        // of another namespace, a deleted edge and an edge to another store are
        // not the edge asked for.
        let elsewhere = f.entity("concept", "Edge row elsewhere").await;
        f.insert_edge(
            OTHER_NAMESPACE,
            into.id,
            elsewhere,
            EdgeRelation::Annotates,
            true,
        )
        .await;
        let reversed = f.entity("concept", "Reversed target").await;
        f.insert_edge(&namespace, reversed, into.id, EdgeRelation::Extends, true)
            .await;
        let deleted_edge = f.entity("concept", "Deleted edge target").await;
        f.insert_edge(
            &namespace,
            into.id,
            deleted_edge,
            EdgeRelation::Annotates,
            false,
        )
        .await;
        let remote = f.entity("concept", "Remote edge target").await;
        f.insert_edge(&namespace, into.id, remote, EdgeRelation::Annotates, true)
            .await;
        f.move_edge_to_other_store(into.id, remote).await;
        for (relation, wanted) in [
            (EdgeRelation::Supports, target),
            (EdgeRelation::Annotates, Uuid::new_v4()),
            (EdgeRelation::Annotates, elsewhere),
            (EdgeRelation::Extends, reversed),
            (EdgeRelation::Annotates, deleted_edge),
            (EdgeRelation::Annotates, remote),
        ] {
            f.assertion_refuses(
                &into,
                &from,
                MergeAssertion::NoteEdgeTo {
                    note: into.id,
                    relation,
                    target: wanted,
                },
            )
            .await;
        }

        // A change committed after the caller's read: the edge is deleted.
        assert!(f
            .runtime
            .delete_edge(&f.token, edge, false)
            .await
            .expect("delete edge"));
        f.assertion_refuses(&into, &from, assertion).await;
    }
}

#[tokio::test]
async fn note_edge_targets_within_refuses_a_second_live_target_of_the_kind() {
    for file_backed in [false, true] {
        let f = Fixture::new(file_backed);
        let into = f.note("Into content").await;
        let from = f.note("From content").await;
        let namespace = f.namespace();
        let allowed = f.entity("project", "Allowed project").await;
        f.link(into.id, allowed, EdgeRelation::Annotates).await;

        // Edges that do not count: another kind, a note, a deleted entity, a
        // target in another store, another namespace's entity of the kind, and
        // an id with no record.
        let concept = f.entity("concept", "Not a project").await;
        f.link(into.id, concept, EdgeRelation::Annotates).await;
        let other_note = f.note("A note target").await;
        f.link(into.id, other_note.id, EdgeRelation::Annotates)
            .await;
        let retired = f.entity("project", "Retired project").await;
        assert!(f
            .runtime
            .delete_entity(&f.token, retired, false)
            .await
            .expect("soft delete"));
        f.insert_edge(&namespace, into.id, retired, EdgeRelation::Annotates, true)
            .await;
        let remote = f.entity("project", "Remote project").await;
        f.insert_edge(&namespace, into.id, remote, EdgeRelation::Annotates, true)
            .await;
        f.move_edge_to_other_store(into.id, remote).await;
        let foreign = f.foreign_entity("project", "Foreign project").await;
        f.insert_edge(&namespace, into.id, foreign, EdgeRelation::Annotates, true)
            .await;
        f.insert_edge(
            &namespace,
            into.id,
            Uuid::new_v4(),
            EdgeRelation::Annotates,
            true,
        )
        .await;
        // A relation the assertion does not name never counts.
        let supported = f.entity("project", "Other relation project").await;
        f.insert_edge(&namespace, into.id, supported, EdgeRelation::Extends, true)
            .await;
        let assertion = MergeAssertion::NoteEdgeTargetsWithin {
            note: into.id,
            relation: EdgeRelation::Annotates,
            target_kind: "project".into(),
            allowed,
        };
        f.holds(&into, &from, assertion.clone()).await;

        // A change committed after the caller's read: another live project.
        let intruder = f.entity("project", "Intruder project").await;
        let intruder_edge = f.link(into.id, intruder, EdgeRelation::Annotates).await;
        f.assertion_refuses(&into, &from, assertion.clone()).await;

        // Control: with that edge gone the same assertion holds again.
        assert!(f
            .runtime
            .delete_edge(&f.token, intruder_edge, false)
            .await
            .expect("delete edge"));
        f.holds(&into, &from, assertion.clone()).await;

        // Only edge rows stored under the caller's namespace are read: a row of
        // another namespace from this note to a live project that is not
        // `allowed` is ignored, so the assertion still holds. Control: the same
        // row in the caller's namespace to another live project refuses.
        f.insert_edge(
            OTHER_NAMESPACE,
            into.id,
            intruder,
            EdgeRelation::Annotates,
            true,
        )
        .await;
        f.holds(&into, &from, assertion.clone()).await;
        let local_intruder = f.entity("project", "Local intruder project").await;
        f.insert_edge(
            &namespace,
            into.id,
            local_intruder,
            EdgeRelation::Annotates,
            true,
        )
        .await;
        f.assertion_refuses(&into, &from, assertion).await;
    }
}

#[tokio::test]
async fn note_edge_targets_within_refuses_an_allowed_entity_that_is_not_live_of_the_kind() {
    let f = Fixture::new(false);
    let into = f.note("Into content").await;
    let from = f.note("From content").await;
    let live = f.entity("project", "Live allowed project").await;
    f.link(into.id, live, EdgeRelation::Annotates).await;
    let holds = |allowed: Uuid| MergeAssertion::NoteEdgeTargetsWithin {
        note: into.id,
        relation: EdgeRelation::Annotates,
        target_kind: "project".into(),
        allowed,
    };
    f.holds(&into, &from, holds(live)).await;

    let deleted = f.entity("project", "Deleted allowed project").await;
    assert!(f
        .runtime
        .delete_entity(&f.token, deleted, false)
        .await
        .expect("soft delete"));
    let merged_away = f.entity("project", "Merged away allowed project").await;
    f.tombstone(merged_away, live).await;
    let other_kind = f.entity("concept", "Allowed of another kind").await;
    for allowed in [deleted, merged_away, other_kind, Uuid::new_v4()] {
        f.assertion_refuses(&into, &from, holds(allowed)).await;
    }

    // `into` also has a counted edge to `live`, which refuses every case above
    // on its own. `from` has no edge, so only the allowed entity can refuse here.
    let bare = |allowed: Uuid| MergeAssertion::NoteEdgeTargetsWithin {
        note: from.id,
        relation: EdgeRelation::Annotates,
        target_kind: "project".into(),
        allowed,
    };
    f.holds(&into, &from, bare(live)).await;
    for allowed in [deleted, merged_away, other_kind, Uuid::new_v4()] {
        f.assertion_refuses(&into, &from, bare(allowed)).await;
    }
}

#[tokio::test]
async fn note_edge_targets_within_says_nothing_about_records_of_another_namespace() {
    let f = Fixture::new(false);
    let namespace = f.namespace();
    let allowed = f.entity("project", "Allowed project").await;
    let foreign = f.foreign_entity("project", "Foreign project").await;
    let assertion = |note: Uuid, allowed: Uuid| MergeAssertion::NoteEdgeTargetsWithin {
        note,
        relation: EdgeRelation::Annotates,
        target_kind: "project".into(),
        allowed,
    };

    // An edge to an entity of the kind in another namespace and an edge to an id
    // with no record give the same outcome: neither counts.
    let mut outcomes = Vec::new();
    for target in [foreign, Uuid::new_v4()] {
        let into = f.note("Into content").await;
        let from = f.note("From content").await;
        f.link(into.id, allowed, EdgeRelation::Annotates).await;
        f.insert_edge(&namespace, into.id, target, EdgeRelation::Annotates, true)
            .await;
        let preview = f
            .merge(
                into.id,
                from.id,
                guard_for(&into, &from, vec![assertion(into.id, allowed)]),
                true,
            )
            .await;
        outcomes.push(preview.is_ok());
    }
    assert_eq!(outcomes, vec![true, true]);

    // Control: an entity of the kind in the caller's namespace does count, so the
    // two outcomes above were not a missing check.
    let into = f.note("Into content").await;
    let from = f.note("From content").await;
    f.link(into.id, allowed, EdgeRelation::Annotates).await;
    let local = f.entity("project", "Local second project").await;
    f.link(into.id, local, EdgeRelation::Annotates).await;
    f.assertion_refuses(&into, &from, assertion(into.id, allowed))
        .await;

    // An `allowed` entity of another namespace refuses without naming the entity
    // or its namespace, so a refusal discloses nothing about that namespace.
    let into = f.note("Into content").await;
    let from = f.note("From content").await;
    let error = f
        .assertion_refuses(&into, &from, assertion(into.id, foreign))
        .await;
    for text in [error.to_string(), format!("{error:?}")] {
        assert!(!text.contains(&foreign.to_string()), "{text}");
        assert!(!text.contains(OTHER_NAMESPACE), "{text}");
    }
}

#[tokio::test]
async fn assertions_are_evaluated_in_order_and_the_first_failure_is_named() {
    let f = Fixture::new(false);
    let into = f.note("Into content").await;
    let from = f.note("From content").await;
    let live = f.entity("concept", "Live entity").await;
    let missing = Uuid::new_v4();
    let guard = guard_for(
        &into,
        &from,
        vec![
            MergeAssertion::EntityLive { entity: live },
            MergeAssertion::EntityLive { entity: missing },
            MergeAssertion::EntityLineageReaches {
                entity: missing,
                canonical: live,
            },
        ],
    );
    let error = f
        .refuses(&into, &from, guard, "assertion 1 (EntityLive)")
        .await;
    assert_conflict(&error, "guarded note merge refused");
    assert!(!error.to_string().contains("assertion 2"), "{error}");
}

#[tokio::test]
async fn survivor_properties_replace_the_survivors_value_before_provenance_is_appended() {
    for file_backed in [false, true] {
        let f = Fixture::new(file_backed);
        let into = f
            .note_of(
                "observation",
                "Into content",
                Some(json!({"keep": "survivor", "contested": "survivor value", "gone": 1})),
            )
            .await;
        let from = f
            .note_of(
                "observation",
                "From content",
                Some(json!({"contested": "duplicate value"})),
            )
            .await;
        let mut guard = guard_for(&into, &from, vec![]);
        guard
            .survivor_properties
            .insert("contested".into(), json!("planned value"));
        guard
            .survivor_properties
            .insert("planned_only".into(), json!({"nested": [1, 2]}));
        // A null is stored as null, and no override removes a key.
        guard.survivor_properties.insert("gone".into(), Value::Null);
        let merged = f
            .merge_with(
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferFrom,
                guard,
                false,
            )
            .await
            .expect("override merges");
        let properties = f.current(into.id).await.properties.expect("properties");
        assert_eq!(properties["contested"], "planned value");
        assert_eq!(properties["planned_only"], json!({"nested": [1, 2]}));
        assert_eq!(properties["keep"], "survivor");
        let object = properties.as_object().expect("object");
        assert_eq!(object.get("gone"), Some(&Value::Null));
        let history = history_of(&f, into.id).await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0]["merged_from"], from.id.to_string());
        assert!(history[0].get("annotation").is_none(), "{history:?}");
        // The override is not a contribution of the merged-away note.
        assert_eq!(merged.summary.properties_merged, 0);
    }
}

#[tokio::test]
async fn an_override_creates_the_properties_object_on_two_notes_without_properties() {
    let f = Fixture::new(false);
    let into = f.note("Into content").await;
    let from = f.note("From content").await;
    // Stored as SQL NULL whatever the note writer normalizes to.
    for note in [&into, &from] {
        f.execute(
            "UPDATE notes SET properties = NULL WHERE id = ?1",
            vec![SqlValue::Text(note.id.to_string())],
        )
        .await;
    }
    let into = f.current(into.id).await;
    let from = f.current(from.id).await;
    assert!(into.properties.is_none() && from.properties.is_none());
    let mut guard = guard_for(&into, &from, vec![]);
    guard
        .survivor_properties
        .insert("project_id".into(), json!("planned"));
    f.merge(into.id, from.id, guard, false)
        .await
        .expect("override merges");
    let properties = f.current(into.id).await.properties.expect("properties");
    assert_eq!(properties["project_id"], "planned");
    assert_eq!(properties["_merge_history"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn survivor_property_overrides_refuse_reserved_keys_and_non_object_properties() {
    let f = Fixture::new(false);
    let into = f.note("Into content").await;
    let from = f.note("From content").await;
    let message_into = f.note("Into message").await;
    let message_from = f.note("From message").await;
    f.set_kind("message", &[&message_into, &message_from]).await;
    let message_into = f.current(message_into.id).await;
    let message_from = f.current(message_from.id).await;

    // The merge owns its provenance, and the runtime owns its reserved keys.
    for (key, expected) in [
        ("_merge_history", "cannot override `_merge_history`"),
        (
            crate::secret_gate::RESERVED_SECRET_GATE_KEY,
            "runtime-owned",
        ),
        (
            crate::secret_gate::RESERVED_WEB_RECEIPT_KEY,
            "web-pack-owned",
        ),
    ] {
        let mut guard = guard_for(&into, &from, vec![]);
        guard.survivor_properties.insert(key.into(), json!(1));
        let error = f.refuses(&into, &from, guard, expected).await;
        assert_invalid_input(&error);
    }

    // A key the note kind owns.
    let mut guard = guard_for(&message_into, &message_from, vec![]);
    guard
        .survivor_properties
        .insert("channel_kind".into(), json!("elsewhere"));
    let error = f
        .refuses(&message_into, &message_from, guard, "`channel_kind`")
        .await;
    assert_invalid_input(&error);

    // Control: the same key is ordinary metadata on a kind that does not own it.
    let mut guard = guard_for(&into, &from, vec![]);
    guard
        .survivor_properties
        .insert("channel_kind".into(), json!("ordinary"));
    f.merge(into.id, from.id, guard, true)
        .await
        .expect("an unowned key overrides");

    // Merged properties that are present and not an object.
    let scalar_into = f
        .note_of("observation", "Scalar into", Some(json!({"keep": 1})))
        .await;
    let scalar_from = f.note("Scalar from").await;
    f.set_properties(scalar_from.id, "\"scalar\"").await;
    let scalar_from = f.current(scalar_from.id).await;
    let mut guard = guard_for(&scalar_into, &scalar_from, vec![]);
    guard
        .survivor_properties
        .insert("project_id".into(), json!("planned"));
    let error = f
        .refuses_merge(
            &scalar_into,
            &scalar_from,
            EntityDedupMergePolicy::PreferFrom,
            guard,
            "the merged properties are not an object",
        )
        .await;
    assert_conflict(&error, "guarded note merge refused");

    // Control: with no override the same merge goes ahead.
    f.merge_with(
        scalar_into.id,
        scalar_from.id,
        EntityDedupMergePolicy::PreferFrom,
        guard_for(&scalar_into, &scalar_from, vec![]),
        false,
    )
    .await
    .expect("no override, no refusal");
}

#[tokio::test]
async fn a_key_owned_by_the_notes_pack_cannot_be_overridden_but_others_can() {
    let f = Fixture::new(false);
    let bare_into = f.note("Bare into").await;
    let bare_from = f.note("Bare from").await;
    let into = f.note("Into content").await;
    let from = f.note("From content").await;

    // Control: a bare runtime has no pack-owned kinds, so the key is ordinary.
    let mut guard = guard_for(&bare_into, &bare_from, vec![]);
    guard
        .survivor_properties
        .insert("from_actor".into(), json!("ordinary metadata"));
    f.merge(bare_into.id, bare_from.id, guard, false)
        .await
        .expect("ordinary key on a kind no pack owns");

    f.runtime
        .install_pack_owned_note_kinds(vec!["observation".into()]);
    let mut guard = guard_for(&into, &from, vec![]);
    guard
        .survivor_properties
        .insert("from_actor".into(), json!("someone else"));
    let error = f.refuses(&into, &from, guard, "`from_actor`").await;
    assert_invalid_input(&error);

    let mut guard = guard_for(&into, &from, vec![]);
    guard
        .survivor_properties
        .insert("project_id".into(), json!("planned"));
    f.merge(into.id, from.id, guard, false)
        .await
        .expect("an unreserved key still overrides");
}

#[tokio::test]
async fn a_credential_shaped_override_or_annotation_is_refused_before_the_transaction() {
    let f = Fixture::new(false);
    let into = f.note("Into content").await;
    let from = f.note("From content").await;

    // Both carry a version that no longer matches, which the transaction would
    // refuse as a conflict. The credential check answers first, so it ran before
    // the transaction read either note.
    let mut in_override = guard_for(&into, &from, vec![]);
    in_override.into_version += 100;
    in_override
        .survivor_properties
        .insert("note".into(), json!(secret_shaped_text()));
    let mut in_key = guard_for(&into, &from, vec![]);
    in_key.into_version += 100;
    in_key
        .survivor_properties
        .insert(secret_shaped_text(), json!(1));
    let mut in_annotation = guard_for(&into, &from, vec![]);
    in_annotation.into_version += 100;
    in_annotation.annotation = Some(json!({"nested": [secret_shaped_text()]}));
    for guard in [in_override, in_key, in_annotation] {
        let error = f.refuses(&into, &from, guard, "").await;
        assert!(
            matches!(error, RuntimeError::SecretDetected(_)),
            "{error:?}"
        );
    }

    // The same malformed guard values refuse ahead of a stale version too.
    let mut reserved = guard_for(&into, &from, vec![]);
    reserved.into_version += 100;
    reserved
        .survivor_properties
        .insert("_merge_history".into(), json!([]));
    let error = f
        .refuses(&into, &from, reserved, "cannot override `_merge_history`")
        .await;
    assert_invalid_input(&error);

    let mut reserved_key = guard_for(&into, &from, vec![]);
    reserved_key.into_version += 100;
    reserved_key.survivor_properties.insert(
        crate::secret_gate::RESERVED_SECRET_GATE_KEY.into(),
        json!(1),
    );
    let error = f.refuses(&into, &from, reserved_key, "runtime-owned").await;
    assert_invalid_input(&error);

    // Control: with the version restored the credential-free guard merges.
    f.merge(into.id, from.id, guard_for(&into, &from, vec![]), false)
        .await
        .expect("a clean guard merges");
}

#[tokio::test]
async fn an_annotation_is_stored_verbatim_in_the_new_history_entry() {
    for file_backed in [false, true] {
        let f = Fixture::new(file_backed);
        let into = f.note("Into content").await;
        let from = f.note("From content").await;
        let annotation = json!({
            "reason": "same record",
            "evidence": {"count": 3, "ids": ["a", "b"], "ratio": 0.5},
            "nothing": null,
        });
        let mut guard = guard_for(&into, &from, vec![]);
        guard.annotation = Some(annotation.clone());
        f.merge(into.id, from.id, guard, false)
            .await
            .expect("annotated merge");
        let history = history_of(&f, into.id).await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0]["annotation"], annotation);
        assert_eq!(history[0]["merged_from"], from.id.to_string());
        assert_eq!(history[0]["strategy"], "PreferInto");
    }
}

#[tokio::test]
async fn an_annotation_over_four_kib_is_refused_and_exactly_four_kib_is_accepted() {
    let f = Fixture::new(false);
    let into = f.note("Into content").await;
    let from = f.note("From content").await;
    let text = |length: usize| -> String { "lorem ipsum ".chars().cycle().take(length).collect() };

    // Serialized, a string is its length plus two quotes.
    let mut over = guard_for(&into, &from, vec![]);
    over.annotation = Some(Value::String(text(4095)));
    let error = f.refuses(&into, &from, over, "4097 bytes").await;
    assert_invalid_input(&error);

    let mut at_limit = guard_for(&into, &from, vec![]);
    at_limit.annotation = Some(Value::String(text(4094)));
    f.merge(into.id, from.id, at_limit, false)
        .await
        .expect("an annotation of exactly 4 KiB is accepted");
    let history = history_of(&f, into.id).await;
    assert_eq!(history[0]["annotation"], Value::String(text(4094)));
}

#[tokio::test]
async fn a_merge_history_that_is_not_an_array_refuses_a_guarded_merge() {
    for file_backed in [false, true] {
        for (label, strategy, history_on_into) in [
            (
                "survivor's own history",
                EntityDedupMergePolicy::PreferInto,
                true,
            ),
            (
                "absorbed note's history",
                EntityDedupMergePolicy::PreferFrom,
                false,
            ),
        ] {
            let f = Fixture::new(file_backed);
            let into = f
                .note_of("observation", "Into content", Some(json!({"keep": 1})))
                .await;
            let from = f.note("From content").await;
            let malformed = if history_on_into { into.id } else { from.id };
            f.set_properties(
                malformed,
                &json!({"keep": 1, "_merge_history": "legacy malformed field"}).to_string(),
            )
            .await;
            let into = f.current(into.id).await;
            let from = f.current(from.id).await;
            let mut guard = guard_for(&into, &from, vec![]);
            guard.annotation = Some(json!({"reason": "kept"}));
            let error = f
                .refuses_merge(
                    &into,
                    &from,
                    strategy,
                    guard,
                    "`_merge_history` is not an array",
                )
                .await;
            assert_conflict(&error, "guarded note merge refused");

            // Control: the unguarded merge of the same pair goes ahead and drops
            // its own provenance entry, which is what the guard refuses to do.
            f.runtime
                .merge_note_with_reason(
                    &f.token,
                    into.id,
                    from.id,
                    strategy,
                    ContentMergeStrategy::Append,
                    false,
                    None,
                )
                .await
                .unwrap_or_else(|error| panic!("{label}: the unguarded merge changed: {error}"));
            let properties = f.current(into.id).await.properties.expect("properties");
            assert_eq!(
                properties["_merge_history"], "legacy malformed field",
                "{label}"
            );
        }
    }
}

#[tokio::test]
async fn a_schedule_managed_note_is_refused_by_the_guarded_merge_as_by_the_unguarded_one() {
    let f = Fixture::new(false);
    let into = f.note("Into content").await;
    let from = f.note("From content").await;
    f.set_kind("scheduled_event", &[&into, &from]).await;
    let into = f.current(into.id).await;
    let from = f.current(from.id).await;

    let unguarded = f
        .runtime
        .merge_note_with_reason(
            &f.token,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .expect_err("the unguarded merge refuses a schedule-managed note");
    assert_invalid_input(&unguarded);
    let guarded = f
        .refuses(
            &into,
            &from,
            guard_for(&into, &from, vec![]),
            "schedule-managed",
        )
        .await;
    assert_invalid_input(&guarded);
    assert_eq!(guarded.to_string(), unguarded.to_string());
}

struct ConstProvider;
struct ConstService;

#[async_trait::async_trait]
impl crate::embedder_registry::EmbedderProvider for ConstProvider {
    fn name(&self) -> &str {
        MODEL
    }

    fn dimensions(&self) -> usize {
        4
    }

    async fn build(
        &self,
    ) -> crate::error::RuntimeResult<std::sync::Arc<dyn lattice_embed::EmbeddingService>> {
        Ok(std::sync::Arc::new(ConstService))
    }
}

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for ConstService {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        Ok(texts.iter().map(|_| vec![1.0_f32; 4]).collect())
    }

    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "guarded-merge-const-service"
    }
}

#[tokio::test]
async fn a_refusal_leaves_vector_rows_alone_and_a_merge_removes_only_the_duplicates() {
    let f = Fixture::new(false);
    let into = f.note("Into content").await;
    let from = f.note("From content").await;
    f.runtime.register_embedder(ConstProvider);
    let vectors = f
        .runtime
        .vectors_for_model(&f.token, MODEL)
        .expect("vector store");
    for note in [&into, &from] {
        vectors
            .insert_batch(vec![VectorRecord {
                subject_id: note.id,
                kind: SubstrateKind::Note,
                namespace: note.namespace.clone(),
                field: "note.content".into(),
                embedding_model: Some(MODEL.into()),
                vectors: vec![vec![1.0; 4]],
                text_fingerprint: Some(VectorRecord::fingerprint_text("seeded input")),
                updated_at: chrono::Utc::now(),
            }])
            .await
            .expect("seed vector");
    }
    let into_rows = f.vector_rows(into.id).await;
    let from_rows = f.vector_rows(from.id).await;
    assert!(into_rows > 0 && from_rows > 0, "both notes are indexed");

    // A refusal (an assertion that does not hold) leaves every vector row.
    f.assertion_refuses(
        &into,
        &from,
        MergeAssertion::EntityLive {
            entity: Uuid::new_v4(),
        },
    )
    .await;
    assert_eq!(f.vector_rows(into.id).await, into_rows);
    assert_eq!(f.vector_rows(from.id).await, from_rows);

    let merged = f
        .merge(into.id, from.id, guard_for(&into, &from, vec![]), false)
        .await
        .expect("a matching guard merges");
    assert_eq!(
        f.vector_rows(from.id).await,
        0,
        "the duplicate is unindexed"
    );
    // The post-commit reindex does not move the version the caller was handed.
    assert_eq!(f.current(into.id).await.version, merged.kept_version);
}

async fn twin_pair(f: &Fixture) -> (Note, Note) {
    let into = f
        .note_of(
            "observation",
            "into body",
            Some(json!({"own": 1, "shared": "into"})),
        )
        .await;
    let from = f
        .note_of(
            "observation",
            "from body",
            Some(json!({"extra": 2, "shared": "from"})),
        )
        .await;
    f.attach(from.id).await;
    (into, from)
}

#[tokio::test]
async fn a_guard_that_adds_nothing_merges_exactly_like_the_unguarded_merge() {
    let f = Fixture::new(false);
    let (plain_into, plain_from) = twin_pair(&f).await;
    let (guarded_into, guarded_from) = twin_pair(&f).await;

    let plain = f
        .runtime
        .merge_note_with_reason(
            &f.token,
            plain_into.id,
            plain_from.id,
            EntityDedupMergePolicy::Union,
            ContentMergeStrategy::Append,
            false,
            Some("same reason".into()),
        )
        .await
        .expect("unguarded merge");
    let guarded = f
        .runtime
        .merge_note_guarded(
            &f.token,
            guarded_into.id,
            guarded_from.id,
            EntityDedupMergePolicy::Union,
            ContentMergeStrategy::Append,
            false,
            Some("same reason".into()),
            guard_for(&guarded_into, &guarded_from, vec![]),
        )
        .await
        .expect("guarded merge");

    let numbers = |summary: &MergeSummary| {
        (
            summary.edges_rewired,
            summary.edges_self_loop_dropped,
            summary.edges_contract_skipped,
            summary.edge_conflict_preimages.len(),
            summary.properties_merged,
            summary.tags_unioned,
            summary.content_appended,
            summary.tx_budget.rows_charged,
            summary.tx_budget.bytes_charged,
        )
    };
    assert_eq!(numbers(&plain), numbers(&guarded.summary));

    let comparable = |note: &Note| {
        let mut properties = note.properties.clone().expect("properties");
        let history = properties
            .as_object_mut()
            .expect("object")
            .remove("_merge_history")
            .expect("history");
        let entries: Vec<Vec<String>> = history
            .as_array()
            .expect("array")
            .iter()
            .map(|entry| {
                let mut keys: Vec<String> = entry
                    .as_object()
                    .expect("entry object")
                    .keys()
                    .cloned()
                    .collect();
                keys.sort();
                keys
            })
            .collect();
        json!({
            "content": note.content,
            "salience": note.salience,
            "status": note.status,
            "properties": properties,
            "history_keys": entries,
        })
    };
    assert_eq!(
        comparable(&f.current(plain_into.id).await),
        comparable(&f.current(guarded_into.id).await)
    );

    // Both merges wrote the same event shape, reason included.
    let keys = |payload: &Value| -> Vec<String> {
        let mut keys: Vec<String> = payload
            .as_object()
            .expect("payload object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    };
    let plain_event = f.merge_event_payload(plain_into.id).await;
    let guarded_event = f.merge_event_payload(guarded_into.id).await;
    assert_eq!(keys(&plain_event), keys(&guarded_event));
    assert_eq!(guarded_event["reason"], "same reason");
}

async fn merge_within_limits(
    f: &Fixture,
    into: Uuid,
    from: Uuid,
    limits: MergeTxLimits,
    guard: Option<NoteMergeGuard>,
) -> Result<(MergeSummary, Note), SqliteError> {
    let pool = f.runtime.backend().pool_arc();
    tokio::task::spawn_blocking(move || {
        let writer = pool.writer().expect("fixture writer");
        writer.transaction(|conn| {
            merge_note_sql(
                conn,
                "local".to_string(),
                "fts_notes".to_string(),
                Vec::new(),
                into,
                from,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                Vec::new(),
                false,
                limits,
                None,
                guard,
            )
            .map_err(|error| match error {
                MergeSqlError::Sqlite(error) => error,
                MergeSqlError::Refusal(error) => SqliteError::InvalidData(format!(
                    "unexpected transactional policy refusal: {error}"
                )),
            })
        })
    })
    .await
    .expect("merge task")
}

#[tokio::test]
async fn assertion_evaluation_charges_the_merge_transaction_budget() {
    let f = Fixture::new(false);
    let project = f.entity("project", "Budget project").await;
    let other = f.entity("project", "Other budget project").await;
    let third = f.note("Third note").await;
    f.link(third.id, project, EdgeRelation::Annotates).await;
    f.link(third.id, other, EdgeRelation::Annotates).await;
    let bare = f.note("Note without edges").await;
    // (assertion, whether it fits when only one row beyond the two note reads is
    // allowed). Each of them charges at least one row.
    let cases = [
        (MergeAssertion::EntityLive { entity: project }, true),
        (
            MergeAssertion::EntityLineageReaches {
                entity: other,
                canonical: project,
            },
            false,
        ),
        (
            MergeAssertion::NoteEdgeTo {
                note: third.id,
                relation: EdgeRelation::Annotates,
                target: project,
            },
            true,
        ),
        (
            MergeAssertion::NoteEdgeTargetsWithin {
                note: third.id,
                relation: EdgeRelation::Annotates,
                target_kind: "project".into(),
                allowed: project,
            },
            false,
        ),
        // No edge to walk: reading the allowed entity is the only charge.
        (
            MergeAssertion::NoteEdgeTargetsWithin {
                note: bare.id,
                relation: EdgeRelation::Annotates,
                target_kind: "project".into(),
                allowed: project,
            },
            true,
        ),
    ];
    for (assertion, fits_one_more_row) in cases {
        let name = assertion.name();
        // The two note reads fill a two-row budget exactly: the unguarded merge
        // fits, and every assertion's first read exceeds it.
        // With one more row, only the assertions that read a single row fit.
        for (max_rows, expect_fit) in [(2, false), (3, fits_one_more_row)] {
            let limits = MergeTxLimits {
                max_rows,
                max_bytes: usize::MAX,
            };
            let into = f.note("Into content").await;
            let from = f.note("From content").await;
            merge_within_limits(&f, into.id, from.id, limits, None)
                .await
                .unwrap_or_else(|error| panic!("{name}/{max_rows}: unguarded merge fits: {error}"));

            let into = f.note("Into content").await;
            let from = f.note("From content").await;
            let result = merge_within_limits(
                &f,
                into.id,
                from.id,
                limits,
                Some(guard_for(&into, &from, vec![assertion.clone()])),
            )
            .await;
            if expect_fit {
                result.unwrap_or_else(|error| panic!("{name}/{max_rows}: should fit: {error}"));
            } else {
                let message = result
                    .expect_err("the assertion's reads are charged")
                    .to_string();
                assert!(
                    message.contains("merge transaction budget exceeded while"),
                    "{name}/{max_rows}: {message}"
                );
                assert!(
                    message.contains("assertion"),
                    "{name}/{max_rows}: {message}"
                );
                assert!(
                    !f.tombstoned(from.id).await,
                    "{name}/{max_rows}: a budget refusal leaves the duplicate in place"
                );
            }
        }
    }
}
