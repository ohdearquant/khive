use super::*;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use khive_runtime::{EmbedderProvider, NamespaceToken, RuntimeConfig, RuntimeError};
use khive_storage::types::{DeleteMode, TextQueryMode, TextSearchRequest};
use khive_storage::{EntityStore, NoteStore};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

const MODEL: &str = "cursor_interleaving_test";
const MARKERS: [&str; 4] = ["cursoralpha", "cursorbravo", "cursorcharlie", "cursordelta"];

#[derive(Clone)]
enum Records {
    Entities(Arc<dyn EntityStore>),
    Notes(Arc<dyn NoteStore>),
}

impl Records {
    fn from_runtime(rt: &KhiveRuntime, token: &NamespaceToken, entities: bool) -> Self {
        if entities {
            Self::Entities(rt.entities(token).unwrap())
        } else {
            Self::Notes(rt.notes(token).unwrap())
        }
    }

    async fn put(&self, id: Uuid, marker: &str, created_at: i64) {
        match self {
            Self::Entities(store) => {
                let mut row = Entity::new("local", "concept", marker);
                row.id = id;
                row.created_at = created_at;
                row.updated_at = created_at;
                store.upsert_entity(row).await.unwrap();
            }
            Self::Notes(store) => {
                let mut row = Note::new("local", "observation", marker);
                row.id = id;
                row.created_at = created_at;
                row.updated_at = created_at;
                store.upsert_note(row).await.unwrap();
            }
        }
    }

    async fn delete(&self, id: Uuid) {
        let deleted = match self {
            Self::Entities(store) => store.delete_entity(id, DeleteMode::Soft).await.unwrap(),
            Self::Notes(store) => store.delete_note(id, DeleteMode::Soft).await.unwrap(),
        };
        assert!(
            deleted,
            "the fetched boundary must exist before its soft delete"
        );
    }

    async fn assert_row(&self, id: Uuid, marker: &str, deleted: bool) {
        let (text, tombstoned) = match self {
            Self::Entities(store) => {
                let row = store
                    .get_entity_including_deleted(id)
                    .await
                    .unwrap()
                    .unwrap();
                (row.name, row.deleted_at.is_some())
            }
            Self::Notes(store) => {
                let row = store.get_note_including_deleted(id).await.unwrap().unwrap();
                (row.content, row.deleted_at.is_some())
            }
        };
        assert_eq!(text, marker);
        assert_eq!(tombstoned, deleted);
    }
}

struct InterleavingService {
    records: Records,
    insert: bool,
    boundary: Uuid,
    added: Uuid,
    fired: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl EmbeddingService for InterleavingService {
    async fn embed(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        assert!(model.document_instruction().is_none());
        self.seen.lock().unwrap().extend_from_slice(texts);
        if !self.fired.swap(true, Ordering::SeqCst) {
            assert_eq!(texts, &[MARKERS[0].to_string()]);
            if self.insert {
                // Newest by the old DESC sort, but last by immutable insertion sequence.
                self.records.put(self.added, MARKERS[3], 400).await;
            } else {
                self.records.delete(self.boundary).await;
            }
        }
        Ok(texts.iter().map(|_| vec![1.0; 8]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        MODEL
    }
}

struct InterleavingProvider(Arc<InterleavingService>);

#[async_trait::async_trait]
impl EmbedderProvider for InterleavingProvider {
    fn name(&self) -> &str {
        MODEL
    }
    fn dimensions(&self) -> usize {
        8
    }
    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        Ok(self.0.clone())
    }
}

fn no_models(mut cfg: RuntimeConfig) -> RuntimeConfig {
    cfg.embedding_model = None;
    cfg.additional_embedding_models.clear();
    cfg.brain_profile = None;
    cfg.packs.clear();
    cfg
}

async fn interleaved_reindex(entities: bool, insert: bool) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cursor.db");
    let config = write_empty_test_config(dir.path());
    let resolve = || {
        no_models(
            resolve_runtime_config(RuntimeConfigInputs {
                db: db.to_str(),
                config: Some(&config),
                namespace: Namespace::local(),
                namespace_explicit: true,
                actor_explicit: false,
                no_embed: true,
                packs: None,
                brain_profile: None,
            })
            .unwrap(),
        )
    };
    let ids = [
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    ];
    {
        let rt = KhiveRuntime::new(resolve()).unwrap();
        let token = rt.authorize(Namespace::local()).unwrap();
        let records = Records::from_runtime(&rt, &token, entities);
        // Align initial insertion and DESC orders so the mutation targets either first page.
        for i in 0..3 {
            records.put(ids[i], MARKERS[i], 300 - i as i64 * 100).await;
        }
        let fts = if entities {
            rt.text(&token).unwrap()
        } else {
            rt.text_for_notes(&token).unwrap()
        };
        assert!(
            fts.search(TextSearchRequest {
                query: MARKERS[1].into(),
                mode: TextQueryMode::Plain,
                filter: None,
                top_k: 10,
                snippet_chars: 0,
            })
            .await
            .unwrap()
            .is_empty(),
            "direct seeding must not pre-fill FTS"
        );
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let fired = Arc::new(AtomicBool::new(false));
    run_reindex_with_setup(
        ReindexArgs {
            id: None,
            db: Some(db.to_str().unwrap().to_owned()),
            config: Some(config.clone()),
            model: Some(MODEL.into()),
            batch_size: 1,
            keep_existing: false,
            namespace: Some("local".into()),
            knowledge_only: false,
            no_knowledge: true,
            best_effort: false,
            no_sections: false,
            sections_only: false,
            rebuild_fts: false,
            human: false,
        },
        no_models,
        |rt| {
            assert!(rt.registered_embedding_model_names().is_empty());
            let token = rt.authorize(Namespace::local())?;
            rt.register_embedder(InterleavingProvider(Arc::new(InterleavingService {
                records: Records::from_runtime(rt, &token, entities),
                insert,
                boundary: ids[0],
                added: ids[3],
                fired: Arc::clone(&fired),
                seen: Arc::clone(&seen),
            })));
            Ok(())
        },
    )
    .await
    .expect("real reindex pass");
    assert!(
        fired.load(Ordering::SeqCst),
        "the actual embed call must perform the mutation"
    );
    let expected_len = if insert { 4 } else { 3 };
    let mut expected = MARKERS[..expected_len]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let mut observed = seen.lock().unwrap().clone();
    expected.sort();
    observed.sort();
    assert_eq!(
        observed, expected,
        "each fetched record must be embedded exactly once"
    );

    let rt = KhiveRuntime::new(resolve()).unwrap();
    let token = rt.authorize(Namespace::local()).unwrap();
    let records = Records::from_runtime(&rt, &token, entities);
    rt.register_embedder(FixedReindexEmbedder {
        name: MODEL.into(),
        dimensions: 8,
    });
    let vectors = rt.vectors_for_model(&token, MODEL).unwrap();
    let fts = if entities {
        rt.text(&token).unwrap()
    } else {
        rt.text_for_notes(&token).unwrap()
    };
    for i in 0..expected_len {
        let deleted = !insert && i == 0;
        records.assert_row(ids[i], MARKERS[i], deleted).await;
        if deleted {
            continue;
        }
        assert!(vectors
            .batch_exists(&[ids[i]], "local")
            .await
            .unwrap()
            .contains(&ids[i]));
        let hits = fts
            .search(TextSearchRequest {
                query: MARKERS[i].into(),
                mode: TextQueryMode::Plain,
                filter: None,
                top_k: 10,
                snippet_chars: 0,
            })
            .await
            .unwrap();
        assert_eq!(
            hits.iter().map(|hit| hit.subject_id).collect::<Vec<_>>(),
            vec![ids[i]]
        );
    }
}

#[tokio::test]
async fn entity_reindex_visits_newer_insert_once() {
    if crate::test_process::run_in_child() {
        return;
    }
    interleaved_reindex(true, true).await;
}

#[tokio::test]
async fn note_reindex_visits_newer_insert_once() {
    if crate::test_process::run_in_child() {
        return;
    }
    interleaved_reindex(false, true).await;
}

#[tokio::test]
async fn entity_reindex_continues_after_boundary_soft_delete() {
    if crate::test_process::run_in_child() {
        return;
    }
    interleaved_reindex(true, false).await;
}

#[tokio::test]
async fn note_reindex_continues_after_boundary_soft_delete() {
    if crate::test_process::run_in_child() {
        return;
    }
    interleaved_reindex(false, false).await;
}
