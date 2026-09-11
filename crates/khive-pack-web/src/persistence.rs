use std::any::Any;
use std::collections::HashSet;

use khive_db::stores::entity::{entity_insert_statement, entity_replace_if_unchanged_statement};
use khive_db::stores::graph::{
    edge_insert_only_guarded_by_endpoints_statement, edge_replace_if_unchanged_statement,
};
use khive_db::stores::text::{delete_document_statement, insert_document_statements};
use khive_pack_kg as _;
use khive_runtime::{
    entity_fts_document, EntityCreateSpec, KhiveRuntime, LinkSpec, NamespaceToken, PackRegistry,
    RuntimeConfig, RuntimeError, VerbRegistryBuilder,
};
use khive_storage::entity::Entity;
use khive_storage::{EdgeRelation, SqlStatement, StorageError};
use uuid::Uuid;

const MAX_ENTITIES: usize = 10_000;
const MAX_EDGES: usize = 50_000;

#[derive(Clone, Debug)]
pub(crate) struct WebEntity {
    pub id: Uuid,
    pub spec: EntityCreateSpec,
}

#[derive(Clone, Debug)]
pub(crate) struct WebEdge {
    pub id: Uuid,
    pub source: Uuid,
    pub target: Uuid,
    pub relation: EdgeRelation,
}

fn staging_runtime() -> Result<KhiveRuntime, RuntimeError> {
    let packs = vec!["kg".to_owned(), "web".to_owned()];
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: packs.clone(),
        events_split: None,
        ..RuntimeConfig::no_embeddings()
    })?;
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(&packs, runtime.clone(), &mut builder)
        .map_err(|error| RuntimeError::Internal(format!("web staging registry: {error}")))?;
    let registry = builder
        .build()
        .map_err(|error| RuntimeError::Internal(format!("web staging registry: {error}")))?;
    runtime.install_edge_rules(registry.all_edge_rules());
    registry.call_register_entity_type_validators(&runtime);
    runtime.install_kind_registry(
        registry
            .all_entity_kinds()
            .into_iter()
            .map(str::to_owned)
            .collect(),
        registry
            .all_note_kinds()
            .into_iter()
            .map(str::to_owned)
            .collect(),
    );
    Ok(runtime)
}

pub(crate) async fn persist(
    target: &KhiveRuntime,
    token: &NamespaceToken,
    entities: Vec<WebEntity>,
    edges: Vec<WebEdge>,
) -> Result<(), RuntimeError> {
    commit_statements(target, prepare(target, token, entities, edges).await?).await
}

async fn prepare(
    target: &KhiveRuntime,
    token: &NamespaceToken,
    entities: Vec<WebEntity>,
    edges: Vec<WebEdge>,
) -> Result<Vec<(SqlStatement, bool)>, RuntimeError> {
    if entities.len() > MAX_ENTITIES || edges.len() > MAX_EDGES {
        return Err(RuntimeError::InvalidInput(format!(
            "web ingest batch exceeds {MAX_ENTITIES} entities or {MAX_EDGES} edges"
        )));
    }
    let mut entity_ids = HashSet::with_capacity(entities.len());
    for entity in &entities {
        if !entity_ids.insert(entity.id) {
            return Err(RuntimeError::InvalidInput(format!(
                "duplicate web entity id {}",
                entity.id
            )));
        }
    }
    let mut edge_ids = HashSet::with_capacity(edges.len());
    let mut triples = HashSet::with_capacity(edges.len());
    for edge in &edges {
        if !edge_ids.insert(edge.id) || entity_ids.contains(&edge.id) {
            return Err(RuntimeError::InvalidInput(format!(
                "duplicate web record id {}",
                edge.id
            )));
        }
        if !triples.insert((edge.source, edge.target, edge.relation)) {
            return Err(RuntimeError::InvalidInput(
                "duplicate web edge triple".into(),
            ));
        }
    }

    let staging = staging_runtime()?;
    let (ids, specs): (Vec<_>, Vec<_>) = entities.into_iter().map(|e| (e.id, e.spec)).unzip();
    // Only the isolated runtime receives the create path's temporary UUIDs.
    let validated = staging.create_many(token, specs).await?;
    let stage_store = staging.entities(token)?;
    let mut statements = Vec::new();
    let mut staged = entity_ids;
    for (id, mut entity) in ids.into_iter().zip(validated) {
        entity.id = id;
        let existing = target.get_entity_including_deleted(token, id).await?;
        let write = if let Some(existing) = &existing {
            if existing.deleted_at.is_some() || existing.merged_into.is_some() {
                return Err(RuntimeError::InvalidInput(format!(
                    "web entity {id} is deleted or merged; ingest cannot restore it"
                )));
            }
            entity.namespace.clone_from(&existing.namespace);
            entity.created_at = existing.created_at;
            entity.content_ref.clone_from(&existing.content_ref);
            if same_entity_content(existing, &entity) {
                entity.updated_at = existing.updated_at;
                None
            } else {
                entity.updated_at =
                    entity
                        .updated_at
                        .max(existing.updated_at.checked_add(1).ok_or_else(|| {
                            RuntimeError::InvalidInput("entity revision overflow".into())
                        })?);
                Some(entity_replace_if_unchanged_statement(
                    &entity,
                    existing.updated_at,
                    existing.deleted_at,
                ))
            }
        } else {
            Some(entity_insert_statement(&entity))
        };
        if let Some(write) = write {
            statements.push((write, true));
            statements.push((
                delete_document_statement("fts_entities", &entity.namespace, entity.id),
                false,
            ));
            statements.extend(
                insert_document_statements("fts_entities", &entity_fts_document(&entity))
                    .into_iter()
                    .map(|statement| (statement, false)),
            );
        }
        stage_store.upsert_entity(entity).await?;
    }

    for input in edges {
        for endpoint in [input.source, input.target] {
            if staged.insert(endpoint) {
                let entity = target.get_entity(token, endpoint).await?;
                stage_store.upsert_entity(entity).await?;
            }
        }
        let mut edge = staging
            .build_edge(
                token,
                &LinkSpec {
                    namespace: None,
                    source_id: input.source,
                    target_id: input.target,
                    relation: input.relation,
                    weight: 1.0,
                    metadata: None,
                },
            )
            .await?;
        edge.id = input.id.into();
        let existing = target.get_edge_including_deleted(token, input.id).await?;
        if let Some(existing) = &existing {
            if existing.deleted_at.is_some() {
                return Err(RuntimeError::InvalidInput(format!(
                    "web edge {} is deleted; ingest cannot restore it",
                    input.id
                )));
            }
            edge.namespace.clone_from(&existing.namespace);
            edge.created_at = existing.created_at;
        }
        if let Some(conflict) = target
            .get_edge_by_natural_key_including_deleted(
                token,
                &edge.namespace,
                edge.source_id,
                edge.target_id,
                edge.relation,
            )
            .await?
        {
            if conflict.id != edge.id {
                return Err(RuntimeError::InvalidInput(format!(
                    "web edge {} conflicts with an existing edge identity",
                    input.id
                )));
            }
        }
        if let Some(existing) = existing {
            if existing.source_id == edge.source_id
                && existing.target_id == edge.target_id
                && existing.relation == edge.relation
                && existing.weight == edge.weight
                && existing.metadata == edge.metadata
                && existing.target_backend == edge.target_backend
            {
                continue;
            }
            edge.updated_at = edge.updated_at.max(
                existing
                    .updated_at
                    .checked_add_signed(chrono::Duration::microseconds(1))
                    .ok_or_else(|| RuntimeError::InvalidInput("edge revision overflow".into()))?,
            );
            statements.push((
                edge_replace_if_unchanged_statement(
                    &edge,
                    existing.updated_at,
                    existing.deleted_at,
                ),
                true,
            ));
        } else {
            statements.push((edge_insert_only_guarded_by_endpoints_statement(&edge), true));
        }
    }

    Ok(statements)
}

fn same_entity_content(a: &Entity, b: &Entity) -> bool {
    a.kind == b.kind
        && a.entity_type == b.entity_type
        && a.name == b.name
        && a.description == b.description
        && a.properties == b.properties
        && a.tags == b.tags
}

async fn commit_statements(
    target: &KhiveRuntime,
    statements: Vec<(SqlStatement, bool)>,
) -> Result<(), RuntimeError> {
    if statements.is_empty() {
        return Ok(());
    }
    // The transaction drives prepared DML only; validation and all reads happen above.
    target
        .sql()
        .atomic_unit(Box::new(move |writer| {
            Box::pin(async move {
                for (statement, exactly_one) in statements {
                    let label = statement.label.clone();
                    let affected = writer.execute(statement).await?;
                    if exactly_one && affected != 1 {
                        return Err(StorageError::Internal(format!(
                            "web ingest write guard failed for {label:?}: affected {affected} rows"
                        )));
                    }
                }
                Ok(Box::new(()) as Box<dyn Any + Send>)
            })
        }))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::Namespace;
    use khive_storage::{EntityFilter, TextFilter, TextQueryMode, TextSearchRequest};

    fn entity(id: u128, kind: &str, entity_type: &str, name: &str) -> WebEntity {
        WebEntity {
            id: Uuid::from_u128(id),
            spec: EntityCreateSpec {
                kind: kind.into(),
                entity_type: Some(entity_type.into()),
                name: name.into(),
                description: None,
                properties: None,
                tags: vec![],
            },
        }
    }

    fn batch() -> (Vec<WebEntity>, Vec<WebEdge>) {
        (
            vec![
                entity(1, "service", "site", "Fictional observatory"),
                entity(2, "document", "page", "Celestial catalog"),
            ],
            vec![WebEdge {
                id: Uuid::from_u128(3),
                source: Uuid::from_u128(1),
                target: Uuid::from_u128(2),
                relation: EdgeRelation::Contains,
            }],
        )
    }

    async fn assert_empty(runtime: &KhiveRuntime, token: &NamespaceToken) {
        assert_eq!(
            runtime
                .entities(token)
                .unwrap()
                .count_entities("local", EntityFilter::default())
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            runtime
                .text(token)
                .unwrap()
                .count(TextFilter::default())
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            runtime
                .graph(token)
                .unwrap()
                .count_edges(Default::default())
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn deterministic_reingest_preserves_rows_and_updates_searchable_description() {
        let target = staging_runtime().unwrap();
        let token = target.authorize(Namespace::local()).unwrap();
        let (entities, edges) = batch();
        persist(&target, &token, entities.clone(), edges.clone())
            .await
            .unwrap();
        let before =
            serde_json::to_value(target.get_entity(&token, Uuid::from_u128(2)).await.unwrap())
                .unwrap();
        let edge_before =
            serde_json::to_value(target.get_edge(&token, Uuid::from_u128(3)).await.unwrap())
                .unwrap();
        let text_before = serde_json::to_value(
            target
                .text(&token)
                .unwrap()
                .get_document("local", Uuid::from_u128(2))
                .await
                .unwrap(),
        )
        .unwrap();
        persist(&target, &token, entities.clone(), edges.clone())
            .await
            .unwrap();
        assert_eq!(
            before,
            serde_json::to_value(target.get_entity(&token, Uuid::from_u128(2)).await.unwrap())
                .unwrap()
        );
        assert_eq!(
            edge_before,
            serde_json::to_value(target.get_edge(&token, Uuid::from_u128(3)).await.unwrap())
                .unwrap()
        );
        assert_eq!(
            text_before,
            serde_json::to_value(
                target
                    .text(&token)
                    .unwrap()
                    .get_document("local", Uuid::from_u128(2))
                    .await
                    .unwrap()
            )
            .unwrap()
        );
        let mut changed = entities;
        changed[1].spec.description = Some("Ultraviolet catalog observations".into());
        persist(&target, &token, changed, edges).await.unwrap();
        let after = target.get_entity(&token, Uuid::from_u128(2)).await.unwrap();
        assert_eq!(before["id"], after.id.to_string());
        assert_eq!(before["created_at"], after.created_at);
        assert!(after.updated_at > before["updated_at"].as_i64().unwrap());
        assert_eq!(
            target
                .entities(&token)
                .unwrap()
                .count_entities("local", EntityFilter::default())
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            target
                .text(&token)
                .unwrap()
                .count(TextFilter::default())
                .await
                .unwrap(),
            2
        );
        let hits = target
            .text(&token)
            .unwrap()
            .search(TextSearchRequest {
                query: "Ultraviolet".into(),
                mode: TextQueryMode::Plain,
                filter: None,
                top_k: 10,
                snippet_chars: 100,
            })
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].subject_id, after.id);
    }

    #[tokio::test]
    async fn all_entity_validation_precedes_target_writes() {
        let target = staging_runtime().unwrap();
        let token = target.authorize(Namespace::local()).unwrap();
        for invalid in [
            entity(4, "unknown_kind", "page", "Invalid kind"),
            entity(4, "document", "not_a_registered_type", "Invalid type"),
            entity(4, "document", "page", "  "),
        ] {
            let (mut entities, edges) = batch();
            entities.push(invalid);
            assert!(persist(&target, &token, entities, edges).await.is_err());
            assert_empty(&target, &token).await;
        }
        let (mut entities, edges) = batch();
        entities[1].spec.properties = Some(serde_json::json!({"khive:secret_gate": true}));
        assert!(persist(&target, &token, entities, edges).await.is_err());
        assert_empty(&target, &token).await;
        let (mut entities, edges) = batch();
        entities[1].spec.description = Some(format!("ghp_{}", "A".repeat(36)));
        assert!(persist(&target, &token, entities, edges).await.is_err());
        assert_empty(&target, &token).await;
    }

    #[tokio::test]
    async fn invalid_edge_and_duplicate_ids_leave_no_rows() {
        let target = staging_runtime().unwrap();
        let token = target.authorize(Namespace::local()).unwrap();
        let (entities, mut edges) = batch();
        edges[0].relation = EdgeRelation::Supports;
        assert!(persist(&target, &token, entities, edges).await.is_err());
        assert_empty(&target, &token).await;
        let (mut entities, edges) = batch();
        entities.push(entities[0].clone());
        assert!(persist(&target, &token, entities, edges).await.is_err());
        assert_empty(&target, &token).await;
    }

    #[tokio::test]
    async fn prepared_entity_insert_refuses_a_competing_live_or_deleted_id() {
        for deleted in [false, true] {
            let target = staging_runtime().unwrap();
            let token = target.authorize(Namespace::local()).unwrap();
            let (entities, edges) = batch();
            let statements = prepare(&target, &token, entities, edges).await.unwrap();
            let mut winner = Entity::new("local", "document", "Competing catalog")
                .with_entity_type(Some("page"));
            winner.id = Uuid::from_u128(2);
            winner.created_at = 11;
            winner.updated_at = 12;
            winner.deleted_at = deleted.then_some(13);
            target
                .entities(&token)
                .unwrap()
                .upsert_entity(winner.clone())
                .await
                .unwrap();
            if !deleted {
                target
                    .text(&token)
                    .unwrap()
                    .upsert_document(entity_fts_document(&winner))
                    .await
                    .unwrap();
            }
            let before = serde_json::to_value(&winner).unwrap();
            let text_before = serde_json::to_value(
                target
                    .text(&token)
                    .unwrap()
                    .get_document("local", winner.id)
                    .await
                    .unwrap(),
            )
            .unwrap();

            assert!(commit_statements(&target, statements).await.is_err());

            assert_eq!(
                before,
                serde_json::to_value(
                    target
                        .get_entity_including_deleted(&token, winner.id,)
                        .await
                        .unwrap()
                        .unwrap()
                )
                .unwrap()
            );
            assert_eq!(
                text_before,
                serde_json::to_value(
                    target
                        .text(&token)
                        .unwrap()
                        .get_document("local", winner.id)
                        .await
                        .unwrap()
                )
                .unwrap()
            );
            assert!(target
                .get_entity_including_deleted(&token, Uuid::from_u128(1))
                .await
                .unwrap()
                .is_none());
            assert!(target
                .text(&token)
                .unwrap()
                .get_document("local", Uuid::from_u128(1))
                .await
                .unwrap()
                .is_none());
            assert!(target
                .get_edge_including_deleted(&token, Uuid::from_u128(3))
                .await
                .unwrap()
                .is_none());
        }
    }

    #[tokio::test]
    async fn prepared_edge_insert_refuses_competing_ids_and_natural_keys() {
        for same_id in [false, true] {
            for deleted in [false, true] {
                let target = staging_runtime().unwrap();
                let token = target.authorize(Namespace::local()).unwrap();
                let (mut entities, edges) = batch();
                entities.push(entity(4, "document", "page", "Competing page"));
                persist(&target, &token, entities, vec![]).await.unwrap();
                let statements = prepare(
                    &target,
                    &token,
                    vec![entity(5, "document", "page", "Uncommitted page")],
                    edges,
                )
                .await
                .unwrap();

                let mut winner = target
                    .build_edge(
                        &token,
                        &LinkSpec {
                            namespace: None,
                            source_id: Uuid::from_u128(1),
                            target_id: Uuid::from_u128(if same_id { 4 } else { 2 }),
                            relation: EdgeRelation::Contains,
                            weight: 0.75,
                            metadata: Some(serde_json::json!({"winner": "competing"})),
                        },
                    )
                    .await
                    .unwrap();
                let winner_id = Uuid::from_u128(if same_id { 3 } else { 99 });
                winner.id = winner_id.into();
                winner.created_at = chrono::DateTime::from_timestamp_micros(11).unwrap();
                winner.updated_at = chrono::DateTime::from_timestamp_micros(12).unwrap();
                winner.deleted_at =
                    deleted.then_some(chrono::DateTime::from_timestamp_micros(13).unwrap());
                target
                    .graph(&token)
                    .unwrap()
                    .upsert_edge(winner.clone())
                    .await
                    .unwrap();
                let before = serde_json::to_value(&winner).unwrap();

                assert!(commit_statements(&target, statements).await.is_err());

                assert_eq!(
                    before,
                    serde_json::to_value(
                        target
                            .get_edge_including_deleted(&token, winner_id,)
                            .await
                            .unwrap()
                            .unwrap()
                    )
                    .unwrap()
                );
                assert!(target
                    .get_entity_including_deleted(&token, Uuid::from_u128(5))
                    .await
                    .unwrap()
                    .is_none());
                assert!(target
                    .text(&token)
                    .unwrap()
                    .get_document("local", Uuid::from_u128(5))
                    .await
                    .unwrap()
                    .is_none());
                assert_eq!(
                    target
                        .entities(&token)
                        .unwrap()
                        .count_entities("local", EntityFilter::default())
                        .await
                        .unwrap(),
                    3
                );
                assert_eq!(
                    target
                        .text(&token)
                        .unwrap()
                        .count(TextFilter::default())
                        .await
                        .unwrap(),
                    3
                );
                if !same_id {
                    assert!(target
                        .get_edge_including_deleted(&token, Uuid::from_u128(3))
                        .await
                        .unwrap()
                        .is_none());
                }
            }
        }
    }

    #[tokio::test]
    async fn prepared_edge_insert_refuses_an_endpoint_deleted_before_commit() {
        let target = staging_runtime().unwrap();
        let token = target.authorize(Namespace::local()).unwrap();
        let (entities, edges) = batch();
        persist(&target, &token, entities, vec![]).await.unwrap();
        let statements = prepare(
            &target,
            &token,
            vec![entity(5, "document", "page", "Uncommitted page")],
            edges,
        )
        .await
        .unwrap();
        target
            .entities(&token)
            .unwrap()
            .delete_entity(Uuid::from_u128(2), khive_storage::DeleteMode::Soft)
            .await
            .unwrap();

        assert!(commit_statements(&target, statements).await.is_err());

        assert!(target
            .get_entity_including_deleted(&token, Uuid::from_u128(5))
            .await
            .unwrap()
            .is_none());
        assert!(target
            .text(&token)
            .unwrap()
            .get_document("local", Uuid::from_u128(5))
            .await
            .unwrap()
            .is_none());
        assert!(target
            .get_edge_including_deleted(&token, Uuid::from_u128(3))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn transaction_rolls_back_entity_and_fts_if_a_later_statement_fails() {
        let target = staging_runtime().unwrap();
        let token = target.authorize(Namespace::local()).unwrap();
        let entity = Entity::new("local", "document", "Fictional rollback catalog");
        let document = entity_fts_document(&entity);
        let mut statements = vec![(entity_insert_statement(&entity), true)];
        statements.extend(
            insert_document_statements("fts_entities", &document)
                .into_iter()
                .map(|s| (s, false)),
        );
        statements.extend(
            insert_document_statements("fts_web_nonexistent_test_table", &document)
                .into_iter()
                .map(|s| (s, false)),
        );
        assert!(commit_statements(&target, statements).await.is_err());
        assert_empty(&target, &token).await;
    }
}
