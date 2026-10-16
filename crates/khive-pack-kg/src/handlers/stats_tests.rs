use super::*;
use async_trait::async_trait;
use khive_runtime::operations::entity_stats_counts;
use khive_runtime::{KhiveRuntime, Namespace};
use khive_storage::entity::EntityTypeCounts;
use khive_storage::types::{BatchWriteSummary, DeleteMode, Page, PageRequest, StorageResult};
use khive_storage::{Entity, EntityFilter, EntityStore, StorageCapability, StorageError};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use uuid::Uuid;

struct LegacyEntityStore {
    inner: Arc<dyn EntityStore>,
    scalar_reads: AtomicUsize,
}

#[async_trait]
impl EntityStore for LegacyEntityStore {
    async fn upsert_entity(&self, entity: Entity) -> StorageResult<()> {
        self.inner.upsert_entity(entity).await
    }
    async fn upsert_entities(&self, entities: Vec<Entity>) -> StorageResult<BatchWriteSummary> {
        self.inner.upsert_entities(entities).await
    }
    async fn get_entity(&self, id: Uuid) -> StorageResult<Option<Entity>> {
        self.inner.get_entity(id).await
    }
    async fn delete_entity(&self, id: Uuid, mode: DeleteMode) -> StorageResult<bool> {
        self.inner.delete_entity(id, mode).await
    }
    async fn query_entities(
        &self,
        namespace: &str,
        filter: EntityFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Entity>> {
        self.inner.query_entities(namespace, filter, page).await
    }
    async fn get_entity_including_deleted(&self, id: Uuid) -> StorageResult<Option<Entity>> {
        self.inner.get_entity_including_deleted(id).await
    }
    async fn count_entities(&self, namespace: &str, filter: EntityFilter) -> StorageResult<u64> {
        self.scalar_reads.fetch_add(1, Ordering::SeqCst);
        self.inner.count_entities(namespace, filter).await
    }
}

struct GroupedEntityStore {
    legacy: LegacyEntityStore,
    group_reads: AtomicUsize,
    late_entity: Option<Entity>,
    report_error: bool,
    groups_override: Option<EntityTypeCounts>,
}

#[async_trait]
impl EntityStore for GroupedEntityStore {
    async fn upsert_entity(&self, entity: Entity) -> StorageResult<()> {
        self.legacy.upsert_entity(entity).await
    }
    async fn upsert_entities(&self, entities: Vec<Entity>) -> StorageResult<BatchWriteSummary> {
        self.legacy.upsert_entities(entities).await
    }
    async fn get_entity(&self, id: Uuid) -> StorageResult<Option<Entity>> {
        self.legacy.get_entity(id).await
    }
    async fn delete_entity(&self, id: Uuid, mode: DeleteMode) -> StorageResult<bool> {
        self.legacy.delete_entity(id, mode).await
    }
    async fn query_entities(
        &self,
        namespace: &str,
        filter: EntityFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Entity>> {
        self.legacy.query_entities(namespace, filter, page).await
    }
    async fn get_entity_including_deleted(&self, id: Uuid) -> StorageResult<Option<Entity>> {
        self.legacy.get_entity_including_deleted(id).await
    }
    async fn count_entities(&self, namespace: &str, filter: EntityFilter) -> StorageResult<u64> {
        self.legacy.count_entities(namespace, filter).await
    }
    async fn count_entities_by_type(
        &self,
        namespaces: &[String],
    ) -> StorageResult<Option<EntityTypeCounts>> {
        self.group_reads.fetch_add(1, Ordering::SeqCst);
        if self.report_error {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Entities,
                operation: "entity_type_report_fixture".into(),
                message: "injected report failure".into(),
            });
        }
        let groups = match &self.groups_override {
            Some(groups) => Some(groups.clone()),
            None => self.legacy.inner.count_entities_by_type(namespaces).await?,
        };
        if let Some(entity) = &self.late_entity {
            self.legacy.inner.upsert_entity(entity.clone()).await?;
        }
        Ok(groups)
    }
}

fn fixture() -> (NamespaceToken, LegacyEntityStore) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let token = runtime
        .authorize_with_visibility(
            Namespace::local(),
            vec![Namespace::parse("lambda:stats-visible").unwrap()],
        )
        .unwrap();
    let store = LegacyEntityStore {
        inner: runtime.entities(&token).unwrap(),
        scalar_reads: AtomicUsize::new(0),
    };
    (token, store)
}

#[tokio::test]
async fn unavailable_entity_type_report_keeps_legacy_scalar_and_omits_wire_field() {
    let (token, store) = fixture();
    for namespace in [
        "local",
        "lambda:stats-visible",
        "lambda:stats-visible",
        "lambda:stats-hidden",
    ] {
        store
            .inner
            .upsert_entity(Entity::new(namespace, "concept", "LegacyReportFixture"))
            .await
            .unwrap();
    }
    let counts = entity_stats_counts(&store, &token)
        .await
        .expect("legacy reporting stays supported");
    assert_eq!(counts.entities, 3);
    let report_unavailable = counts.entities_by_type.is_none();
    assert_eq!(store.scalar_reads.load(Ordering::SeqCst), 1);
    let mut response = json!({"notes":0});
    add_entity_counts(&mut response, counts);
    assert_eq!(response["entities"], json!(3));
    assert!(
        response.get("entities_by_type").is_none(),
        "unavailable report is omitted, not fabricated empty"
    );
    assert!(
        report_unavailable,
        "the inherited default reports unavailable"
    );
}

#[tokio::test]
async fn supported_entity_stats_use_one_group_read_without_scalar_resampling() {
    let (token, legacy) = fixture();
    legacy
        .inner
        .upsert_entity(
            Entity::new("local", "concept", "BeforeSnapshot").with_entity_type(Some("algorithm")),
        )
        .await
        .unwrap();
    let store = GroupedEntityStore {
        legacy,
        group_reads: AtomicUsize::new(0),
        late_entity: Some(
            Entity::new("local", "concept", "AfterSnapshot")
                .with_entity_type(Some("after_snapshot")),
        ),
        report_error: false,
        groups_override: None,
    };
    let counts = entity_stats_counts(&store, &token).await.unwrap();
    let mut response = json!({});
    add_entity_counts(&mut response, counts);
    assert_eq!(
        response,
        json!({"entities":1,"entities_by_type":[
            {"entity_type":null,"count":0},{"entity_type":"algorithm","count":1}
        ]})
    );
    assert_eq!(store.group_reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        store.legacy.scalar_reads.load(Ordering::SeqCst),
        0,
        "a supported report must not sample the scalar after the grouped snapshot"
    );
    assert_eq!(
        store
            .legacy
            .inner
            .count_entities("local", EntityFilter::default())
            .await
            .unwrap(),
        2,
        "the later committed row distinguishes resampling from the held report"
    );
}

#[tokio::test]
async fn unordered_entity_type_report_is_rendered_in_null_first_byte_order() {
    let (token, legacy) = fixture();
    let store = GroupedEntityStore {
        legacy,
        group_reads: AtomicUsize::new(0),
        late_entity: None,
        report_error: false,
        groups_override: Some(vec![
            (Some("b".into()), 5),
            (Some("a".into()), 2),
            (None, 3),
        ]),
    };
    let counts = entity_stats_counts(&store, &token).await.unwrap();
    let mut response = json!({});
    add_entity_counts(&mut response, counts);
    assert_eq!(
        response,
        json!({"entities":10,"entities_by_type":[
            {"entity_type":null,"count":3},
            {"entity_type":"a","count":2},
            {"entity_type":"b","count":5}
        ]})
    );
    assert_eq!(store.group_reads.load(Ordering::SeqCst), 1);
    assert_eq!(store.legacy.scalar_reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn implemented_entity_report_error_propagates_without_scalar_fallback() {
    let (token, legacy) = fixture();
    legacy
        .inner
        .upsert_entity(Entity::new("local", "concept", "ErrorFallbackFixture"))
        .await
        .unwrap();
    let store = GroupedEntityStore {
        legacy,
        group_reads: AtomicUsize::new(0),
        late_entity: None,
        report_error: true,
        groups_override: None,
    };
    let error = entity_stats_counts(&store, &token)
        .await
        .expect_err("implemented report failure must propagate");
    assert!(matches!(
        error,
        RuntimeError::Storage(StorageError::InvalidInput { .. })
    ));
    assert_eq!(store.group_reads.load(Ordering::SeqCst), 1);
    assert_eq!(store.legacy.scalar_reads.load(Ordering::SeqCst), 0);
}
