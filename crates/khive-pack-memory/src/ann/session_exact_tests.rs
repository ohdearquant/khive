use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use khive_db::StorageBackend;
use khive_runtime::{EmbedderProvider, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError};
use khive_storage::{DeleteMode, Note, SqlStatement, SqlValue};
use khive_types::SubstrateKind;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use uuid::Uuid;

use super::super::{count_session_statements, sanitize_model_key};
use super::session_exact_candidates;

const MODEL: &str = "session-tie-order";
const QUERY: [f32; 3] = [1.0, 0.0, 0.0];

struct ConstantProvider;
struct ConstantService;
#[async_trait::async_trait]
impl EmbeddingService for ConstantService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| QUERY.to_vec()).collect())
    }
    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "session-tie-constant"
    }
}
#[async_trait::async_trait]
impl EmbedderProvider for ConstantProvider {
    fn name(&self) -> &str {
        MODEL
    }
    fn dimensions(&self) -> usize {
        3
    }
    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        Ok(Arc::new(ConstantService))
    }
}

fn open(path: &Path) -> KhiveRuntime {
    let backend = StorageBackend::sqlite_for_test_with_journal_mode_in(
        path,
        true,
        Duration::from_secs(5),
        path.parent().unwrap().join("locks"),
    )
    .unwrap();
    backend.prepare_core_schema().unwrap();
    let rt = KhiveRuntime::from_backend(
        Arc::new(backend),
        RuntimeConfig {
            db_path: Some(path.to_owned()),
            default_namespace: Namespace::local(),
            visible_namespaces: Vec::new(),
            allowed_outbound_namespaces: Vec::new(),
            actor_id: Some("session-tie-fixture".into()),
            credentials: Vec::new(),
            visibility_receipts: None,
            events_split: None,
            mounts: Vec::new(),
            blob: Default::default(),
            packs: Vec::new(),
            ..RuntimeConfig::no_embeddings()
        },
    );
    rt.try_register_embedder(ConstantProvider).unwrap();
    rt
}

async fn create(rt: &KhiveRuntime, namespace: &str) -> (Uuid, u64) {
    let token = rt.authorize(Namespace::parse(namespace).unwrap()).unwrap();
    let (note, fences) = rt
        .create_note_with_decay_for_embedding_model_with_visibility(
            &token,
            "memory",
            None,
            "equal nonzero embedding",
            Some(0.8),
            0.01,
            None,
            vec![],
            Some(MODEL),
        )
        .await
        .unwrap();
    let seq = fences
        .into_iter()
        .find(|(model, _)| model == MODEL)
        .expect("actual committed model receipt")
        .1;
    (note.id, seq)
}

async fn read(
    rt: &KhiveRuntime,
    visible: &[&str],
    receipt_namespace: &str,
    seq: u64,
    k: usize,
) -> Option<Vec<(Uuid, f32)>> {
    let visible: Vec<String> = visible.iter().map(|ns| (*ns).to_owned()).collect();
    let (result, probes, exact) = count_session_statements(session_exact_candidates(
        rt,
        MODEL,
        &QUERY,
        &visible,
        receipt_namespace,
        seq,
        k,
    ))
    .await;
    assert_eq!(
        (probes, exact),
        (0, 1),
        "candidate proof stays in exactly one statement"
    );
    result.unwrap()
}

async fn note(rt: &KhiveRuntime, id: u128, namespace: &str) {
    let token = rt.authorize(Namespace::parse(namespace).unwrap()).unwrap();
    let mut note = Note::new(namespace, "memory", "scope control");
    note.id = Uuid::from_u128(id);
    rt.notes(&token).unwrap().upsert_note(note).await.unwrap();
}

async fn vector(
    rt: &KhiveRuntime,
    id: u128,
    namespace: &str,
    model: &str,
    kind: SubstrateKind,
    field: &str,
    embedding: [f32; 3],
) {
    let store = rt
        .backend()
        .vectors_for_namespace(&sanitize_model_key(MODEL), model, 3, namespace)
        .unwrap();
    store
        .insert(
            Uuid::from_u128(id),
            kind,
            namespace,
            field,
            vec![embedding.to_vec()],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn tied_live_note_boundary_and_receipt_survive_file_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let rt = open(&path);
    let mut ids = Vec::new();
    let mut seq = 0;
    for _ in 0..6 {
        let (id, committed_seq) = create(&rt, "local").await;
        ids.push(id);
        seq = committed_seq;
    }
    ids.sort_unstable();
    let expected: Vec<_> = ids[..3].iter().map(|id| (*id, 1.0)).collect();
    // Force the same known ascending physical insertion order as the DB
    // regression, while retaining real notes and a real committed receipt.
    {
        let store = rt
            .backend()
            .vectors_for_namespace(&sanitize_model_key(MODEL), MODEL, 3, "local")
            .unwrap();
        for id in &ids {
            assert!(store.delete(*id).await.unwrap());
        }
        for id in &ids {
            store
                .insert(
                    *id,
                    SubstrateKind::Note,
                    "local",
                    "note.content",
                    vec![QUERY.to_vec()],
                )
                .await
                .unwrap();
        }
    }
    let before = read(&rt, &["local"], "local", seq, 3).await.unwrap();
    assert_eq!(before, expected);
    assert_eq!(
        read(&rt, &["local"], "local", seq, 4097)
            .await
            .unwrap()
            .len(),
        6
    );
    drop(rt);
    let reopened = open(&path);
    let after = read(&reopened, &["local"], "local", seq, 3).await.unwrap();
    assert_eq!(after, expected);
    assert_eq!(after, before);
}

#[tokio::test]
async fn namespace_arms_filter_metadata_then_merge_without_duplicate_scope_hits() {
    let dir = tempfile::tempdir().unwrap();
    let rt = open(&dir.path().join("scope.db"));
    let (_, seq) = create(&rt, "receipt").await;
    for (namespace, ids) in [("local", [10, 20, 30, 40]), ("other", [15, 25, 35, 45])] {
        for id in ids {
            note(&rt, id, namespace).await;
            vector(
                &rt,
                id,
                namespace,
                MODEL,
                SubstrateKind::Note,
                "note.content",
                QUERY,
            )
            .await;
        }
    }
    // All distractors have smaller IDs and the same distance as valid notes.
    for id in [1, 2, 3] {
        note(&rt, id, "local").await;
    }
    vector(
        &rt,
        1,
        "local",
        MODEL,
        SubstrateKind::Entity,
        "note.content",
        QUERY,
    )
    .await;
    vector(
        &rt,
        2,
        "local",
        MODEL,
        SubstrateKind::Note,
        "other.field",
        QUERY,
    )
    .await;
    vector(
        &rt,
        3,
        "local",
        "wrong-model",
        SubstrateKind::Note,
        "note.content",
        QUERY,
    )
    .await;
    note(&rt, 4, "hidden").await;
    vector(
        &rt,
        4,
        "hidden",
        MODEL,
        SubstrateKind::Note,
        "note.content",
        QUERY,
    )
    .await;
    note(&rt, 5, "hidden").await;
    vector(
        &rt,
        5,
        "local",
        MODEL,
        SubstrateKind::Note,
        "note.content",
        QUERY,
    )
    .await;
    let expected = vec![
        (Uuid::from_u128(10), 1.0),
        (Uuid::from_u128(15), 1.0),
        (Uuid::from_u128(20), 1.0),
    ];
    assert_eq!(
        read(&rt, &["other", "local", "other"], "receipt", seq, 3)
            .await
            .unwrap(),
        expected
    );
    assert_eq!(
        read(&rt, &["local", "other"], "receipt", seq, 3)
            .await
            .unwrap(),
        expected
    );
}

#[tokio::test]
async fn post_limit_orphans_are_dropped_without_backfill() {
    for state in ["missing", "deleted", "namespace-mismatch"] {
        let dir = tempfile::tempdir().unwrap();
        let rt = open(&dir.path().join("orphan.db"));
        let (_, seq) = create(&rt, "receipt").await;
        for id in [10, 20] {
            note(&rt, id, "local").await;
            vector(
                &rt,
                id,
                "local",
                MODEL,
                SubstrateKind::Note,
                "note.content",
                QUERY,
            )
            .await;
        }
        match state {
            "deleted" => {
                note(&rt, 1, "local").await;
                let token = rt.authorize(Namespace::local()).unwrap();
                assert!(rt
                    .notes(&token)
                    .unwrap()
                    .delete_note(Uuid::from_u128(1), DeleteMode::Soft)
                    .await
                    .unwrap());
            }
            "namespace-mismatch" => note(&rt, 1, "hidden").await,
            "missing" => {}
            _ => unreachable!(),
        }
        vector(
            &rt,
            1,
            "local",
            MODEL,
            SubstrateKind::Note,
            "note.content",
            QUERY,
        )
        .await;
        assert_eq!(
            read(&rt, &["local"], "receipt", seq, 2).await.unwrap(),
            vec![(Uuid::from_u128(10), 1.0)],
            "{state} must consume its selected slot without promoting id20"
        );
        assert_eq!(
            read(&rt, &["local"], "receipt", seq, 3).await.unwrap(),
            vec![(Uuid::from_u128(10), 1.0), (Uuid::from_u128(20), 1.0)],
            "the survivor beyond the smaller boundary is actually stored"
        );
    }
}

#[tokio::test]
async fn receipt_refusals_and_empty_candidates_keep_one_statement_proof() {
    let dir = tempfile::tempdir().unwrap();
    let rt = open(&dir.path().join("proof.db"));
    let (_, seq) = create(&rt, "local").await;
    assert_eq!(read(&rt, &[], "local", seq, 3).await, Some(vec![]));
    assert_eq!(read(&rt, &["local"], "local", seq, 0).await, Some(vec![]));
    assert!(read(&rt, &["local"], "other", seq, 3).await.is_none());
    assert!(read(&rt, &["local"], "local", seq + 1, 3).await.is_none());
    assert!(read(&rt, &["local"], "local", 0, 3).await.is_none());
    let (too_large, probes, exact) = count_session_statements(session_exact_candidates(
        &rt,
        MODEL,
        &QUERY,
        &["local".into()],
        "local",
        u64::MAX,
        3,
    ))
    .await;
    assert!(too_large.unwrap().is_none());
    assert_eq!(
        (probes, exact),
        (0, 0),
        "impossible SQLite sequence refuses before I/O"
    );
    for (operation, model) in [("delete", MODEL), ("upsert", "wrong-model")] {
        let mut writer = rt.sql().writer().await.unwrap();
        writer
            .execute(SqlStatement {
                sql: "UPDATE ann_write_log SET op = ?1, embedding_model = ?2 WHERE seq = ?3".into(),
                params: vec![
                    SqlValue::Text(operation.into()),
                    SqlValue::Text(model.into()),
                    SqlValue::Integer(seq as i64),
                ],
                label: Some("session_tie_receipt_control".into()),
            })
            .await
            .unwrap();
        drop(writer);
        assert!(read(&rt, &["local"], "local", seq, 3).await.is_none());
    }
}

#[tokio::test]
async fn distance_precedes_uuid_and_bad_numeric_queries_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let rt = open(&dir.path().join("numeric.db"));
    let (_, seq) = create(&rt, "receipt").await;
    for (id, embedding) in [
        (1, [-1.0, 0.0, 0.0]),
        (2, [0.0, 1.0, 0.0]),
        (3, [3.0, 4.0, 0.0]),
        (99, QUERY),
    ] {
        note(&rt, id, "local").await;
        vector(
            &rt,
            id,
            "local",
            MODEL,
            SubstrateKind::Note,
            "note.content",
            embedding,
        )
        .await;
    }
    let hits = read(&rt, &["local"], "receipt", seq, 4).await.unwrap();
    assert_eq!(
        hits.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        [99, 3, 2, 1].map(Uuid::from_u128)
    );
    for ((_, score), expected) in hits.iter().zip([1.0, 0.6, 0.0, -1.0]) {
        assert!((*score - expected).abs() < 1e-6);
    }
    for query in [
        vec![],
        vec![f32::NAN, 0.0, 0.0],
        vec![1.0, 0.0],
        vec![0.0; 3],
    ] {
        assert!(
            session_exact_candidates(&rt, MODEL, &query, &["local".into()], "receipt", seq, 4)
                .await
                .is_err()
        );
    }
    note(&rt, 100, "local").await;
    vector(
        &rt,
        100,
        "local",
        MODEL,
        SubstrateKind::Note,
        "note.content",
        [f32::MAX; 3],
    )
    .await;
    assert!(session_exact_candidates(
        &rt,
        MODEL,
        &[f32::MAX; 3],
        &["local".into()],
        "receipt",
        seq,
        5
    )
    .await
    .is_err());
}
