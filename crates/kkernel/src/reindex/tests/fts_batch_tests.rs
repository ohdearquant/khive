use super::*;
use async_trait::async_trait;
use khive_storage::error::WriterTaskRequestState;
use khive_storage::types::{
    BatchWriteErrorClass, BatchWriteRetryability, BatchWriteSummary, IndexRebuildScope,
    StorageResult, TextFilter, TextIndexStats, TextSearchHit, TextSearchRequest,
    MAX_BATCH_WRITE_ERROR_DETAILS,
};
use std::sync::Mutex;

struct BatchStore {
    outcome: Mutex<Option<StorageResult<BatchWriteSummary>>>,
    submitted: Mutex<Vec<Vec<TextDocument>>>,
}

impl BatchStore {
    fn new(outcome: StorageResult<BatchWriteSummary>) -> Self {
        Self {
            outcome: Mutex::new(Some(outcome)),
            submitted: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl TextSearch for BatchStore {
    async fn upsert_document(&self, _: TextDocument) -> StorageResult<()> {
        panic!("backfill must not use or replay singleton writes");
    }

    async fn upsert_documents(
        &self,
        documents: Vec<TextDocument>,
    ) -> StorageResult<BatchWriteSummary> {
        self.submitted.lock().unwrap().push(documents);
        self.outcome.lock().unwrap().take().expect("one batch call")
    }

    async fn delete_document(&self, _: &str, _: Uuid) -> StorageResult<bool> {
        panic!("unexpected delete");
    }

    async fn get_document(&self, _: &str, _: Uuid) -> StorageResult<Option<TextDocument>> {
        panic!("unexpected read");
    }

    async fn search(&self, _: TextSearchRequest) -> StorageResult<Vec<TextSearchHit>> {
        panic!("unexpected search");
    }

    async fn count(&self, _: TextFilter) -> StorageResult<u64> {
        panic!("unexpected count");
    }

    async fn stats(&self) -> StorageResult<TextIndexStats> {
        panic!("unexpected stats");
    }

    async fn rebuild(&self, _: IndexRebuildScope) -> StorageResult<TextIndexStats> {
        panic!("unexpected rebuild");
    }
}

fn documents(count: usize) -> Vec<TextDocument> {
    (0..count)
        .map(|i| note_fts_document(&Note::new("local", "memory", format!("body {i}"))))
        .collect()
}

#[tokio::test]
async fn batch_backfill_counts_failures_beyond_the_diagnostic_cap() {
    let docs = documents(MAX_BATCH_WRITE_ERROR_DETAILS + 3);
    let failed = docs.len() as u64 - 1;
    let mut summary = BatchWriteSummary {
        attempted: docs.len() as u64,
        affected: 1,
        ..Default::default()
    };
    for (index, doc) in docs.iter().enumerate().skip(1) {
        summary.record_failure(
            index,
            Some(doc.subject_id.to_string()),
            BatchWriteErrorClass::Constraint,
            BatchWriteRetryability::Permanent,
            "refused document",
        );
    }
    assert_eq!(summary.errors.len(), MAX_BATCH_WRITE_ERROR_DETAILS);
    assert!(summary.errors_truncated);
    assert_eq!(summary.errors_omitted, 2);
    let store = BatchStore::new(Ok(summary));
    let expected = serde_json::to_value(&docs).unwrap();
    assert_eq!(
        fts_backfill_documents(&store, docs, SubstrateKind::Note).await,
        failed
    );
    let submitted = store.submitted.lock().unwrap();
    assert_eq!(submitted.len(), 1);
    assert_eq!(serde_json::to_value(&submitted[0]).unwrap(), expected);
}

#[tokio::test]
async fn batch_backfill_refuses_unknown_outcomes_and_inconsistent_counts_without_replay() {
    let cases = [
        (
            Err(StorageError::writer_task_terminated(
                WriterTaskRequestState::SideEffectsUnknown,
            )),
            3,
        ),
        (
            Ok(BatchWriteSummary {
                attempted: 2,
                affected: 3,
                ..Default::default()
            }),
            3,
        ),
        (
            Ok(BatchWriteSummary {
                attempted: 3,
                affected: 1,
                failed: 1,
                ..Default::default()
            }),
            3,
        ),
        (
            Ok(BatchWriteSummary {
                attempted: 3,
                affected: u64::MAX,
                failed: 4,
                ..Default::default()
            }),
            3,
        ),
        // Aggregate-only backends need not provide per-item diagnostics.
        (
            Ok(BatchWriteSummary {
                attempted: 3,
                affected: 2,
                failed: 1,
                ..Default::default()
            }),
            1,
        ),
    ];
    for (outcome, expected) in cases {
        let store = BatchStore::new(outcome);
        assert_eq!(
            fts_backfill_documents(&store, documents(3), SubstrateKind::Note).await,
            expected
        );
        assert_eq!(store.submitted.lock().unwrap().len(), 1);
    }
    let store = BatchStore::new(Err(StorageError::WriterTaskBusy { timeout_ms: 1 }));
    assert_eq!(
        fts_backfill_documents(&store, vec![], SubstrateKind::Note).await,
        0
    );
    assert!(store.submitted.lock().unwrap().is_empty());
}

#[tokio::test]
async fn batch_backfill_keeps_successful_peers_and_restores_refused_note_and_entity() {
    let rt = KhiveRuntime::memory().expect("runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize");
    let notes: Vec<_> = (0..3)
        .map(|i| {
            let mut note = Note::new("local", "memory", format!("note {i}"));
            note.name = Some(format!("title {i}"));
            note.properties = Some(serde_json::json!({"position": i}));
            note
        })
        .collect();
    let entities: Vec<_> = (0..3)
        .map(|i| {
            let mut entity = Entity::new("local", "concept", format!("entity {i}"))
                .with_description(format!("description {i}"));
            entity.tags = vec!["backfill".into()];
            entity.properties = Some(serde_json::json!({"position": i}));
            entity
        })
        .collect();
    for is_note in [true, false] {
        let (fts, table, docs) = if is_note {
            (
                rt.text_for_notes(&token).unwrap(),
                "fts_notes_rowids",
                notes.iter().map(note_fts_document).collect::<Vec<_>>(),
            )
        } else {
            (
                rt.text(&token).unwrap(),
                "fts_entities_rowids",
                entities.iter().map(entity_fts_document).collect::<Vec<_>>(),
            )
        };
        let mut stale = docs[1].clone();
        stale.body = "previous committed body".into();
        fts.upsert_document(stale.clone()).await.unwrap();
        {
            let writer = rt.backend().pool().writer().unwrap();
            // This private in-memory fixture refuses the rowid-map insertion
            // after the FTS replacement, exercising savepoint restoration.
            writer
                .autocommit(|conn| {
                    Ok(conn.execute_batch(&format!(
                        "CREATE TRIGGER refuse_backfill BEFORE INSERT ON {table} \
                 WHEN NEW.subject_id = '{}' BEGIN SELECT RAISE(FAIL, 'refused backfill'); END",
                        docs[1].subject_id
                    ))?)
                })
                .unwrap();
        }
        let failed = if is_note {
            fts_backfill_notes_batch(&rt, &token, &notes).await
        } else {
            fts_backfill_entities_batch(&rt, &token, &entities).await
        };
        assert_eq!(failed, 1);
        for (index, doc) in docs.iter().enumerate() {
            let actual = fts
                .get_document("local", doc.subject_id)
                .await
                .unwrap()
                .unwrap();
            let expected = if index == 1 { &stale } else { doc };
            if is_note {
                assert!(actual.tags.is_empty());
            } else {
                assert_eq!(actual.tags, vec!["backfill"]);
            }
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
        rt.backend()
            .pool()
            .writer()
            .unwrap()
            .autocommit(|conn| Ok(conn.execute_batch("DROP TRIGGER refuse_backfill")?))
            .unwrap();
        let failed = if is_note {
            fts_backfill_notes_batch(&rt, &token, &notes).await
        } else {
            fts_backfill_entities_batch(&rt, &token, &entities).await
        };
        assert_eq!(failed, 0);
        let retried = fts
            .get_document("local", docs[1].subject_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(retried).unwrap(),
            serde_json::to_value(&docs[1]).unwrap()
        );
    }
}
