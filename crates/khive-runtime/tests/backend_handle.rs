use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use khive_db::StorageBackend;
use khive_runtime::{BackendHandle, BackendHandleError, RetrievalTier};
use khive_storage::{
    Entity, EntityFilter, Note, SparseStore, SparseStoreFactory, SparseVector, SqlAccess,
    SqlStatement, StorageCapability, StorageError, StorageResult, TextDocument, TextSearch,
    TextSearchFactory, VectorStore, VectorStoreFactory, WriterTaskRequestState,
};
use khive_types::SubstrateKind;
use uuid::Uuid;

async fn tables(sql: &dyn SqlAccess) -> BTreeSet<String> {
    sql.reader()
        .await
        .unwrap()
        .query_all(SqlStatement::new(
            "SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name",
            vec![],
        ))
        .await
        .unwrap()
        .iter()
        .map(|row| row.text("name").unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn sqlite_construction_is_inert_and_each_retrieval_binding_opens_on_demand() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(StorageBackend::sqlite_for_test(dir.path().join("handle.db")).unwrap());
    let before = tables(backend.sql().as_ref()).await;
    let handle = BackendHandle::from_sqlite(backend.clone());
    let cloned = handle.clone();
    assert!(Arc::ptr_eq(handle.entity(), cloned.entity()));
    assert!(Arc::ptr_eq(handle.note(), cloned.note()));
    assert!(Arc::ptr_eq(handle.graph(), cloned.graph()));
    assert!(Arc::ptr_eq(handle.event(), cloned.event()));
    assert!(Arc::ptr_eq(handle.sql(), cloned.sql()));
    assert!(handle.event().supports_idempotent_audit_batch());
    assert_eq!(handle.sql().database_path(), backend.sql().database_path());
    assert_eq!(tables(handle.sql().as_ref()).await, before);
    assert_eq!(backend.notes_seq_repair_run_count(), 0);

    let vector = handle.vector("first", "embedding-A", 3, "one").unwrap();
    let after_vector = tables(handle.sql().as_ref()).await;
    assert!(after_vector.contains("vec_first"));
    assert!(!after_vector.contains("sparse_first"));
    assert!(!after_vector.contains("fts_first"));
    for core in ["entities", "notes", "graph_edges", "events"] {
        assert_eq!(after_vector.contains(core), before.contains(core));
    }
    assert_eq!(vector.info().await.unwrap().dimensions, 3);
    let subject = Uuid::new_v4();
    vector
        .insert(
            subject,
            SubstrateKind::Note,
            "one",
            "content",
            vec![vec![1.0, 0.0, 0.0]],
        )
        .await
        .unwrap();
    assert_eq!(vector.count().await.unwrap(), 1);
    assert_eq!(
        handle
            .vector("first", "embedding-A", 3, "two")
            .unwrap()
            .count()
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        handle
            .vector("second", "embedding-B", 2, "one")
            .unwrap()
            .info()
            .await
            .unwrap()
            .dimensions,
        2
    );
    let rows = handle
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement::new(
            "SELECT embedding_model, namespace FROM vec_first",
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text("embedding_model").unwrap(), "embedding-A");
    assert_eq!(rows[0].text("namespace").unwrap(), "one");

    let sparse = handle.sparse("first", "one").unwrap();
    let after_sparse = tables(handle.sql().as_ref()).await;
    assert!(after_sparse.contains("sparse_first"));
    assert!(!after_sparse.contains("fts_first"));
    sparse
        .insert_sparse(
            subject,
            SubstrateKind::Note,
            "one",
            "content",
            SparseVector {
                indices: vec![1],
                values: vec![1.0],
            },
        )
        .await
        .unwrap();
    assert_eq!(sparse.count().await.unwrap(), 1);
    assert_eq!(
        handle
            .sparse("first", "two")
            .unwrap()
            .count()
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        handle
            .sparse("second", "one")
            .unwrap()
            .count()
            .await
            .unwrap(),
        0
    );

    let text = handle.text("first", "unicode61").unwrap();
    let trigram = handle.text("second", "trigram").unwrap();
    assert!(tables(handle.sql().as_ref()).await.contains("fts_first"));
    assert!(tables(handle.sql().as_ref()).await.contains("fts_second"));
    let document = TextDocument {
        subject_id: subject,
        kind: SubstrateKind::Note,
        record_kind: Some("observation".into()),
        namespace: "one".into(),
        title: Some("Binding".into()),
        body: "exact factory binding".into(),
        tags: vec![],
        metadata: None,
        updated_at: chrono::Utc::now(),
    };
    text.upsert_document(document.clone()).await.unwrap();
    assert_eq!(
        text.get_document("one", subject)
            .await
            .unwrap()
            .unwrap()
            .body,
        document.body
    );
    assert!(text.get_document("two", subject).await.unwrap().is_none());
    assert!(trigram
        .get_document("one", subject)
        .await
        .unwrap()
        .is_none());
    assert_eq!(backend.notes_seq_repair_run_count(), 0);
}

#[tokio::test]
async fn sqlite_core_handles_execute_existing_data_operations() {
    let backend = Arc::new(StorageBackend::memory().unwrap());
    backend.prepare_core_schema().unwrap();
    let handle = BackendHandle::from_sqlite(backend.clone());
    let entity = Entity::new("local", "concept", "backend handle");
    handle.entity().upsert_entity(entity.clone()).await.unwrap();
    assert_eq!(
        handle
            .entity()
            .get_entity(entity.id)
            .await
            .unwrap()
            .unwrap()
            .name,
        entity.name
    );
    assert_eq!(
        handle
            .entity()
            .count_entities("local", EntityFilter::default())
            .await
            .unwrap(),
        1
    );
    let note = Note::new("local", "observation", "lazy readiness");
    handle.note().upsert_note(note.clone()).await.unwrap();
    assert_eq!(
        handle
            .note()
            .get_note(note.id)
            .await
            .unwrap()
            .unwrap()
            .content,
        note.content
    );
    assert_eq!(backend.notes_seq_repair_run_count(), 1);
    let now = chrono::Utc::now();
    let edge = khive_storage::Edge {
        id: Uuid::new_v4().into(),
        namespace: "local".into(),
        source_id: note.id,
        target_id: entity.id,
        relation: khive_types::EdgeRelation::Annotates,
        weight: 1.0,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: None,
        target_backend: None,
    };
    handle.graph().upsert_edge(edge.clone()).await.unwrap();
    assert_eq!(
        handle
            .graph()
            .get_edge(edge.id)
            .await
            .unwrap()
            .unwrap()
            .target_id,
        entity.id
    );
    let event = khive_storage::Event::new(
        "local",
        "create",
        khive_types::EventKind::NoteCreated,
        SubstrateKind::Note,
        "backend-handle-test",
    );
    handle.event().preflight_event(&event).unwrap();
    handle.event().append_event(event.clone()).await.unwrap();
    assert_eq!(
        handle.event().get_event(event.id).await.unwrap(),
        Some(event)
    );
    assert!(tables(handle.sql().as_ref()).await.contains("events"));
}

#[derive(Debug, PartialEq, Eq)]
enum Binding {
    Vector(String, String, usize, String),
    Sparse(String, String),
    Text(String, String),
}

struct RecordingFactories {
    vector: Arc<dyn VectorStore>,
    sparse: Arc<dyn SparseStore>,
    text: Arc<dyn TextSearch>,
    calls: Mutex<Vec<Binding>>,
    failure: Mutex<Option<StorageError>>,
}

impl RecordingFactories {
    fn new() -> Self {
        let backend = StorageBackend::memory().unwrap();
        Self {
            vector: backend.vectors("fixture", "fixture-model", 2).unwrap(),
            sparse: backend.sparse("fixture").unwrap(),
            text: backend.text("fixture").unwrap(),
            calls: Mutex::new(Vec::new()),
            failure: Mutex::new(None),
        }
    }

    fn record(&self, binding: Binding) -> StorageResult<()> {
        self.calls.lock().unwrap().push(binding);
        match self.failure.lock().unwrap().take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl VectorStoreFactory for RecordingFactories {
    fn open(
        &self,
        model_key: &str,
        embedding_model: &str,
        dimensions: usize,
        namespace: &str,
    ) -> StorageResult<Arc<dyn VectorStore>> {
        self.record(Binding::Vector(
            model_key.into(),
            embedding_model.into(),
            dimensions,
            namespace.into(),
        ))?;
        Ok(self.vector.clone())
    }
}

impl SparseStoreFactory for RecordingFactories {
    fn open(&self, model_key: &str, namespace: &str) -> StorageResult<Arc<dyn SparseStore>> {
        self.record(Binding::Sparse(model_key.into(), namespace.into()))?;
        Ok(self.sparse.clone())
    }
}

impl TextSearchFactory for RecordingFactories {
    fn open(&self, table_key: &str, tokenizer: &str) -> StorageResult<Arc<dyn TextSearch>> {
        self.record(Binding::Text(table_key.into(), tokenizer.into()))?;
        Ok(self.text.clone())
    }
}

fn injected(
    core: &BackendHandle,
    factories: &Arc<RecordingFactories>,
    missing: &[RetrievalTier],
) -> BackendHandle {
    BackendHandle::from_parts(
        core.entity().clone(),
        core.note().clone(),
        core.graph().clone(),
        core.event().clone(),
        core.sql().clone(),
        (!missing.contains(&RetrievalTier::Vector))
            .then(|| factories.clone() as Arc<dyn VectorStoreFactory>),
        (!missing.contains(&RetrievalTier::Sparse))
            .then(|| factories.clone() as Arc<dyn SparseStoreFactory>),
        (!missing.contains(&RetrievalTier::Text))
            .then(|| factories.clone() as Arc<dyn TextSearchFactory>),
    )
}

fn acquire(handle: &BackendHandle, tier: RetrievalTier) -> Result<(), BackendHandleError> {
    match tier {
        RetrievalTier::Vector => handle
            .vector("model", "embedding", 7, "namespace")
            .map(|_| ()),
        RetrievalTier::Sparse => handle.sparse("lexical", "namespace").map(|_| ()),
        RetrievalTier::Text => handle.text("corpus", "unicode61").map(|_| ()),
    }
}

#[test]
fn from_parts_preserves_core_and_returned_store_identity_and_exact_factory_arguments() {
    let core = BackendHandle::from_sqlite(Arc::new(StorageBackend::memory().unwrap()));
    let factories = Arc::new(RecordingFactories::new());
    let handle = injected(&core, &factories, &[]);
    let cloned = handle.clone();
    assert!(factories.calls.lock().unwrap().is_empty());
    for candidate in [&handle, &cloned] {
        assert!(Arc::ptr_eq(core.entity(), candidate.entity()));
        assert!(Arc::ptr_eq(core.note(), candidate.note()));
        assert!(Arc::ptr_eq(core.graph(), candidate.graph()));
        assert!(Arc::ptr_eq(core.event(), candidate.event()));
        assert!(Arc::ptr_eq(core.sql(), candidate.sql()));
    }
    assert!(Arc::ptr_eq(
        &handle
            .vector("raw-key", "model spelling", 137, " tenant ")
            .unwrap(),
        &factories.vector
    ));
    assert!(Arc::ptr_eq(
        &cloned.sparse("lexical-key", "other").unwrap(),
        &factories.sparse
    ));
    assert!(Arc::ptr_eq(
        &handle.text("table-key", "tokenizer spelling").unwrap(),
        &factories.text
    ));
    assert_eq!(
        *factories.calls.lock().unwrap(),
        vec![
            Binding::Vector(
                "raw-key".into(),
                "model spelling".into(),
                137,
                " tenant ".into()
            ),
            Binding::Sparse("lexical-key".into(), "other".into()),
            Binding::Text("table-key".into(), "tokenizer spelling".into()),
        ]
    );
}

#[test]
fn each_optional_tier_fails_only_when_requested_and_names_the_missing_capability() {
    let core = BackendHandle::from_sqlite(Arc::new(StorageBackend::memory().unwrap()));
    let factories = Arc::new(RecordingFactories::new());
    let tiers = [
        RetrievalTier::Vector,
        RetrievalTier::Sparse,
        RetrievalTier::Text,
    ];
    for missing in tiers {
        let handle = injected(&core, &factories, &[missing]);
        factories.calls.lock().unwrap().clear();
        let error = acquire(&handle, missing).unwrap_err();
        assert!(matches!(error, BackendHandleError::MissingCapability { tier } if tier == missing));
        assert_eq!(
            error.to_string(),
            format!("backend does not provide the {missing} capability")
        );
        assert!(factories.calls.lock().unwrap().is_empty());
        for available in tiers.into_iter().filter(|tier| *tier != missing) {
            acquire(&handle, available).unwrap();
        }
        assert_eq!(factories.calls.lock().unwrap().len(), 2);
    }
    let handle = injected(&core, &factories, &tiers);
    factories.calls.lock().unwrap().clear();
    for tier in tiers {
        assert!(
            matches!(acquire(&handle, tier), Err(BackendHandleError::MissingCapability { tier: actual }) if actual == tier)
        );
    }
    assert!(factories.calls.lock().unwrap().is_empty());
}

#[derive(Debug, thiserror::Error)]
#[error("factory sentinel {0}")]
struct FactorySentinel(u32);

#[test]
fn present_factory_failures_retain_storage_classification_and_typed_source() {
    let core = BackendHandle::from_sqlite(Arc::new(StorageBackend::memory().unwrap()));
    let factories = Arc::new(RecordingFactories::new());
    let handle = injected(&core, &factories, &[]);
    for tier in [
        RetrievalTier::Vector,
        RetrievalTier::Sparse,
        RetrievalTier::Text,
    ] {
        *factories.failure.lock().unwrap() = Some(StorageError::driver(
            StorageCapability::Text,
            "fixture",
            FactorySentinel(73),
        ));
        let BackendHandleError::Storage(StorageError::Driver {
            capability,
            operation,
            source,
        }) = acquire(&handle, tier).unwrap_err()
        else {
            panic!("factory driver failure must remain a storage driver failure");
        };
        assert_eq!(capability, StorageCapability::Text);
        assert_eq!(operation, "fixture");
        assert_eq!(source.downcast_ref::<FactorySentinel>().unwrap().0, 73);

        *factories.failure.lock().unwrap() = Some(StorageError::Unsupported {
            capability: StorageCapability::Graph,
            operation: "factory-specific".into(),
            message: "present factory refusal".into(),
        });
        assert!(
            matches!(acquire(&handle, tier), Err(BackendHandleError::Storage(StorageError::Unsupported {
            capability: StorageCapability::Graph, ref operation, ref message,
        })) if operation == "factory-specific" && message == "present factory refusal")
        );

        *factories.failure.lock().unwrap() = Some(StorageError::writer_task_terminated(
            WriterTaskRequestState::SideEffectsUnknown,
        ));
        assert!(matches!(
            acquire(&handle, tier),
            Err(BackendHandleError::Storage(
                StorageError::WriterTaskTerminated {
                    request_state: WriterTaskRequestState::SideEffectsUnknown,
                    ..
                }
            ))
        ));

        *factories.failure.lock().unwrap() = Some(StorageError::AdmissionTimeout {
            operation: "binding-open".into(),
            timeout_ms: 31,
            pool_identity: Some("fixture.db".into()),
        });
        assert!(
            matches!(acquire(&handle, tier), Err(BackendHandleError::Storage(StorageError::AdmissionTimeout {
            ref operation, timeout_ms: 31, ref pool_identity,
        })) if operation == "binding-open" && pool_identity.as_deref() == Some("fixture.db"))
        );
    }
    assert_eq!(factories.calls.lock().unwrap().len(), 12);
}
