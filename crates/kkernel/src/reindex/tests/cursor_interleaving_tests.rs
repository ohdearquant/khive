use super::*;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use khive_runtime::{EmbedderProvider, NamespaceToken, RuntimeConfig, RuntimeError};
use khive_storage::types::{DeleteMode, TextQueryMode, TextSearchRequest};
use khive_storage::{EntityStore, NoteStore};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

const MODEL: &str = "cursor_interleaving_test";
const MARKERS: [&str; 4] = ["cursoralpha", "cursorbravo", "cursorcharlie", "cursordelta"];

const SENTINEL: &str = "cursorforeign";

#[derive(Clone, Copy)]
enum Mutation {
    Insert,
    SoftDelete,
    HardDelete,
}

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

    async fn put(&self, namespace: &str, id: Uuid, marker: &str, created_at: i64) {
        match self {
            Self::Entities(store) => {
                let mut row = Entity::new(namespace, "concept", marker);
                row.id = id;
                row.created_at = created_at;
                row.updated_at = created_at;
                store.upsert_entity(row).await.unwrap();
            }
            Self::Notes(store) => {
                let mut row = Note::new(namespace, "observation", marker);
                row.id = id;
                row.created_at = created_at;
                row.updated_at = created_at;
                store.upsert_note(row).await.unwrap();
            }
        }
    }

    async fn delete(&self, id: Uuid, mode: DeleteMode) {
        let hard = matches!(mode, DeleteMode::Hard);
        let deleted = match self {
            Self::Entities(store) => store.delete_entity(id, mode).await.unwrap(),
            Self::Notes(store) => store.delete_note(id, mode).await.unwrap(),
        };
        assert!(deleted, "the fetched boundary must exist before deletion");
        if hard {
            self.assert_absent(id).await;
        }
    }

    async fn assert_absent(&self, id: Uuid) {
        let absent = match self {
            Self::Entities(store) => store
                .get_entity_including_deleted(id)
                .await
                .unwrap()
                .is_none(),
            Self::Notes(store) => store
                .get_note_including_deleted(id)
                .await
                .unwrap()
                .is_none(),
        };
        assert!(absent, "hard deletion must remove the boundary row");
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
    mutation: Mutation,
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
            match self.mutation {
                Mutation::Insert => {
                    // Newest by the old DESC sort, but last by immutable insertion sequence.
                    self.records.put("local", self.added, MARKERS[3], 400).await;
                }
                Mutation::SoftDelete => self.records.delete(self.boundary, DeleteMode::Soft).await,
                Mutation::HardDelete => self.records.delete(self.boundary, DeleteMode::Hard).await,
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
    cfg.disable_embedding_models();
    cfg.brain_profile = None;
    cfg.packs.clear();
    cfg
}

async fn interleaved_reindex(entities: bool, mutation: Mutation) {
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
    let foreign_id = Uuid::new_v4();
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
        let foreign_token = rt.authorize(Namespace::parse("foreign").unwrap()).unwrap();
        let foreign_records = Records::from_runtime(&rt, &foreign_token, entities);
        foreign_records
            .put("foreign", foreign_id, SENTINEL, 500)
            .await;
        // Align initial insertion and DESC orders so the mutation targets either first page.
        for i in 0..3 {
            records
                .put("local", ids[i], MARKERS[i], 300 - i as i64 * 100)
                .await;
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
                mutation,
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
    let expected_len = if matches!(mutation, Mutation::Insert) {
        4
    } else {
        3
    };
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
        let deleted = !matches!(mutation, Mutation::Insert) && i == 0;
        if deleted && matches!(mutation, Mutation::HardDelete) {
            records.assert_absent(ids[i]).await;
            continue;
        }
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
    let foreign_token = rt.authorize(Namespace::parse("foreign").unwrap()).unwrap();
    let foreign_records = Records::from_runtime(&rt, &foreign_token, entities);
    foreign_records
        .assert_row(foreign_id, SENTINEL, false)
        .await;
    assert!(vectors
        .batch_exists(&[foreign_id], "foreign")
        .await
        .unwrap()
        .is_empty());
    let foreign_fts = if entities {
        rt.text(&foreign_token).unwrap()
    } else {
        rt.text_for_notes(&foreign_token).unwrap()
    };
    assert!(
        foreign_fts
            .search(TextSearchRequest {
                query: SENTINEL.into(),
                mode: TextQueryMode::Plain,
                filter: None,
                top_k: 10,
                snippet_chars: 0,
            })
            .await
            .unwrap()
            .is_empty(),
        "reindex must not backfill the foreign sentinel"
    );
}

#[tokio::test]
async fn entity_reindex_visits_newer_insert_once() {
    if crate::test_process::run_in_child() {
        return;
    }
    interleaved_reindex(true, Mutation::Insert).await;
}

#[tokio::test]
async fn note_reindex_visits_newer_insert_once() {
    if crate::test_process::run_in_child() {
        return;
    }
    interleaved_reindex(false, Mutation::Insert).await;
}

#[tokio::test]
async fn entity_reindex_continues_after_boundary_soft_delete() {
    if crate::test_process::run_in_child() {
        return;
    }
    interleaved_reindex(true, Mutation::SoftDelete).await;
}

#[tokio::test]
async fn note_reindex_continues_after_boundary_soft_delete() {
    if crate::test_process::run_in_child() {
        return;
    }
    interleaved_reindex(false, Mutation::SoftDelete).await;
}

#[tokio::test]
async fn entity_reindex_continues_after_boundary_hard_delete() {
    if crate::test_process::run_in_child() {
        return;
    }
    interleaved_reindex(true, Mutation::HardDelete).await;
}

#[tokio::test]
async fn note_reindex_continues_after_boundary_hard_delete() {
    if crate::test_process::run_in_child() {
        return;
    }
    interleaved_reindex(false, Mutation::HardDelete).await;
}
