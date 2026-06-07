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
        let notes = self
            .runtime
            .notes(token)?
            .count_notes(token.namespace().as_str(), None)
            .await
            .map_err(RuntimeError::Storage)?;
        Ok(serde_json::json!({
            "entities": entities,
            "edges": edges,
            "notes": notes,
        }))
    }
}
