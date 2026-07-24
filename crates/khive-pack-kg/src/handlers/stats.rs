//! `stats` verb handler.

use serde_json::Value;

use khive_runtime::{NamespaceToken, RuntimeError};

use khive_runtime::EdgeListFilter;

use super::common::{deser, StatsParams};
use crate::KgPack;

impl KgPack {
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
