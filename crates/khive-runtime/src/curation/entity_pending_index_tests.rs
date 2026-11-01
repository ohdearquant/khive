use super::*;
use crate::curation::EntityPatch;
use khive_storage::SqlValue;
use std::sync::Arc;
use uuid::Uuid;

const MODEL: &str = "pending-test-vector";

struct Provider;
struct Service;

#[async_trait::async_trait]
impl crate::embedder_registry::EmbedderProvider for Provider {
    fn name(&self) -> &str {
        MODEL
    }
    fn dimensions(&self) -> usize {
        4
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn lattice_embed::EmbeddingService>> {
        Ok(Arc::new(Service))
    }
}

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for Service {
    async fn embed(
        &self,
        texts: &[String],
        _: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        Ok(texts
            .iter()
            .map(|text| vec![text.len() as f32; 4])
            .collect())
    }
    fn supports_model(&self, _: lattice_embed::EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "pending-test-vector"
    }
}

async fn fixture() -> (KhiveRuntime, NamespaceToken, Entity) {
    let rt = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    rt.register_embedder(Provider);
    let entity = Entity::new("local", "concept", "old");
    rt.entities(&token)
        .unwrap()
        .upsert_entity(entity.clone())
        .await
        .unwrap();
    rt.reindex_entity(&token, &entity).await.unwrap();
    (rt, token, entity)
}

fn schema(rt: &KhiveRuntime, sql: &str) {
    rt.backend()
        .pool_arc()
        .writer()
        .unwrap()
        .autocommit(|conn| Ok(conn.execute_batch(sql)?))
        .unwrap();
}

async fn pending(rt: &KhiveRuntime, id: Uuid) -> Option<PendingEntityIndex> {
    pending_index::read(rt.sql().reader().await.unwrap().as_mut(), id)
        .await
        .unwrap()
}

async fn vector_blob(rt: &KhiveRuntime, id: Uuid) -> Vec<u8> {
    let value = rt
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(SqlStatement::new(
            "SELECT embedding FROM vec_pending_test_vector WHERE subject_id=?1",
            vec![SqlValue::Text(id.to_string())],
        ))
        .await
        .unwrap()
        .unwrap();
    match value {
        SqlValue::Blob(bytes) => bytes,
        other => panic!("expected vector BLOB, got {other:?}"),
    }
}

fn change(name: &str) -> EntityPatch {
    EntityPatch {
        name: Some(name.into()),
        ..Default::default()
    }
}

fn fail_vector(rt: &KhiveRuntime) {
    schema(
        rt,
        "CREATE TRIGGER fail_pending_vector BEFORE INSERT ON ann_write_log \
        WHEN NEW.embedding_model='pending-test-vector' AND NEW.op='upsert' \
        BEGIN SELECT RAISE(ABORT, 'forced vector publication failure'); END",
    );
}

#[tokio::test]
async fn t1_vector_failure_commits_entity_and_names_durable_unfinished_target() {
    let (rt, token, before) = fixture().await;
    let old_blob = vector_blob(&rt, before.id).await;
    fail_vector(&rt);
    let (entity, report) = rt
        .update_entity_with_index_report(&token, before.id, change("a different entity body"), None)
        .await
        .unwrap();
    assert_eq!(entity.version, before.version + 1);
    assert_eq!(
        rt.entities(&token)
            .unwrap()
            .get_entity(entity.id)
            .await
            .unwrap()
            .unwrap()
            .name,
        entity.name
    );
    let record = pending(&rt, entity.id)
        .await
        .expect("update must durably record work");
    assert_eq!(record.source_version, entity.version);
    assert_eq!(record.targets.len(), 2);
    assert!(
        record.targets[0].done,
        "healthy FTS publication must survive vector rollback"
    );
    assert!(!record.targets[1].done);
    assert_eq!(record.targets[1].attempts, 1);
    assert_eq!(
        record.targets[1].last_error.as_deref(),
        Some("index_publish")
    );
    assert_eq!(report.indexing.status, "unfinished");
    assert_eq!(report.indexing.unfinished.len(), 1);
    assert_eq!(report.indexing.unfinished[0].target, record.targets[1].kind);
    assert_eq!(report.indexing.unfinished[0].stage, "index_publish");
    assert_eq!(
        vector_blob(&rt, entity.id).await,
        old_blob,
        "failed target replaces nothing"
    );
    let mut reader = rt.sql().reader().await.unwrap();
    assert_eq!(
        reader
            .count(SqlStatement::new(
                "SELECT COUNT(*) FROM pending_entity_indexes WHERE entity_id=?1",
                vec![SqlValue::Text(entity.id.to_string())],
            ))
            .await
            .unwrap(),
        1
    );
    let fts = rt
        .text(&token)
        .unwrap()
        .get_document("local", entity.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fts.title.as_deref(), Some(entity.name.as_str()));
}

#[tokio::test]
async fn t2_pending_write_failure_rolls_back_entity_and_version() {
    let (rt, token, before) = fixture().await;
    schema(
        &rt,
        "CREATE TRIGGER fail_pending_record BEFORE INSERT ON pending_entity_indexes \
        BEGIN SELECT RAISE(ABORT, 'forced pending write failure'); END",
    );
    rt.update_entity_with_index_report(&token, before.id, change("must not commit"), None)
        .await
        .expect_err("entity and obligation must commit together");
    let after = rt
        .entities(&token)
        .unwrap()
        .get_entity(before.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.name, before.name);
    assert_eq!(after.version, before.version);
    assert_eq!(after.updated_at, before.updated_at);
    assert!(pending(&rt, before.id).await.is_none());
}

#[tokio::test]
async fn t4_success_replaces_vectors_and_fts_and_removes_pending_record() {
    let (rt, token, before) = fixture().await;
    let old_blob = vector_blob(&rt, before.id).await;
    let (entity, report) = rt
        .update_entity_with_index_report(
            &token,
            before.id,
            change("new and longer entity name"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(report.indexing.status, "complete");
    assert!(report.indexing.unfinished.is_empty());
    assert!(
        pending(&rt, entity.id).await.is_none(),
        "last publication removes the obligation"
    );
    assert_ne!(vector_blob(&rt, entity.id).await, old_blob);
    let doc = rt
        .text(&token)
        .unwrap()
        .get_document("local", entity.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(doc.title.as_deref(), Some(entity.name.as_str()));
    assert_eq!(doc.body, super::super::entity_fts_document(&entity).body);
    let mut reader = rt.sql().reader().await.unwrap();
    assert_eq!(
        reader
            .count(SqlStatement::new(
                "SELECT COUNT(*) FROM ann_write_log WHERE subject_id=?1 AND op='upsert'",
                vec![SqlValue::Text(entity.id.to_string())],
            ))
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn t5_live_version_moves_before_publication_and_nothing_is_consumed() {
    let (runtime, token, before) = fixture().await;
    let rt = Arc::new(runtime);
    let barrier = Arc::new((tokio::sync::Barrier::new(2), tokio::sync::Barrier::new(2)));
    let update_rt = Arc::clone(&rt);
    let update_token = token.clone();
    let id = before.id;
    let update = tokio::spawn(super::super::race_seam::BEFORE_ENTITY_INDEX_PUBLISH.scope(
        Arc::clone(&barrier),
        async move {
            update_rt
                .update_entity_with_index_report(&update_token, id, change("raced update"), None)
                .await
        },
    ));
    tokio::time::timeout(std::time::Duration::from_secs(10), barrier.0.wait())
        .await
        .expect("update reached FTS publication boundary");
    let original_pending = pending(&rt, id).await.unwrap();
    // A property-only update advances the row version without creating new work.
    let (_, property_report) = rt
        .update_entity_with_index_report(
            &token,
            id,
            EntityPatch {
                properties: Some(serde_json::json!({"new": "metadata"})),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(property_report.indexing.status, "not_requested");
    barrier.1.wait().await;
    let (_, report) = update.await.unwrap().unwrap();
    assert!(report
        .indexing
        .unfinished
        .iter()
        .all(|failure| failure.stage == "superseded"));
    assert_eq!(report.indexing.unfinished.len(), 2);
    let retained = pending(&rt, id).await.unwrap();
    assert_eq!(retained, original_pending);
    assert!(retained.targets.iter().all(|target| !target.done));
    assert_eq!(
        rt.text(&token)
            .unwrap()
            .get_document("local", id)
            .await
            .unwrap()
            .unwrap()
            .title
            .as_deref(),
        Some(before.name.as_str())
    );
}

#[tokio::test]
async fn t7_missing_or_mismatched_snapshot_model_remains_blocked() {
    let (rt, token, before) = fixture().await;
    fail_vector(&rt);
    let (entity, _) = rt
        .update_entity_with_index_report(&token, before.id, change("blocked target"), None)
        .await
        .unwrap();
    schema(&rt, "DROP TRIGGER fail_pending_vector");
    let record = pending(&rt, entity.id).await.unwrap();
    for (models, stage) in [
        (vec![], "blocked_model_missing"),
        (vec![(MODEL.into(), 7)], "blocked_dimensions"),
    ] {
        let report = rt
            .publish_pending_entity_indexes(&token, &entity, &record, &models)
            .await;
        assert_eq!(report.indexing.unfinished.len(), 1);
        assert_eq!(report.indexing.unfinished[0].stage, stage);
        let after = pending(&rt, entity.id).await.unwrap();
        assert_eq!(after.generation, record.generation);
        assert!(!after.targets[1].done);
        assert_eq!(after.targets[1].kind, record.targets[1].kind);
    }
}

#[tokio::test]
async fn property_and_tag_only_updates_create_no_pending_obligation() {
    let (rt, token, before) = fixture().await;
    let (_, report) = rt
        .update_entity_with_index_report(
            &token,
            before.id,
            EntityPatch {
                tags: Some(vec!["new-tag".into()]),
                properties: Some(serde_json::json!({"key": "value"})),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(report.indexing.status, "not_requested");
    assert!(pending(&rt, before.id).await.is_none());
}

#[tokio::test]
async fn legacy_update_returns_committed_non_retryable_pending_diagnostics() {
    let (rt, token, before) = fixture().await;
    fail_vector(&rt);
    let error = rt
        .update_entity_with_embedding_report(&token, before.id, change("committed legacy update"))
        .await
        .expect_err("legacy tuple must not hide unfinished targets");
    let RuntimeError::Khive(error) = error else {
        panic!("expected structured committed error");
    };
    let details = error.details().unwrap();
    assert_eq!(details.get("committed"), Some("true"));
    assert_eq!(details.get("retryable"), Some("false"));
    assert!(details
        .get("post_commit_degradations")
        .unwrap()
        .contains(MODEL));
    assert!(pending(&rt, before.id).await.is_some());
}

#[tokio::test]
async fn lost_publication_ack_remains_unknown_even_when_record_is_absent() {
    let rt = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let before = Entity::new("local", "concept", "before");
    rt.entities(&token)
        .unwrap()
        .upsert_entity(before.clone())
        .await
        .unwrap();
    let (entity, report) = UNKNOWN_AFTER_PUBLISH
        .scope(
            true,
            rt.update_entity_with_index_report(
                &token,
                before.id,
                change("committed but unacknowledged"),
                None,
            ),
        )
        .await
        .unwrap();
    assert_eq!(report.indexing.status, "unfinished");
    assert_eq!(report.indexing.unfinished.len(), 1);
    assert_eq!(report.indexing.unfinished[0].stage, "outcome_unknown");
    assert!(
        pending(&rt, entity.id).await.is_none(),
        "absence is not used as a completion receipt"
    );
    assert_eq!(
        rt.text(&token)
            .unwrap()
            .get_document("local", entity.id)
            .await
            .unwrap()
            .unwrap()
            .title
            .as_deref(),
        Some(entity.name.as_str())
    );
}
