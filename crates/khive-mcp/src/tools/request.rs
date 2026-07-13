//! Parameter type for the single `request` MCP tool.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// Input for `request` — a DSL string (function-call or JSON form) plus
/// optional presentation controls (`presentation` and `presentation_per_op`).
#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
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

    /// File path for result sink.
    ///
    /// When set, the full results are written as JSONL to this path and the
    /// caller receives a self-describing manifest instead of the raw results:
    /// `{path, rows, per_column_null_counts, schema_fingerprint, checksum}`.
    ///
    /// The manifest lets agents detect bulk-export corruption (e.g. 10 000 null
    /// rows) in one call rather than after a downstream judgment fleet has graded
    /// blind. Parent directories are created if absent.
    ///
    /// The resolved destination MUST stay within the allowed export root
    /// (default `~/.khive/exports`, overridable via `KHIVE_SAVE_TO_ROOT`).
    /// Relative paths are joined under the root; absolute paths are accepted
    /// only if they resolve inside it. Paths containing `..` traversal
    /// components and symlinked destinations are rejected.
    ///
    /// When omitted, results are returned inline (default behaviour).
    #[serde(default)]
    #[schemars(
        description = "File path to sink results as JSONL (returns manifest, not raw results)"
    )]
    pub save_to: Option<String>,

    /// Output serialization format for all ops in this request (ADR-078).
    ///
    /// - `"json"` (default): compact, lossless JSON.
    /// - `"auto"`: shape-aware — markdown table for homogeneous record arrays,
    ///   flat key-value block for single records, compact-JSON fallback.
    /// - `"table"`: force markdown-table renderer regardless of shape.
    ///
    /// Overrides `KHIVE_OUTPUT_FORMAT` and the TOML `default_output_format`.
    /// When omitted, the server's resolved default (env → toml → builtin `json`) is used.
    #[serde(default)]
    #[schemars(description = "Output format: \"json\" (default), \"auto\", or \"table\"")]
    pub format: Option<String>,

    /// Per-operation output format overrides (ADR-078).
    ///
    /// When provided, entries override `format` per op by index.
    /// `null` entries fall back to the batch-level `format`.
    ///
    /// When omitted, all ops use `format`.
    #[serde(default)]
    #[schemars(description = "Per-op output format override (optional)")]
    pub format_per_op: Option<Vec<Option<String>>>,

    /// Caller-supplied correlation id (khive#948), forwarded unchanged onto
    /// the daemon request frame and echoed back on the response so a
    /// benchmark harness can join its own pre-send sample to the server-side
    /// audit row for this request. Purely a correlation label — it never
    /// changes how a request is dispatched. When omitted, the request
    /// carries no id and its audit row has no `request_id` key.
    #[serde(default)]
    #[schemars(
        description = "Caller-supplied correlation id, echoed back and stamped into the audit event (optional)"
    )]
    pub request_id: Option<u64>,
}
