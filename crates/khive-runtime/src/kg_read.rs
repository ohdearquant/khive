//! Shared, read-only KG handle resolution for pack-scoped backends.

use std::collections::{BTreeSet, HashMap, HashSet};

use uuid::Uuid;

use crate::{KhiveRuntime, NamespaceToken, Resolved, RuntimeError};

pub(crate) struct KgReadResolver {
    runtimes: Vec<KhiveRuntime>,
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
        Self { runtimes: unique }
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
    use khive_storage::{Entity, Note};

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
            Err(RuntimeError::Storage(_) | RuntimeError::Sqlite(_))
        ));
        assert!(matches!(
            khive_storage::scope_request_read_deadline(expired, resolver.prefix("bad01234", false))
                .await,
            Err(RuntimeError::Storage(_) | RuntimeError::Sqlite(_))
        ));
        assert!(matches!(
            khive_storage::scope_request_read_deadline(expired, resolver.prefix("deadbeef", false))
                .await,
            Err(RuntimeError::Storage(_) | RuntimeError::Sqlite(_))
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
