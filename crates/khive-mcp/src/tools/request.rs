//! Parameter type for the single `request` MCP tool.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// Input for `request` — a DSL string (function-call or JSON form) plus
/// optional presentation controls (`presentation` and `presentation_per_op`).
#[derive(Debug, Default, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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

    /// Parse and describe the request without dispatching any operation.
    /// Only `ops` may accompany `plan=true`; syntax errors are plan results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<bool>,

    /// Presentation mode for the response.
    ///
    /// - `"agent"` (default): token-efficient — short UUIDs, compact timestamps,
    ///   empty fields dropped.
    /// - `"verbose"`: full canonical shape, no transformation.
    /// - `"human"`: delegated to CLI layer (same as verbose at runtime level).
    ///
    /// When omitted, defaults to `"agent"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Presentation mode: \"agent\" (default), \"verbose\", or \"human\"")]
    pub presentation: Option<String>,

    /// Per-operation presentation overrides.
    ///
    /// When provided, entries override `presentation` per op by index.
    /// `null` entries fall back to the batch-level `presentation`.
    ///
    /// When omitted, all ops use `presentation`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Per-op presentation mode override (optional)")]
    pub presentation_per_op: Option<Vec<Option<String>>>,

    /// File path for result sink.
    ///
    /// When set, the full results are written as JSONL to this path and the
    /// caller receives a self-describing manifest instead of the raw results:
    /// `{path, rows, per_column_null_counts, schema_fingerprint, checksum,
    /// summary, failures?}`. Each failure retains its `error` payload and
    /// optional stable refusal `reason`.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Output format: \"json\" (default), \"auto\", or \"table\"")]
    pub format: Option<String>,

    /// Per-operation output format overrides (ADR-078).
    ///
    /// When provided, entries override `format` per op by index.
    /// `null` entries fall back to the batch-level `format`.
    ///
    /// When omitted, all ops use `format`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Per-op output format override (optional)")]
    pub format_per_op: Option<Vec<Option<String>>>,

    /// Request-group correlation id (khive#948), forwarded unchanged onto the
    /// daemon request frame and echoed back on the response so a benchmark
    /// harness can join its own pre-send sample to the server-side audit row.
    /// A caller-supplied value wins unchanged; when omitted, the MCP bridge
    /// mints an opaque nonzero id before daemon forwarding or local fallback.
    /// Purely a correlation label — it never changes dispatch semantics. Every
    /// operation in a batch or chain shares the value; it is not an
    /// operation-unique id or a cross-attempt idempotency key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Request-group correlation id echoed by the daemon and stamped into every operation's audit event; the MCP bridge generates one when omitted"
    )]
    pub request_id: Option<u64>,
}

pub(crate) const PLAN_COMPANIONS: [&str; 6] = [
    "presentation",
    "presentation_per_op",
    "format",
    "format_per_op",
    "save_to",
    "request_id",
];

impl<'de> Deserialize<'de> for RequestParams {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.get("plan") == Some(&serde_json::Value::Bool(true)) {
            if let Some(field) = PLAN_COMPANIONS
                .iter()
                .find(|field| value.get(**field).is_some())
            {
                return Err(serde::de::Error::custom(format!(
                    "plan=true cannot be combined with {field}"
                )));
            }
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            ops: String,
            plan: Option<bool>,
            presentation: Option<String>,
            presentation_per_op: Option<Vec<Option<String>>>,
            save_to: Option<String>,
            format: Option<String>,
            format_per_op: Option<Vec<Option<String>>>,
            request_id: Option<u64>,
        }
        let wire: Wire = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            ops: wire.ops,
            plan: wire.plan,
            presentation: wire.presentation,
            presentation_per_op: wire.presentation_per_op,
            save_to: wire.save_to,
            format: wire.format,
            format_per_op: wire.format_per_op,
            request_id: wire.request_id,
        })
    }
}

impl RequestParams {
    pub(crate) fn validate_plan_envelope(&self) -> Result<(), rmcp::ErrorData> {
        let present = [
            self.presentation.is_some(),
            self.presentation_per_op.is_some(),
            self.format.is_some(),
            self.format_per_op.is_some(),
            self.save_to.is_some(),
            self.request_id.is_some(),
        ];
        if let Some((field, _)) = PLAN_COMPANIONS
            .iter()
            .zip(present)
            .find(|(_, present)| *present)
        {
            return Err(rmcp::ErrorData::invalid_params(
                format!("plan=true cannot be combined with {field}"),
                None,
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::RequestParams;
    use serde_json::json;

    #[test]
    fn plan_envelope_accepts_ops_without_companions() {
        for ops in ["stats()", "stats("] {
            let params = serde_json::from_value::<RequestParams>(json!({
                "ops": ops, "plan": true
            }))
            .expect("planning accepts valid and malformed DSL envelopes");
            assert_eq!(params.ops, ops);
        }
        let schema = serde_json::to_value(rmcp::schemars::schema_for!(RequestParams)).unwrap();
        assert!(schema["properties"].get("plan").is_some());
    }

    #[test]
    fn plan_envelope_serialization_preserves_absent_companions() {
        let params = RequestParams {
            ops: "stats()".into(),
            plan: Some(true),
            ..Default::default()
        };
        let payload = serde_json::to_value(params).unwrap();
        assert_eq!(payload, json!({"ops":"stats()", "plan":true}));
        serde_json::from_value::<RequestParams>(payload).unwrap();
    }

    #[test]
    fn plan_envelope_rejects_each_present_companion_including_null() {
        for (field, value) in [
            ("presentation", json!("verbose")),
            ("presentation_per_op", json!([null])),
            ("format", json!("json")),
            ("format_per_op", json!([null])),
            ("save_to", json!("unused.jsonl")),
            ("request_id", json!(7)),
        ] {
            for value in [value, serde_json::Value::Null] {
                let mut payload = json!({"ops":"stats()", "plan":true});
                payload[field] = value;
                let error = serde_json::from_value::<RequestParams>(payload).unwrap_err();
                assert!(error.to_string().contains(field), "{field}: {error}");
                assert!(
                    !error.to_string().contains("unknown field `plan`"),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn request_params_reject_unknown_envelope_fields() {
        let error = serde_json::from_value::<RequestParams>(json!({
            "ops": "verbs()",
            "presentaton": "verbose"
        }))
        .expect_err("misspelled request-envelope fields must be rejected");

        assert!(
            error.to_string().contains("presentaton"),
            "the validation error must name the rejected field: {error}"
        );
    }

    #[test]
    fn request_tool_schema_is_closed() {
        let schema = rmcp::schemars::schema_for!(RequestParams);
        let value = serde_json::to_value(schema).expect("serialize request schema");

        assert_eq!(
            value.get("additionalProperties"),
            Some(&json!(false)),
            "the MCP request schema must reject fields outside its declared properties"
        );
    }
}
