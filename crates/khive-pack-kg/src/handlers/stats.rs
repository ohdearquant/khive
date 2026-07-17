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
        let notes = self.runtime.count_notes(token, None).await?;
        Ok(serde_json::json!({
            "entities": entities,
            "edges": edges,
            "edges_by_relation": edges_by_relation,
            "notes": notes,
        }))
    }
}
