//! `stats` and grouped event `count` verb handlers.

use std::collections::BTreeMap;

use khive_storage::event::EventGroupBy;
use khive_storage::EventFilter;
use serde::Deserialize;
use serde_json::Value;

use khive_runtime::operations::EntityStatsCounts;
use khive_runtime::{NamespaceToken, RuntimeError};

use khive_runtime::EdgeListFilter;

use super::common::{
    deser, parse_event_kind, parse_event_outcome, parse_event_substrate, StatsParams,
};
use crate::KgPack;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventCountParams {
    kind: String,
    group_by: EventGroupBy,
    #[serde(default)]
    verb: Option<String>,
    #[serde(default)]
    verbs: Vec<String>,
    #[serde(default)]
    event_kind: Option<String>,
    #[serde(default)]
    event_kinds: Vec<String>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    substrate: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    since: Option<i64>,
    #[serde(default)]
    until: Option<i64>,
}

impl KgPack {
    /// Aggregate stored event counts over only the caller's visible namespaces.
    pub(crate) async fn handle_count(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: EventCountParams = deser(params)?;
        if p.kind.trim().to_ascii_lowercase() != "event" {
            return Err(RuntimeError::InvalidInput(
                "count: kind must be event".into(),
            ));
        }
        let filter = EventFilter {
            verbs: p.verb.into_iter().chain(p.verbs).collect(),
            kinds: p
                .event_kind
                .iter()
                .chain(p.event_kinds.iter())
                .map(|kind| parse_event_kind(kind))
                .collect::<Result<_, _>>()?,
            actors: p.actor.into_iter().collect(),
            substrates: p
                .substrate
                .as_deref()
                .map(parse_event_substrate)
                .transpose()?
                .into_iter()
                .collect(),
            outcome: p.outcome.as_deref().map(parse_event_outcome).transpose()?,
            after: p.since,
            before: p.until,
            ..EventFilter::default()
        };
        let mut counts = BTreeMap::<String, u64>::new();
        for namespace in token.visible_namespaces() {
            // Narrow only to an already-authorized read namespace. The event
            // accessor retains the attributed/split store routing.
            let scoped = token.with_namespace(namespace.clone());
            for (key, count) in self
                .runtime
                .events(&scoped)?
                .count_events_grouped(filter.clone(), p.group_by)
                .await?
            {
                let total = counts.entry(key).or_default();
                *total = total
                    .checked_add(count)
                    .ok_or_else(|| RuntimeError::Internal("grouped event count overflow".into()))?;
            }
        }
        super::common::to_json(&counts)
    }

    /// Aggregate KG substrate counts (entities, edges, notes).
    ///
    /// Scope contract: every total here is summed across the caller's
    /// full *visible-namespace* set (`token.visible_namespaces()`), the same
    /// scope `list(kind=...)` merges pages over — not just `token.namespace()`.
    /// This keeps `stats()` reconcilable with a full `list` keyset walk under
    /// the same identity: `edges_by_relation` sums to `edges`, and each
    /// scalar equals the count of a full multi-namespace `list` walk, for
    /// entities, edges, and notes alike (#711).
    pub(crate) async fn handle_stats(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let _p: StatsParams = deser(params)?;
        let entity_counts = self.runtime.entity_stats_counts(token).await?;
        let edges = self
            .runtime
            .count_edges(token, EdgeListFilter::default())
            .await?;
        let edges_by_relation = self.runtime.count_edges_by_relation(token).await?;
        let by_base = self.runtime.count_edges_by_endpoint_base(token).await?;
        let notes = self.runtime.count_notes(token, None).await?;
        // `edges` is every live edge and is left alone. What it cannot be is a
        // density denominator: on a real store most edges are provenance, so
        // edges/entities computed from it overstates how connected the graph
        // is by roughly the provenance share. `edges_structural` names the
        // denominator that answers that question, and `edges_annotates` names
        // the largest thing excluded from it, so neither has to be derived by
        // a caller who would derive it wrong. Subtracting `annotates` from the
        // total is the wrong derivation: `supports` and `refutes` are
        // same-substrate, so a note-to-note edge is neither annotates nor
        // structure.
        let edges_annotates = edges_by_relation.get("annotates").copied().unwrap_or(0);
        let mut result = serde_json::json!({
            "count_scope": {
                "namespaces": "caller_visible",
                "rows": "live_only",
            },
            "edges": edges,
            "edges_by_relation": edges_by_relation,
            "edges_by_endpoint_base": {
                "entity_entity": by_base.entity_entity,
                "entity_note": by_base.entity_note,
                "note_entity": by_base.note_entity,
                "note_note": by_base.note_note,
                "unresolved": by_base.unresolved,
            },
            "edges_structural": by_base.entity_entity,
            "edges_annotates": edges_annotates,
            "notes": notes,
        });
        add_entity_counts(&mut result, entity_counts);
        Ok(result)
    }
}

fn add_entity_counts(result: &mut Value, counts: EntityStatsCounts) {
    result["entities"] = serde_json::json!(counts.entities);
    if let Some(mut groups) = counts.entities_by_type {
        groups.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        if groups
            .first()
            .is_none_or(|(entity_type, _)| entity_type.is_some())
        {
            groups.insert(0, (None, 0));
        }
        result["entities_by_type"] = Value::Array(
            groups
                .into_iter()
                .map(|(entity_type, count)| serde_json::json!({"entity_type": entity_type, "count": count}))
                .collect(),
        );
    }
}

#[cfg(test)]
mod tests {
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
        async fn count_entities(
            &self,
            namespace: &str,
            filter: EntityFilter,
        ) -> StorageResult<u64> {
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
        async fn count_entities(
            &self,
            namespace: &str,
            filter: EntityFilter,
        ) -> StorageResult<u64> {
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
                Entity::new("local", "concept", "BeforeSnapshot")
                    .with_entity_type(Some("algorithm")),
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
}
