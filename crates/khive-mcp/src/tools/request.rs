//! Parameter type for the single `request` MCP tool.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// Input for `request` — a DSL string (function-call or JSON form) plus
/// optional presentation controls (`presentation` and `presentation_per_op`).
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RequestParams {
    /// One or more operations as a function-call DSL or JSON-form string.
    ///
    /// Examples:
    /// - `gtd.next()`
    /// - `gtd.assign(title="ship", priority="p1")`
    /// - `create(kind="entity", name="A") | link(source_id=$prev.id, target_id="b", relation="extends")`
    /// - `[create(kind="entity", entity_kind="concept", name="A"), create(kind="entity", entity_kind="concept", name="B")]`
    /// - `[{"tool":"gtd.next","args":{}}, {"tool":"gtd.complete","args":{"id":"abc"}}]`
    ///
    /// Max 100 operations per batch.
    #[schemars(
        description = "Function-call DSL or JSON-form batch. See request tool description."
    )]
    pub ops: String,

    /// Presentation mode for the response.
    ///
    /// - `"agent"` (default): token-efficient — short UUIDs, compact timestamps,
    ///   empty fields dropped.
    /// - `"verbose"`: full canonical shape, no transformation.
    /// - `"human"`: delegated to CLI layer (same as verbose at runtime level).
    ///
    /// When omitted, defaults to `"agent"`.
    #[serde(default)]
    #[schemars(description = "Presentation mode: \"agent\" (default), \"verbose\", or \"human\"")]
    pub presentation: Option<String>,

    /// Per-operation presentation overrides.
    ///
    /// When provided, entries override `presentation` per op by index.
    /// `null` entries fall back to the batch-level `presentation`.
    ///
    /// When omitted, all ops use `presentation`.
    #[serde(default)]
    #[schemars(description = "Per-op presentation mode override (optional)")]
    pub presentation_per_op: Option<Vec<Option<String>>>,
}
