//! Graph lint under the accepted ADR-048 built-in rule and guarded-fix contract.

mod params;
mod rules;
mod stats;

use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};
use serde_json::{json, Value};

use crate::KnowledgePack;
use params::Selection;

impl KnowledgePack {
    pub(crate) async fn handle_lint(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let selection = Selection::parse(params)?;
        let capture = registry.capture_graph_lint(&self.runtime, token).await?;
        khive_storage::ensure_request_read_active("knowledge.lint report")?;
        if selection.stats_only {
            let stats = stats::collect(capture.entities(), capture.notes(), capture.edges());
            khive_storage::ensure_request_read_active("knowledge.lint report")?;
            return Ok(json!({"stats": stats}));
        }
        let report = rules::evaluate(
            capture.entities(),
            capture.notes(),
            capture.edges(),
            &selection,
        );
        khive_storage::ensure_request_read_active("knowledge.lint report")?;
        if selection.fix {
            let (report, fixes) = capture.apply_fixes(registry, token, report).await?;
            let mut result = serde_json::to_value(report).map_err(|error| {
                RuntimeError::Internal(format!("lint report serialization: {error}"))
            })?;
            result
                .as_object_mut()
                .ok_or_else(|| {
                    RuntimeError::Internal("lint report must serialize as an object".into())
                })?
                .insert(
                    "fixes".into(),
                    serde_json::to_value(fixes).map_err(|error| {
                        RuntimeError::Internal(format!("lint receipt serialization: {error}"))
                    })?,
                );
            Ok(result)
        } else {
            serde_json::to_value(report).map_err(|error| {
                RuntimeError::Internal(format!("lint report serialization: {error}"))
            })
        }
    }
}

#[cfg(test)]
#[path = "lint/dispatch_tests.rs"]
mod dispatch_tests;
