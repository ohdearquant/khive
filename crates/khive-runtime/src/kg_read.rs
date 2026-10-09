//! Shared KG handle resolution and entity-delete routing for pack-scoped backends.

use std::collections::{BTreeSet, HashMap, HashSet};

use khive_storage::types::{Direction, NeighborCursor, NeighborHit, NeighborQuery};
use uuid::Uuid;

use crate::{KhiveRuntime, Namespace, NamespaceToken, Resolved, RuntimeError, VerbRegistry};

/// Query options for a KG neighbor read whose origin may live on a pack backend.
pub struct KgNeighborRead {
    pub query: NeighborQuery,
    pub after: Option<NeighborCursor>,
    pub neighbor_kinds: Option<Vec<String>>,
    pub enrich: bool,
    /// Narrow graph selection to an already visible namespace while retaining
    /// the caller token's identity and originating request metadata.
    pub namespace: Option<Namespace>,
}

pub(crate) fn neighbor_read_namespaces<'a>(
    token: &'a NamespaceToken,
    namespace: Option<&'a Namespace>,
) -> Result<&'a [Namespace], RuntimeError> {
    match namespace {
        Some(namespace) if token.visible_namespaces().contains(namespace) => {
            Ok(std::slice::from_ref(namespace))
        }
        Some(_) => Err(RuntimeError::InvalidInput(
            "KG neighbor namespace must already be visible to the caller".into(),
        )),
        None => Ok(token.visible_namespaces()),
    }
}

impl VerbRegistry {
    /// Resolve a live KG origin across configured backends before expanding
    /// adjacency on the graph runtime with the original caller token.
    pub async fn neighbors_for_kg_read(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        node_id: Uuid,
        options: KgNeighborRead,
    ) -> Result<Vec<NeighborHit>, RuntimeError> {
        self.neighbors_for_kg_read_inner(runtime, token, node_id, options, false)
            .await
            .map(|(hits, _)| hits)
    }

    /// Resolve a live origin and share entity-kind hints from its graph's
    /// deletion screen with mailbox filtering, retaining the original token.
    pub async fn neighbors_for_kg_read_with_entity_kinds(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        node_id: Uuid,
        options: KgNeighborRead,
    ) -> Result<(Vec<NeighborHit>, HashMap<Uuid, String>), RuntimeError> {
        self.neighbors_for_kg_read_inner(runtime, token, node_id, options, true)
            .await
    }

    async fn neighbors_for_kg_read_inner(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        node_id: Uuid,
        options: KgNeighborRead,
        with_entity_kinds: bool,
    ) -> Result<(Vec<NeighborHit>, HashMap<Uuid, String>), RuntimeError> {
        neighbor_read_namespaces(token, options.namespace.as_ref())?;
        if self
            .resolve_kg_read_by_id(runtime, token, node_id, false)
            .await?
            .is_none()
            && !runtime.substrate_exists_by_id(token, node_id).await?
        {
            return Err(RuntimeError::NotFound(format!(
                "neighbor anchor {node_id} not found"
            )));
        }
        if with_entity_kinds {
            runtime
                .neighbors_for_resolved_kg_read_with_entity_kinds(token, node_id, options)
                .await
        } else {
            runtime
                .neighbors_for_resolved_kg_read(token, node_id, options)
                .await
                .map(|hits| (hits, HashMap::new()))
        }
    }

    /// The directed form of [`Self::neighbors_for_kg_read`], retaining stored
    /// edge direction and the existing graph namespace selection.
    pub async fn directed_neighbors_for_kg_read(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        node_id: Uuid,
        query: NeighborQuery,
        namespace: Option<&Namespace>,
    ) -> Result<Vec<(NeighborHit, Direction)>, RuntimeError> {
        neighbor_read_namespaces(token, namespace)?;
        if self
            .resolve_kg_read_by_id(runtime, token, node_id, false)
            .await?
            .is_none()
            && !runtime.substrate_exists_by_id(token, node_id).await?
        {
            return Err(RuntimeError::NotFound(format!(
                "neighbor anchor {node_id} not found"
            )));
        }
        runtime
            .directed_neighbors_for_resolved_kg_read(token, node_id, query, namespace)
            .await
    }
}

pub(crate) struct KgReadResolver {
    runtimes: Vec<KhiveRuntime>,
    primary: KhiveRuntime,
    by_pack: HashMap<String, KhiveRuntime>,
}

impl KgReadResolver {
    pub(crate) fn new(primary: &KhiveRuntime, runtimes: &HashMap<String, KhiveRuntime>) -> Self {
        // Keep primary precedence deterministic and query each assigned backend
        // once, even when several packs share it. These handles own no registry,
        // so the registry-held topology creates no reference cycle.
        let mut seen = HashSet::new();
        let mut unique = Vec::new();
        let mut packs: Vec<_> = runtimes.iter().collect();
        packs.sort_by_key(|(name, _)| *name);
        for runtime in std::iter::once(primary).chain(packs.into_iter().map(|(_, rt)| rt)) {
            if seen.insert(runtime.backend_id().clone()) {
                unique.push(runtime.clone());
            }
        }
        Self {
            runtimes: unique,
            primary: primary.clone(),
            by_pack: runtimes.clone(),
        }
    }

    pub(crate) fn runtime_for_pack(&self, pack: &str) -> &KhiveRuntime {
        self.by_pack.get(pack).unwrap_or(&self.primary)
    }

    pub(crate) async fn by_id(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        include_deleted: bool,
    ) -> Result<Option<Resolved>, RuntimeError> {
        let mut found = None;
        for runtime in &self.runtimes {
            let candidate = if include_deleted {
                runtime.resolve_by_id_including_deleted(token, id).await?
            } else {
                runtime.resolve_by_id(token, id).await?
            };
            // Preserve entity-before-note substrate precedence. A success must
            // not hide a failure from a later configured backend.
            if found.is_none()
                || (matches!(&found, Some(Resolved::Note(_)))
                    && matches!(&candidate, Some(Resolved::Entity(_))))
            {
                found = candidate;
            }
        }
        Ok(found)
    }

    pub(crate) async fn entity_runtime(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> Result<Option<KhiveRuntime>, RuntimeError> {
        let mut owner = None;
        for runtime in &self.runtimes {
            let store = runtime.entities(token)?;
            let entity = store.get_entity_including_deleted(id).await?;
            if entity.is_some() {
                if owner.is_some() {
                    return Err(RuntimeError::InvalidInput(format!(
                        "entity {id} exists on multiple backends; deletion is ambiguous"
                    )));
                }
                owner = Some(runtime.clone());
            }
        }
        Ok(owner)
    }

    pub(crate) async fn prefix(
        &self,
        prefix: &str,
        include_deleted: bool,
    ) -> Result<Option<Uuid>, RuntimeError> {
        let mut matches = BTreeSet::new();
        for runtime in &self.runtimes {
            let candidate = runtime
                .resolve_prefix_for_kg_read(prefix, include_deleted)
                .await;
            match candidate {
                Ok(Some(id)) => {
                    matches.insert(id);
                }
                Ok(None) => {}
                Err(RuntimeError::AmbiguousPrefix { matches: ids, .. }) => matches.extend(ids),
                Err(error) => return Err(error),
            }
        }
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.into_iter().next()),
            _ => Err(RuntimeError::AmbiguousPrefix {
                prefix: prefix.into(),
                matches: matches.into_iter().collect(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackendId, Namespace, RuntimeConfig};
    use chrono::Utc;
    use khive_storage::types::{Edge, LinkId};
    use khive_storage::{Entity, Note};

    #[tokio::test]
    async fn kg_neighbor_namespace_selection_is_narrowing_and_keeps_directed_self_loops() {
        let project = Namespace::parse("project").unwrap();
        let main = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            backend_id: BackendId::main(),
            actor_id: Some("reader".into()),
            visible_namespaces: vec![project.clone()],
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let token = main
            .authorize_with_visibility(Namespace::local(), vec![project.clone()])
            .unwrap();
        let anchor = Entity::new("local", "concept", "namespace selector anchor");
        let neighbor = Entity::new("project", "concept", "namespace selector neighbor");
        for entity in [&anchor, &neighbor] {
            main.entities(&token)
                .unwrap()
                .upsert_entity(entity.clone())
                .await
                .unwrap();
        }
        let self_loop = Uuid::new_v4();
        for (namespace, id, source) in [
            (Namespace::local(), self_loop, anchor.id),
            (project.clone(), Uuid::new_v4(), neighbor.id),
        ] {
            let edge_token = main.authorize(namespace.clone()).unwrap();
            let now = Utc::now();
            main.graph(&edge_token)
                .unwrap()
                .upsert_edge(Edge {
                    id: LinkId(id),
                    namespace: namespace.to_string(),
                    source_id: source,
                    target_id: anchor.id,
                    relation: khive_storage::EdgeRelation::Extends,
                    weight: 1.0,
                    created_at: now,
                    updated_at: now,
                    deleted_at: None,
                    metadata: None,
                    target_backend: None,
                })
                .await
                .unwrap();
        }
        let registry = crate::VerbRegistryBuilder::new().build().unwrap();
        let query = NeighborQuery {
            direction: Direction::Both,
            relations: None,
            limit: Some(10),
            min_weight: None,
        };
        let baseline = main
            .neighbors_with_query_directed(&token, anchor.id, query.clone())
            .await
            .unwrap();
        let full = registry
            .directed_neighbors_for_kg_read(&main, &token, anchor.id, query.clone(), None)
            .await
            .unwrap();
        let keys = |hits: &[(NeighborHit, Direction)]| {
            hits.iter()
                .map(|(hit, direction)| (hit.node_id, hit.edge_id, direction.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&full), keys(&baseline));
        let local = registry
            .directed_neighbors_for_kg_read(
                &main,
                &token,
                anchor.id,
                query.clone(),
                Some(&Namespace::local()),
            )
            .await
            .unwrap();
        assert_eq!(local.len(), 2);
        assert_eq!(local[0].0.edge_id, self_loop);
        assert_eq!(local[0].1, Direction::Out);
        assert_eq!(local[1].1, Direction::In);
        let project_hits = registry
            .neighbors_for_kg_read(
                &main,
                &token,
                anchor.id,
                KgNeighborRead {
                    query: query.clone(),
                    after: None,
                    neighbor_kinds: None,
                    enrich: true,
                    namespace: Some(project),
                },
            )
            .await
            .unwrap();
        assert_eq!(project_hits.len(), 1);
        assert_eq!(project_hits[0].node_id, neighbor.id);
        let outside = Namespace::parse("outside").unwrap();
        assert!(matches!(
            registry
                .directed_neighbors_for_kg_read(
                    &main,
                    &token,
                    anchor.id,
                    query.clone(),
                    Some(&outside)
                )
                .await,
            Err(RuntimeError::InvalidInput(_))
        ));
        assert!(matches!(
            registry
                .neighbors_for_kg_read(
                    &main,
                    &token,
                    anchor.id,
                    KgNeighborRead {
                        query,
                        after: None,
                        neighbor_kinds: None,
                        enrich: true,
                        namespace: Some(outside),
                    },
                )
                .await,
            Err(RuntimeError::InvalidInput(_))
        ));
    }

    fn runtime(name: &str) -> KhiveRuntime {
        KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            backend_id: BackendId::parse(name).unwrap(),
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap()
    }

    async fn note(runtime: &KhiveRuntime, id: Uuid, namespace: &str) -> Note {
        let token = runtime
            .authorize(Namespace::parse(namespace).unwrap())
            .unwrap();
        let mut note = Note::new(namespace, "observation", "read routing fixture");
        note.id = id;
        runtime
            .notes(&token)
            .unwrap()
            .upsert_note(note.clone())
            .await
            .unwrap();
        note
    }

    #[tokio::test]
    async fn issue2992_shared_reads_deduplicate_backends_and_preserve_global_prefix_ambiguity() {
        let main = runtime("main");
        let secondary = runtime("comm");
        let resolver = KgReadResolver::new(
            &main,
            &HashMap::from([
                ("kg".into(), main.clone()),
                ("comm".into(), secondary.clone()),
                ("another-pack".into(), secondary.clone()),
            ]),
        );
        assert_eq!(resolver.runtimes.len(), 2);
        let first = Uuid::parse_str("cafe1234-0000-4000-8000-000000000001").unwrap();
        let second = Uuid::parse_str("cafe1234-0000-4000-8000-000000000002").unwrap();
        let remote = note(&secondary, first, "elsewhere").await;
        let token = main.authorize(Namespace::local()).unwrap();
        assert!(
            matches!(resolver.by_id(&token, first, false).await.unwrap(), Some(Resolved::Note(found)) if found == remote)
        );
        assert_eq!(
            resolver.prefix("cafe1234", false).await.unwrap(),
            Some(first)
        );
        // The same global UUID in another backend is not a distinct prefix candidate.
        note(&main, first, "local").await;
        assert_eq!(
            resolver.prefix("cafe1234", false).await.unwrap(),
            Some(first)
        );
        note(&main, second, "local").await;
        let error = resolver.prefix("cafe1234", false).await.unwrap_err();
        assert!(
            matches!(error, RuntimeError::AmbiguousPrefix { matches, .. } if matches == vec![first, second])
        );
        assert!(resolver.prefix("deadbeef", false).await.unwrap().is_none());
        assert!(resolver
            .by_id(&token, Uuid::new_v4(), false)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn issue2992_shared_reads_surface_backend_read_failures_instead_of_partial_results() {
        let main = runtime("main");
        let secondary = runtime("comm");
        let resolver =
            KgReadResolver::new(&main, &HashMap::from([("comm".into(), secondary.clone())]));
        let id = Uuid::parse_str("bad01234-0000-4000-8000-000000000001").unwrap();
        note(&main, id, "local").await;
        let token = main.authorize(Namespace::local()).unwrap();
        assert!(resolver.by_id(&token, id, false).await.unwrap().is_some());
        assert_eq!(resolver.prefix("bad01234", false).await.unwrap(), Some(id));
        // A backend read that cannot be admitted is a storage failure of the
        // whole lookup, never a quiet miss: a pooled reader checkout refused by
        // an exhausted request read deadline surfaces through both entry points.
        // (Schema faults injected through the SQL writer are invisible to the
        // pooled readers' snapshots, so admission is the fault a test can inject.)
        let expired = std::time::Duration::ZERO;
        assert!(matches!(
            khive_storage::scope_request_read_deadline(expired, resolver.by_id(&token, id, false))
                .await,
            Err(RuntimeError::Storage(_))
        ));
        assert!(matches!(
            khive_storage::scope_request_read_deadline(expired, resolver.prefix("bad01234", false))
                .await,
            Err(RuntimeError::Storage(_))
        ));
        assert!(matches!(
            khive_storage::scope_request_read_deadline(expired, resolver.prefix("deadbeef", false))
                .await,
            Err(RuntimeError::Storage(_))
        ));
    }

    #[tokio::test]
    async fn issue2992_shared_reads_include_live_entities_and_explicit_deleted_notes() {
        let main = runtime("main");
        let secondary = runtime("archive");
        let resolver = KgReadResolver::new(
            &main,
            &HashMap::from([("archive".into(), secondary.clone())]),
        );
        let token = main.authorize(Namespace::local()).unwrap();
        let entity = Entity::new("elsewhere", "concept", "remote entity");
        let entity_id = entity.id;
        secondary
            .entities(&token)
            .unwrap()
            .upsert_entity(entity)
            .await
            .unwrap();
        assert!(
            matches!(resolver.by_id(&token, entity_id, false).await.unwrap(), Some(Resolved::Entity(found)) if found.id == entity_id)
        );
        let mut deleted = Note::new("local", "observation", "deleted remote note");
        deleted.deleted_at = Some(deleted.updated_at);
        secondary
            .notes(&token)
            .unwrap()
            .upsert_note(deleted.clone())
            .await
            .unwrap();
        assert!(resolver
            .by_id(&token, deleted.id, false)
            .await
            .unwrap()
            .is_none());
        assert!(
            matches!(resolver.by_id(&token, deleted.id, true).await.unwrap(), Some(Resolved::Note(found)) if found == deleted)
        );
        let prefix = deleted.id.simple().to_string();
        assert_eq!(
            resolver.prefix(&prefix, true).await.unwrap(),
            Some(deleted.id)
        );
    }
}
