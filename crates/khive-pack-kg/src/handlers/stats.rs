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
        if !p.kind.trim().eq_ignore_ascii_case("event") {
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
#[path = "stats_tests.rs"]
mod tests;
