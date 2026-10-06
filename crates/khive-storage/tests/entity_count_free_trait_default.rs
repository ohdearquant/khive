//! An `EntityStore` backend that does not implement count-free entity pages
//! must refuse them with `StorageError::Unsupported` for the Entities
//! capability, and must not answer through the exact query or a count: the
//! count-free operation exists to avoid the exact total those paths compute.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use uuid::Uuid;

use khive_storage::types::{BatchWriteSummary, DeleteMode, Page, PageRequest, StorageResult};
use khive_storage::{Entity, EntityFilter, EntityStore, StorageCapability, StorageError};

/// Implements only the methods without a trait default, and records every
/// call to the exact query and the count.
#[derive(Default)]
struct TraitDefaultOnlyEntityStore {
    exact_queries: AtomicUsize,
    counts: AtomicUsize,
}

#[async_trait]
impl EntityStore for TraitDefaultOnlyEntityStore {
    async fn upsert_entity(&self, _entity: Entity) -> StorageResult<()> {
        Ok(())
    }
    async fn upsert_entities(&self, _entities: Vec<Entity>) -> StorageResult<BatchWriteSummary> {
        Ok(BatchWriteSummary::default())
    }
    async fn get_entity(&self, _id: Uuid) -> StorageResult<Option<Entity>> {
        Ok(None)
    }
    async fn delete_entity(&self, _id: Uuid, _mode: DeleteMode) -> StorageResult<bool> {
        Ok(false)
    }
    async fn query_entities(
        &self,
        _namespace: &str,
        _filter: EntityFilter,
        _page: PageRequest,
    ) -> StorageResult<Page<Entity>> {
        self.exact_queries.fetch_add(1, Ordering::SeqCst);
        Ok(Page {
            items: Vec::new(),
            total: Some(0),
        })
    }
    async fn count_entities(&self, _namespace: &str, _filter: EntityFilter) -> StorageResult<u64> {
        self.counts.fetch_add(1, Ordering::SeqCst);
        Ok(0)
    }
    async fn get_entity_including_deleted(&self, _id: Uuid) -> StorageResult<Option<Entity>> {
        Ok(None)
    }
}

#[tokio::test]
async fn count_free_entity_page_default_refuses_without_exact_fallback() {
    let store = TraitDefaultOnlyEntityStore::default();
    let error = store
        .query_entities_count_free("local", EntityFilter::default(), PageRequest::default())
        .await
        .expect_err("a backend without count-free pages must report Unsupported");
    assert!(
        matches!(
            &error,
            StorageError::Unsupported {
                capability: StorageCapability::Entities,
                operation,
                ..
            } if operation == "query_entities_count_free"
        ),
        "unexpected error: {error:?}"
    );
    assert_eq!(store.exact_queries.load(Ordering::SeqCst), 0);
    assert_eq!(store.counts.load(Ordering::SeqCst), 0);
}
