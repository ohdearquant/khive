use super::super::StorageBackend;
use super::map_open_error;
use async_trait::async_trait;
use khive_storage::entity::{Entity, EntityFilter, EntityStore, EntityTypeCounts};
use khive_storage::{
    Attachment, BatchWriteSummary, DeleteMode, Page, PageRequest, SeekCursor, SeekPage,
    StorageCapability, StorageResult,
};
use uuid::Uuid;

#[async_trait]
impl EntityStore for StorageBackend {
    async fn upsert_entity(&self, entity: Entity) -> StorageResult<()> {
        self.entities()
            .map_err(|error| map_open_error(error, StorageCapability::Entities, "entities"))?
            .upsert_entity(entity)
            .await
    }

    async fn insert_entity_if_absent(&self, _entity: Entity) -> StorageResult<bool> {
        self.entities()
            .map_err(|error| map_open_error(error, StorageCapability::Entities, "entities"))?
            .insert_entity_if_absent(_entity)
            .await
    }

    async fn upsert_entity_with_attachments(
        &self,
        _entity: Entity,
        _attachments: Vec<Attachment>,
    ) -> StorageResult<()> {
        self.entities()
            .map_err(|error| map_open_error(error, StorageCapability::Entities, "entities"))?
            .upsert_entity_with_attachments(_entity, _attachments)
            .await
    }

    async fn upsert_entities(&self, entities: Vec<Entity>) -> StorageResult<BatchWriteSummary> {
        self.entities()
            .map_err(|error| map_open_error(error, StorageCapability::Entities, "entities"))?
            .upsert_entities(entities)
            .await
    }

    async fn replace_entity_if_unchanged(
        &self,
        _entity: Entity,
        _expected_updated_at: i64,
        _expected_deleted_at: Option<i64>,
    ) -> StorageResult<bool> {
        self.entities()
            .map_err(|error| map_open_error(error, StorageCapability::Entities, "entities"))?
            .replace_entity_if_unchanged(_entity, _expected_updated_at, _expected_deleted_at)
            .await
    }

    async fn get_entity(&self, id: Uuid) -> StorageResult<Option<Entity>> {
        self.entities()
            .map_err(|error| map_open_error(error, StorageCapability::Entities, "entities"))?
            .get_entity(id)
            .await
    }

    async fn delete_entity(&self, id: Uuid, mode: DeleteMode) -> StorageResult<bool> {
        self.entities()
            .map_err(|error| map_open_error(error, StorageCapability::Entities, "entities"))?
            .delete_entity(id, mode)
            .await
    }

    async fn query_entities(
        &self,
        namespace: &str,
        filter: EntityFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Entity>> {
        self.entities()
            .map_err(|error| map_open_error(error, StorageCapability::Entities, "entities"))?
            .query_entities(namespace, filter, page)
            .await
    }

    async fn query_entities_count_free(
        &self,
        _namespace: &str,
        _filter: EntityFilter,
        _page: PageRequest,
    ) -> StorageResult<Page<Entity>> {
        self.entities()
            .map_err(|error| map_open_error(error, StorageCapability::Entities, "entities"))?
            .query_entities_count_free(_namespace, _filter, _page)
            .await
    }

    async fn entity_sequence(&self, _id: Uuid) -> StorageResult<Option<i64>> {
        self.entities()
            .map_err(|error| map_open_error(error, StorageCapability::Entities, "entities"))?
            .entity_sequence(_id)
            .await
    }

    async fn query_entities_after(
        &self,
        _namespace: &str,
        _filter: EntityFilter,
        _after: Option<SeekCursor>,
        _limit: u32,
    ) -> StorageResult<SeekPage<Entity>> {
        self.entities()
            .map_err(|error| map_open_error(error, StorageCapability::Entities, "entities"))?
            .query_entities_after(_namespace, _filter, _after, _limit)
            .await
    }

    async fn count_entities(&self, namespace: &str, filter: EntityFilter) -> StorageResult<u64> {
        self.entities()
            .map_err(|error| map_open_error(error, StorageCapability::Entities, "entities"))?
            .count_entities(namespace, filter)
            .await
    }

    async fn count_entities_by_type(
        &self,
        _namespaces: &[String],
    ) -> StorageResult<Option<EntityTypeCounts>> {
        self.entities()
            .map_err(|error| map_open_error(error, StorageCapability::Entities, "entities"))?
            .count_entities_by_type(_namespaces)
            .await
    }

    async fn get_entity_including_deleted(&self, id: Uuid) -> StorageResult<Option<Entity>> {
        self.entities()
            .map_err(|error| map_open_error(error, StorageCapability::Entities, "entities"))?
            .get_entity_including_deleted(id)
            .await
    }
}
