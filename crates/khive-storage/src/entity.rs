//! Entity storage capability — graph node CRUD.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::types::{BatchWriteSummary, DeleteMode, Page, PageRequest, StorageResult};

/// Storage-level entity record. Flat SQL-friendly representation.
/// Maps to the `entities` substrate table.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entity {
    pub id: Uuid,
    pub namespace: String,
    pub kind: String,
    /// Pack-governed subtype token. Maps to `entities.entity_type` column.
    pub entity_type: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub properties: Option<Value>,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
    /// When this entity was tombstoned by a merge, the `into` entity's ID.
    pub merged_into: Option<Uuid>,
    /// Opaque event ID for the merge that tombstoned this entity.
    pub merge_event_id: Option<Uuid>,
}

impl Entity {
    /// Create a new entity with a generated UUID and current timestamp.
    pub fn new(
        namespace: impl Into<String>,
        kind: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        let now = chrono::Utc::now().timestamp_micros();
        Self {
            id: Uuid::new_v4(),
            namespace: namespace.into(),
            kind: kind.into(),
            entity_type: None,
            name: name.into(),
            description: None,
            properties: None,
            tags: Vec::new(),
            created_at: now,
            updated_at: now,
            deleted_at: None,
            merged_into: None,
            merge_event_id: None,
        }
    }

    /// Set the pack-governed entity subtype token.
    pub fn with_entity_type(mut self, t: Option<impl Into<String>>) -> Self {
        self.entity_type = t.map(Into::into);
        self
    }

    /// Set the entity description.
    pub fn with_description(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }

    /// Set the entity properties JSON blob.
    pub fn with_properties(mut self, p: Value) -> Self {
        self.properties = Some(p);
        self
    }

    /// Set the entity tags.
    pub fn with_tags(mut self, t: Vec<String>) -> Self {
        self.tags = t;
        self
    }
}

/// Entity filter for query operations.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EntityFilter {
    pub ids: Vec<Uuid>,
    pub kinds: Vec<String>,
    /// Filter by exact `entity_type` value. Multiple values are ORed.
    pub entity_types: Vec<String>,
    pub name_prefix: Option<String>,
    pub tags_any: Vec<String>,
    /// When non-empty, restricts results to any of these namespaces using
    /// `namespace IN (...)`. Takes precedence over the `namespace` string
    /// parameter passed to `query_entities` / `count_entities`. When empty the
    /// caller-supplied `namespace` parameter is used (single-namespace path,
    /// backward-compatible default).
    #[serde(default)]
    pub namespaces: Vec<String>,
}

/// Entity CRUD operations over the entities substrate table.
#[async_trait]
pub trait EntityStore: Send + Sync + 'static {
    /// Insert or update a single entity.
    async fn upsert_entity(&self, entity: Entity) -> StorageResult<()>;
    /// Insert or update a batch of entities.
    async fn upsert_entities(&self, entities: Vec<Entity>) -> StorageResult<BatchWriteSummary>;
    /// Fetch an entity by UUID, returning `None` if absent.
    async fn get_entity(&self, id: Uuid) -> StorageResult<Option<Entity>>;
    /// Delete an entity by UUID using the specified delete mode.
    async fn delete_entity(&self, id: Uuid, mode: DeleteMode) -> StorageResult<bool>;
    /// Query entities by namespace with filter and pagination.
    async fn query_entities(
        &self,
        namespace: &str,
        filter: EntityFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Entity>>;
    /// Count entities in a namespace matching the given filter.
    async fn count_entities(&self, namespace: &str, filter: EntityFilter) -> StorageResult<u64>;
    /// Fetch an entity by UUID regardless of soft-deletion state.
    ///
    /// Returns the entity row even when `deleted_at` is set. Callers use this
    /// to distinguish "soft-deleted" from "never existed".
    async fn get_entity_including_deleted(&self, id: Uuid) -> StorageResult<Option<Entity>>;
}
