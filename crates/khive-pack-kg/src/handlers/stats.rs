//! `stats` verb handler.

use serde_json::Value;

use khive_runtime::{NamespaceToken, RuntimeError};

use khive_runtime::EdgeListFilter;

use super::common::{deser, StatsParams};
use crate::KgPack;

impl KgPack {
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
        let entities = self.runtime.count_entities(token, None).await?;
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
        Ok(serde_json::json!({
            "count_scope": {
                "namespaces": "caller_visible",
                "rows": "live_only",
            },
            "entities": entities,
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
        }))
    }
}
