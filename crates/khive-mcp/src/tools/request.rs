//! Parameter type for the single `request` MCP tool (ADR-020).

use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// Input for `request` — a DSL string (function-call or JSON form).
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RequestParams {
    /// One or more operations as a function-call DSL or JSON-form string.
    ///
    /// Examples:
    /// - `next()`
    /// - `assign(title="ship", priority="p1")`
    /// - `[create(kind="entity", entity_kind="concept", name="A"), create(kind="entity", entity_kind="concept", name="B")]`
    /// - `[{"tool":"next","args":{}}, {"tool":"complete","args":{"id":"abc"}}]`
    ///
    /// Max 100 operations per batch.
    #[schemars(
        description = "Function-call DSL or JSON-form batch (ADR-020). See request tool description."
    )]
    pub ops: String,
}
