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
    pub name: String,
    pub description: Option<String>,
    pub properties: Option<Value>,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
}

impl Entity {
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
            name: name.into(),
            description: None,
            properties: None,
            tags: Vec::new(),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        }
    }

    pub fn with_description(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }

    pub fn with_properties(mut self, p: Value) -> Self {
        self.properties = Some(p);
        self
    }

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
    pub name_prefix: Option<String>,
    pub tags_any: Vec<String>,
}

#[async_trait]
pub trait EntityStore: Send + Sync + 'static {
    async fn upsert_entity(&self, entity: Entity) -> StorageResult<()>;
    async fn upsert_entities(&self, entities: Vec<Entity>) -> StorageResult<BatchWriteSummary>;
    async fn get_entity(&self, id: Uuid) -> StorageResult<Option<Entity>>;
    async fn delete_entity(&self, id: Uuid, mode: DeleteMode) -> StorageResult<bool>;
    async fn query_entities(
        &self,
        namespace: &str,
        filter: EntityFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Entity>>;
    async fn count_entities(&self, namespace: &str, filter: EntityFilter) -> StorageResult<u64>;
}
