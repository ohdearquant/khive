//! High-level operations composing storage capabilities into user-facing verbs.

use std::str::FromStr;

use uuid::Uuid;

use khive_storage::note::{Note, NoteKind};
use khive_storage::types::{
    DeleteMode, Direction, EdgeSortField, GraphPath, LinkId, NeighborHit, NeighborQuery,
    PageRequest, SortOrder, SqlStatement, TextDocument, TraversalRequest,
};
use khive_storage::{Edge, EdgeRelation, Entity, EntityFilter};
use khive_types::{EntityKind, SubstrateKind};

use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::KhiveRuntime;

impl KhiveRuntime {
    // ---- Entity operations ----

    /// Create and persist a new entity.
    pub async fn create_entity(
        &self,
        namespace: Option<&str>,
        kind: &str,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
    ) -> RuntimeResult<Entity> {
        let ns = self.ns(namespace);
        let entity_kind = EntityKind::from_str(kind).map_err(RuntimeError::InvalidInput)?;
        let mut entity = Entity::new(ns, entity_kind, name);
        if let Some(d) = description {
            entity = entity.with_description(d);
        }
        if let Some(p) = properties {
            entity = entity.with_properties(p);
        }
        if !tags.is_empty() {
            entity = entity.with_tags(tags);
        }
        self.entities(Some(ns))?
            .upsert_entity(entity.clone())
            .await?;

        let body = match &entity.description {
            Some(d) if !d.is_empty() => format!("{} {}", entity.name, d),
            _ => entity.name.clone(),
        };
        self.text(namespace)?
            .upsert_document(TextDocument {
                subject_id: entity.id,
                kind: SubstrateKind::Entity,
                title: Some(entity.name.clone()),
                body: body.clone(),
                tags: entity.tags.clone(),
                namespace: ns.to_string(),
                metadata: entity.properties.clone(),
                updated_at: chrono::Utc::now(),
            })
            .await?;

        if self.config().embedding_model.is_some() {
            let vector = self.embed(&body).await?;
            self.vectors(namespace)?
                .insert(entity.id, SubstrateKind::Entity, ns, vector)
                .await?;
        }

        Ok(entity)
    }

    /// Retrieve an entity by ID.
    pub async fn get_entity(
        &self,
        namespace: Option<&str>,
        id: Uuid,
    ) -> RuntimeResult<Option<Entity>> {
        Ok(self.entities(namespace)?.get_entity(id).await?)
    }

    /// List entities in a namespace, optionally filtered by kind.
    pub async fn list_entities(
        &self,
        namespace: Option<&str>,
        kind: Option<&str>,
        limit: u32,
    ) -> RuntimeResult<Vec<Entity>> {
        let filter = EntityFilter {
            kinds: kind
                .map(|k| vec![EntityKind::from_str(k).unwrap_or_default()])
                .unwrap_or_default(),
            ..Default::default()
        };
        let page = self
            .entities(namespace)?
            .query_entities(self.ns(namespace), filter, PageRequest { offset: 0, limit })
            .await?;
        Ok(page.items)
    }

    // ---- Edge operations ----

    /// Create a directed edge between two entities.
    pub async fn link(
        &self,
        namespace: Option<&str>,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
        weight: f64,
    ) -> RuntimeResult<Edge> {
        let edge = Edge {
            id: LinkId::from(Uuid::new_v4()),
            source_id,
            target_id,
            relation,
            weight,
            created_at: chrono::Utc::now(),
            metadata: None,
        };
        self.graph(namespace)?.upsert_edge(edge.clone()).await?;
        Ok(edge)
    }

    /// Get immediate neighbors of a node.
    pub async fn neighbors(
        &self,
        namespace: Option<&str>,
        node_id: Uuid,
        direction: Direction,
        limit: Option<u32>,
    ) -> RuntimeResult<Vec<NeighborHit>> {
        let query = NeighborQuery {
            direction,
            relations: None,
            limit,
            min_weight: None,
        };
        Ok(self.graph(namespace)?.neighbors(node_id, query).await?)
    }

    /// Traverse the graph from a set of root nodes.
    pub async fn traverse(
        &self,
        namespace: Option<&str>,
        request: TraversalRequest,
    ) -> RuntimeResult<Vec<GraphPath>> {
        Ok(self.graph(namespace)?.traverse(request).await?)
    }

    // ---- Note operations ----

    /// Create and persist a note.
    pub async fn create_note(
        &self,
        namespace: Option<&str>,
        kind: NoteKind,
        content: &str,
        salience: f64,
    ) -> RuntimeResult<Note> {
        let note = Note::new(self.ns(namespace), kind, content).with_salience(salience);
        self.notes(namespace)?.upsert_note(note.clone()).await?;
        Ok(note)
    }

    /// List notes, optionally filtered by kind.
    pub async fn list_notes(
        &self,
        namespace: Option<&str>,
        kind: Option<&str>,
        limit: u32,
    ) -> RuntimeResult<Vec<Note>> {
        let page = self
            .notes(namespace)?
            .query_notes(
                self.ns(namespace),
                kind.and_then(|k| NoteKind::from_str(k).ok()),
                PageRequest { offset: 0, limit },
            )
            .await?;
        Ok(page.items)
    }

    // ---- Query operations ----

    /// Execute a GQL or SPARQL query string, returning raw SQL rows.
    ///
    /// The query is compiled to SQL with the namespace scope applied.
    /// GQL syntax: `MATCH (a:concept)-[e:extends]->(b) RETURN a, b LIMIT 10`
    /// SPARQL syntax: `SELECT ?a WHERE { ?a :kind "concept" . }`
    pub async fn query(
        &self,
        namespace: Option<&str>,
        query: &str,
    ) -> RuntimeResult<Vec<khive_storage::types::SqlRow>> {
        let ns = self.ns(namespace);
        let ast = khive_query::parse_auto(query)?;
        let opts = khive_query::CompileOptions {
            scopes: vec![ns.to_string()],
            ..Default::default()
        };
        let compiled = khive_query::compile(&ast, &opts)?;
        let mut reader = self.sql().reader().await?;
        let stmt = SqlStatement {
            sql: compiled.sql,
            params: compiled.params,
            label: None,
        };
        Ok(reader.query_all(stmt).await?)
    }

    /// Delete an entity by ID (soft delete by default).
    pub async fn delete_entity(
        &self,
        namespace: Option<&str>,
        id: Uuid,
        hard: bool,
    ) -> RuntimeResult<bool> {
        let mode = if hard {
            DeleteMode::Hard
        } else {
            DeleteMode::Soft
        };
        Ok(self.entities(namespace)?.delete_entity(id, mode).await?)
    }

    /// Count entities in a namespace, optionally filtered.
    pub async fn count_entities(
        &self,
        namespace: Option<&str>,
        kind: Option<&str>,
    ) -> RuntimeResult<u64> {
        let filter = EntityFilter {
            kinds: kind
                .map(|k| vec![EntityKind::from_str(k).unwrap_or_default()])
                .unwrap_or_default(),
            ..Default::default()
        };
        Ok(self
            .entities(namespace)?
            .count_entities(self.ns(namespace), filter)
            .await?)
    }

    // ---- Edge CRUD operations ----

    /// Fetch a single edge by id. Returns `None` if the edge does not exist.
    pub async fn get_edge(
        &self,
        namespace: Option<&str>,
        edge_id: Uuid,
    ) -> RuntimeResult<Option<Edge>> {
        Ok(self
            .graph(namespace)?
            .get_edge(LinkId::from(edge_id))
            .await?)
    }

    /// List edges matching `filter`. `limit` is capped at 1000; defaults to 100.
    pub async fn list_edges(
        &self,
        namespace: Option<&str>,
        filter: crate::curation::EdgeListFilter,
        limit: u32,
    ) -> RuntimeResult<Vec<Edge>> {
        let limit = limit.clamp(1, 1000);
        let page = self
            .graph(namespace)?
            .query_edges(
                filter.into(),
                vec![SortOrder {
                    field: EdgeSortField::CreatedAt,
                    direction: khive_storage::types::SortDirection::Asc,
                }],
                PageRequest { offset: 0, limit },
            )
            .await?;
        Ok(page.items)
    }

    /// Patch-style edge update. Only `Some(_)` fields are applied.
    pub async fn update_edge(
        &self,
        namespace: Option<&str>,
        edge_id: Uuid,
        relation: Option<EdgeRelation>,
        weight: Option<f64>,
    ) -> RuntimeResult<Edge> {
        let graph = self.graph(namespace)?;
        let mut edge = graph
            .get_edge(LinkId::from(edge_id))
            .await?
            .ok_or_else(|| crate::RuntimeError::NotFound(format!("edge {edge_id}")))?;

        if let Some(r) = relation {
            edge.relation = r;
        }
        if let Some(w) = weight {
            edge.weight = w.clamp(0.0, 1.0);
        }

        graph.upsert_edge(edge.clone()).await?;
        Ok(edge)
    }

    /// Hard-delete an edge by id. Returns `true` if an edge was removed.
    pub async fn delete_edge(&self, namespace: Option<&str>, edge_id: Uuid) -> RuntimeResult<bool> {
        Ok(self
            .graph(namespace)?
            .delete_edge(LinkId::from(edge_id))
            .await?)
    }

    /// Count edges matching `filter`.
    pub async fn count_edges(
        &self,
        namespace: Option<&str>,
        filter: crate::curation::EdgeListFilter,
    ) -> RuntimeResult<u64> {
        Ok(self.graph(namespace)?.count_edges(filter.into()).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curation::EdgeListFilter;
    use crate::runtime::KhiveRuntime;

    fn rt() -> KhiveRuntime {
        KhiveRuntime::memory().unwrap()
    }

    #[tokio::test]
    async fn update_edge_changes_weight() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        let updated = rt
            .update_edge(None, edge_id, None, Some(0.5))
            .await
            .unwrap();
        assert!((updated.weight - 0.5).abs() < 0.001);
    }

    #[tokio::test]
    async fn update_edge_changes_relation() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        let updated = rt
            .update_edge(None, edge_id, Some(EdgeRelation::VariantOf), None)
            .await
            .unwrap();
        assert_eq!(updated.relation, EdgeRelation::VariantOf);
    }

    #[tokio::test]
    async fn list_edges_filters_by_relation() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(None, "concept", "C", None, None, vec![])
            .await
            .unwrap();

        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        rt.link(None, a.id, c.id, EdgeRelation::DependsOn, 1.0)
            .await
            .unwrap();

        let filter = EdgeListFilter {
            relations: vec![EdgeRelation::Extends],
            ..Default::default()
        };
        let edges = rt.list_edges(None, filter, 100).await.unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].relation, EdgeRelation::Extends);
    }

    #[tokio::test]
    async fn list_edges_filters_by_source() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(None, "concept", "C", None, None, vec![])
            .await
            .unwrap();
        let d = rt
            .create_entity(None, "concept", "D", None, None, vec![])
            .await
            .unwrap();

        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        rt.link(None, c.id, d.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();

        let filter = EdgeListFilter {
            source_id: Some(a.id),
            ..Default::default()
        };
        let edges = rt.list_edges(None, filter, 100).await.unwrap();
        assert_eq!(edges.len(), 1);
        let src: Uuid = edges[0].source_id;
        assert_eq!(src, a.id);
    }

    #[tokio::test]
    async fn delete_edge_removes_from_storage() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        let deleted = rt.delete_edge(None, edge_id).await.unwrap();
        assert!(deleted);

        let fetched = rt.get_edge(None, edge_id).await.unwrap();
        assert!(fetched.is_none(), "edge should be gone after delete");
    }

    #[tokio::test]
    async fn count_edges_matches_filter() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(None, "concept", "C", None, None, vec![])
            .await
            .unwrap();

        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        rt.link(None, a.id, c.id, EdgeRelation::DependsOn, 1.0)
            .await
            .unwrap();

        let all = rt
            .count_edges(None, EdgeListFilter::default())
            .await
            .unwrap();
        assert_eq!(all, 2);

        let just_extends = rt
            .count_edges(
                None,
                EdgeListFilter {
                    relations: vec![EdgeRelation::Extends],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(just_extends, 1);
    }
}
