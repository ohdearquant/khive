//! Verb handler implementations for `BrainPack`.

use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use khive_runtime::{
    micros_to_iso, DispatchHook, EventView, KhiveRuntime, NamespaceToken, RuntimeError,
    VerbRegistry,
};
use khive_storage::event::{Event, EventFilter};
use khive_storage::types::PageRequest;
use khive_types::HandlerDef;

use crate::event::interpret;
use crate::{sync_balanced_recall_record, BrainPack, ENTITY_CACHE_CAPACITY};
use khive_brain_core::derive_deterministic_weights;
#[cfg(feature = "lattice-router")]
use khive_brain_core::BetaPosterior;
use khive_brain_core::{
    ConsumerKind, FeedbackEventKind, ProfileBinding, ProfileLifecycle, ProfileRecord,
    SectionPosteriorState, SectionType, DEFAULT_ESS_CAP,
};

// ── Adapter revision sentinel ─────────────────────────────────────────────────

/// Base model revision that registered adapters must target; overridable via
/// KHIVE_BRAIN_BASE_MODEL_REVISION so both accept and reject paths are testable.
pub(crate) const DEFAULT_BASE_MODEL_REVISION: &str = "base-v0";

// ── Handler table ─────────────────────────────────────────────────────────────

/// Brain pack verb surface. Visibility::Verb = exposed on the MCP `request` tool.
/// Visibility::Subhandler = internal / operator-only. Illocutionary classification applied.
pub(crate) static BRAIN_HANDLERS: &[HandlerDef] = &[
    // ── Assertive (read) verbs ────────────────────────────────────────────
    HandlerDef {
        name: "brain.state",
        description: "Return current BrainState snapshot for inspection",
        visibility: khive_types::Visibility::Subhandler,
        category: khive_types::VerbCategory::Assertive,
        params: &[],
    },
    HandlerDef {
        name: "brain.config",
        description: "Return projected config for a named pack parameter",
        visibility: khive_types::Visibility::Subhandler,
        category: khive_types::VerbCategory::Assertive,
        params: &[khive_types::ParamDef {
            name: "parameter",
            param_type: "string",
            required: false,
            description: "Specific parameter to query: \"recall::relevance_weight\" | \"recall::salience_weight\" | \"recall::temporal_weight\". Omit to return all.",
        }],
    },
    HandlerDef {
        name: "brain.events",
        description: "List recent brain-relevant events for debugging",
        visibility: khive_types::Visibility::Subhandler,
        category: khive_types::VerbCategory::Assertive,
        params: &[khive_types::ParamDef {
            name: "limit",
            param_type: "integer",
            required: false,
            description: "Maximum events to return (default 20, max 100).",
        }],
    },
    HandlerDef {
        name: "brain.event_counts",
        description: "Windowed event counts grouped by kind, actor, and verb over the event \
            plane (ADR-103 Stage 1, #724 Ask A); feedback_explicit events additionally split by \
            served_by_profile_id; events carrying a work_class (today: phase_started / \
            phase_completed / phase_cancelled payloads, checked before any future \
            payload.resource.work_class) split by counts_by_work_class. Events carrying \
            payload.resource.cost_unit (ADR-103 Amendment 1; stamped on every successful verb \
            dispatch since PR #927) sum into total_cost_unit and cost_unit_by_verb; events with \
            no resource.cost_unit (pre-Amendment-1 events, or errored/denied dispatches) simply \
            do not contribute. Both fields are omitted, not zero-filled, when no event in the \
            window carries cost_unit. When truncated=true, both sums are over the fetched page \
            only, same as the other counts_by_* fields.",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            khive_types::ParamDef {
                name: "since",
                param_type: "string",
                required: true,
                description: "Window start, ISO-8601/RFC-3339 datetime (e.g. \"2026-07-01T00:00:00Z\"). Inclusive.",
            },
            khive_types::ParamDef {
                name: "until",
                param_type: "string",
                required: false,
                description: "Window end, ISO-8601/RFC-3339 datetime. Exclusive. Defaults to now.",
            },
            khive_types::ParamDef {
                name: "actor",
                param_type: "string",
                required: false,
                description: "Filter to a single actor. Stored actor strings are prefixed \
                    (e.g. \"actor:lambda:khive\"); pass either the bare seat form \
                    (\"lambda:khive\") or the stored prefixed form — both match. Omit for all \
                    actors.",
            },
            khive_types::ParamDef {
                name: "kind",
                param_type: "string",
                required: false,
                description: "Filter to a single EventKind (e.g. \"recall_executed\", \"feedback_explicit\"). Omit for all kinds.",
            },
        ],
    },
    HandlerDef {
        name: "brain.profiles",
        description: "List profiles, optionally filtered by lifecycle",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[khive_types::ParamDef {
            name: "lifecycle",
            param_type: "string",
            required: false,
            description: "Filter profiles by lifecycle state: \"active\" | \"inactive\" | \"archived\". Omit to return all.",
        }],
    },
    HandlerDef {
        name: "brain.profile",
        description: "Profile metadata, latest snapshot, current state summary",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[khive_types::ParamDef {
            name: "profile_id",
            param_type: "string",
            required: true,
            description: "Profile ID string (e.g. \"balanced-recall-v1\"). NOT a UUID — use the string identifier. Alias: id.",
        }],
    },
    HandlerDef {
        name: "brain.resolve",
        description: "Show which profile would serve a caller context",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            khive_types::ParamDef {
                name: "consumer_kind",
                param_type: "string",
                required: true,
                description: "Verb or operation type the caller is about to perform (e.g. \"recall\").",
            },
            khive_types::ParamDef {
                name: "actor",
                param_type: "string",
                required: false,
                description: "Caller actor identifier. Defaults to the caller's dispatch identity; anonymous callers match only wildcard bindings. Pass explicitly to query another identity.",
            },
            khive_types::ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Namespace for binding resolution. Defaults to \"*\" wildcard match.",
            },
        ],
    },
    // ── Commissive (write state) verbs ────────────────────────────────────
    HandlerDef {
        name: "brain.activate",
        description: "Move a profile to Active (lifecycle transition; serving reads profile state per-request)",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[khive_types::ParamDef {
            name: "profile_id",
            param_type: "string",
            required: true,
            description: "Profile ID to activate (e.g. \"balanced-recall-v1\").",
        }],
    },
    HandlerDef {
        name: "brain.deactivate",
        description: "Move a profile to Inactive (stop live updates, retain state)",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[khive_types::ParamDef {
            name: "profile_id",
            param_type: "string",
            required: true,
            description: "Profile ID to deactivate.",
        }],
    },
    HandlerDef {
        name: "brain.archive",
        description: "Move a profile to Archived (read-only, audit-retained)",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Declaration,
        params: &[khive_types::ParamDef {
            name: "profile_id",
            param_type: "string",
            required: true,
            description: "Profile ID to archive.",
        }],
    },
    HandlerDef {
        name: "brain.reset",
        description: "Reset posteriors to priors (preserves event history; increments exploration_epoch, which counts resets and nothing else)",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Declaration,
        params: &[khive_types::ParamDef {
            name: "profile_id",
            param_type: "string",
            required: false,
            description: "Profile ID to reset (must exist and be active). Defaults to \"balanced-recall-v1\". Use brain.profiles() to list profiles.",
        }],
    },
    HandlerDef {
        name: "brain.feedback",
        description: "Emit a FeedbackExplicit event into the shared log",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            khive_types::ParamDef {
                name: "target_id",
                param_type: "uuid",
                required: true,
                description: "UUID of the memory note or entity the feedback applies to.",
            },
            khive_types::ParamDef {
                name: "signal",
                param_type: "string",
                required: true,
                description: "Feedback signal: \"useful\" | \"not_useful\" | \"wrong\" | \"explicit_positive\" | \"explicit_negative\" | \"implicit_positive\" | \"implicit_negative\" | \"correction\".",
            },
            khive_types::ParamDef {
                name: "served_by_profile_id",
                param_type: "string",
                required: false,
                description: "Profile ID that served the result being rated. Recorded in the event payload.",
            },
            khive_types::ParamDef {
                name: "section_signals",
                param_type: "object",
                required: false,
                description: "Per-section feedback signals: {\"section_name\": \"useful\"|\"not_useful\"|\"wrong\"}. For knowledge_compose profiles.",
            },
            khive_types::ParamDef {
                name: "scorer_run_id",
                param_type: "string",
                required: false,
                description: "ADR-081: scorer pass identifier, half of the (scorer_run_id, serve_ledger_id) dedup key. Must be supplied together with serve_ledger_id.",
            },
            khive_types::ParamDef {
                name: "serve_ledger_id",
                param_type: "string",
                required: false,
                description: "ADR-081: id of the brain_serve_ledger row being graded. Must be supplied together with scorer_run_id; backfills the row's grade and gates dedup.",
            },
        ],
    },
    HandlerDef {
        name: "brain.auto_feedback",
        description: "Emit implicit feedback for recall results supplied by an agent. \
            Convenience verb: agents call this after memory.recall instead of constructing \
            a brain.feedback call manually. Keeps memory and brain packs decoupled (#517).",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            khive_types::ParamDef {
                name: "query",
                param_type: "string",
                required: true,
                description: "Recall query that produced the results.",
            },
            khive_types::ParamDef {
                name: "results",
                param_type: "array",
                required: true,
                description: "Recall result objects; the first object's id is credited.",
            },
            khive_types::ParamDef {
                name: "signal",
                param_type: "string",
                required: false,
                description: "Feedback signal. Defaults to \"implicit_positive\".",
            },
            khive_types::ParamDef {
                name: "served_by_profile_id",
                param_type: "string",
                required: false,
                description: "Profile ID that served the recall. Defaults like brain.feedback.",
            },
            khive_types::ParamDef {
                name: "scorer_run_id",
                param_type: "string",
                required: false,
                description: "ADR-081: forwarded verbatim to brain.feedback. Must be supplied together with serve_ledger_id.",
            },
            khive_types::ParamDef {
                name: "serve_ledger_id",
                param_type: "string",
                required: false,
                description: "ADR-081: forwarded verbatim to brain.feedback. Must be supplied together with scorer_run_id.",
            },
        ],
    },
    HandlerDef {
        name: "brain.record_serve",
        description: "ADR-081 §4/§5: append cross-session serve-ledger rows for a batch of \
            recall targets. Internal-only — memory.recall dispatches this off its response \
            path after resolving served_by_profile_id via the ADR-035 three-tier discipline.",
        visibility: khive_types::Visibility::Subhandler,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            khive_types::ParamDef {
                name: "consumer_kind",
                param_type: "string",
                required: true,
                description: "Consumer kind that served these results, e.g. \"recall\".",
            },
            khive_types::ParamDef {
                name: "served_by_profile_id",
                param_type: "string",
                required: false,
                description: "Profile ID resolved at serve time. Omitted when unresolved.",
            },
            khive_types::ParamDef {
                name: "target_ids",
                param_type: "array",
                required: true,
                description: "Note/entity ids that were served; one ledger row per id.",
            },
            khive_types::ParamDef {
                name: "query_raw",
                param_type: "string",
                required: true,
                description: "Raw query text; query_class is derived from this deterministically.",
            },
            khive_types::ParamDef {
                name: "served_at",
                param_type: "integer",
                required: false,
                description: "Serve timestamp in epoch microseconds. Defaults to now.",
            },
        ],
    },
    // ── Declaration verbs ─────────────────────────────────────────────────
    HandlerDef {
        name: "brain.bind",
        description: "Write a row in the profile resolution table",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Declaration,
        params: &[
            khive_types::ParamDef {
                name: "profile_id",
                param_type: "string",
                required: true,
                description: "Profile ID to bind (must exist).",
            },
            khive_types::ParamDef {
                name: "actor",
                param_type: "string",
                required: false,
                description: "Actor identifier to match. Default \"*\" (all actors). Cannot contain \"*\" inside a real value.",
            },
            khive_types::ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Namespace to match. Default \"*\" (all namespaces).",
            },
            khive_types::ParamDef {
                name: "consumer_kind",
                param_type: "string",
                required: false,
                description: "Verb / operation kind to match. Default \"*\" (all kinds).",
            },
            khive_types::ParamDef {
                name: "priority",
                param_type: "integer",
                required: false,
                description: "Binding priority; higher wins when multiple bindings match (default 0).",
            },
        ],
    },
    HandlerDef {
        name: "brain.unbind",
        description: "Remove rows from the profile resolution table. At least one filter (profile_id, actor, namespace, or consumer_kind) is required.",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Declaration,
        params: &[
            khive_types::ParamDef {
                name: "profile_id",
                param_type: "string",
                required: false,
                description: "Remove bindings for this profile ID. All filters use AND semantics. At least one filter is required.",
            },
            khive_types::ParamDef {
                name: "actor",
                param_type: "string",
                required: false,
                description: "Remove bindings for this actor.",
            },
            khive_types::ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Remove bindings for this namespace.",
            },
            khive_types::ParamDef {
                name: "consumer_kind",
                param_type: "string",
                required: false,
                description: "Remove bindings for this consumer_kind.",
            },
        ],
    },
    HandlerDef {
        name: "brain.bindings",
        description: "List rows in the profile resolution table, optionally filtered",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            khive_types::ParamDef {
                name: "profile_id",
                param_type: "string",
                required: false,
                description: "Filter bindings by profile ID.",
            },
            khive_types::ParamDef {
                name: "actor",
                param_type: "string",
                required: false,
                description: "Filter bindings by actor.",
            },
            khive_types::ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Filter bindings by namespace.",
            },
            khive_types::ParamDef {
                name: "consumer_kind",
                param_type: "string",
                required: false,
                description: "Filter bindings by consumer_kind.",
            },
        ],
    },
    HandlerDef {
        name: "brain.create_profile",
        description: "Create a new brain profile with given name and optional seed priors",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Declaration,
        params: &[
            khive_types::ParamDef {
                name: "name",
                param_type: "string",
                required: true,
                description: "Profile ID / name (alphanumeric, hyphens allowed, e.g. \"my-profile-v1\"). Must be unique.",
            },
            khive_types::ParamDef {
                name: "description",
                param_type: "string",
                required: false,
                description: "Human-readable description for this profile.",
            },
            khive_types::ParamDef {
                name: "consumer_kind",
                param_type: "string",
                required: false,
                description: "Operation kind this profile targets (e.g. \"recall\"). Default \"recall\".",
            },
            khive_types::ParamDef {
                name: "seed_priors",
                param_type: "object",
                required: false,
                description: "Seed priors object. For knowledge_compose: {\"section_posteriors\": {\"overview\": {\"alpha\": 2.0, \"beta\": 2.0}, ...}}. For recall: {\"relevance\": {\"alpha\": 7.0, \"beta\": 3.0}, ...}.",
            },
        ],
    },
    HandlerDef {
        name: "brain.register_adapter",
        description: "Register an adapter integrity record so the router only composes \
            adapters matching the active base model revision",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Declaration,
        params: &[
            khive_types::ParamDef {
                name: "adapter_id",
                param_type: "string",
                required: true,
                description: "Stable identifier for the adapter (used as the entity name).",
            },
            khive_types::ParamDef {
                name: "content_hash",
                param_type: "string",
                required: true,
                description: "Content hash of the adapter weights for integrity verification.",
            },
            khive_types::ParamDef {
                name: "base_model_revision",
                param_type: "string",
                required: true,
                description: "Base model revision the adapter was trained against. Must match the active revision or registration is rejected.",
            },
            khive_types::ParamDef {
                name: "metadata",
                param_type: "object",
                required: false,
                description: "Optional additional metadata merged into entity properties.",
            },
        ],
    },
    // ── Legacy / internal ─────────────────────────────────────────────────
    HandlerDef {
        name: "brain.emit",
        description: "Manually emit a feedback event (deprecated; use brain.feedback)",
        visibility: khive_types::Visibility::Subhandler,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            khive_types::ParamDef {
                name: "target_id",
                param_type: "uuid",
                required: true,
                description: "UUID of the record the feedback applies to.",
            },
            khive_types::ParamDef {
                name: "signal",
                param_type: "string",
                required: true,
                description: "Feedback signal: \"useful\" | \"not_useful\" | \"wrong\". Deprecated: use brain.feedback instead.",
            },
            khive_types::ParamDef {
                name: "served_by_profile_id",
                param_type: "string",
                required: false,
                description: "Profile ID that served the result.",
            },
        ],
    },
];

// ── BrainPack handler impl ────────────────────────────────────────────────────

impl BrainPack {
    // ── brain.state ───────────────────────────────────────────────────────

    pub(crate) async fn handle_state(&self, _params: Value) -> Result<Value, RuntimeError> {
        let state = self.state.lock().unwrap();
        let snapshot = state.to_snapshot();
        serde_json::to_value(&snapshot).map_err(|e| RuntimeError::InvalidInput(e.to_string()))
    }

    // ── brain.config ──────────────────────────────────────────────────────

    pub(crate) async fn handle_config(&self, params: Value) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ConfigParams {
            parameter: Option<String>,
        }
        let p: ConfigParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let state = self.state.lock().unwrap();
        let br = &state.balanced_recall;

        let param_map = [
            ("recall::relevance_weight", &br.relevance),
            ("recall::salience_weight", &br.salience),
            ("recall::temporal_weight", &br.temporal),
        ];

        match p.parameter {
            Some(key) => {
                let posterior = param_map
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, p)| *p)
                    .ok_or_else(|| {
                        RuntimeError::NotFound(format!(
                            "parameter {key:?}; valid: {}",
                            param_map
                                .iter()
                                .map(|(k, _)| *k)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                    })?;
                Ok(json!({
                    "parameter": key,
                    "mean": posterior.mean(),
                    "variance": posterior.variance(),
                    "ess": posterior.effective_sample_size(),
                    "alpha": posterior.alpha(),
                    "beta": posterior.beta(),
                }))
            }
            None => {
                let configs: serde_json::Map<String, Value> = param_map
                    .iter()
                    .map(|(k, p)| {
                        (
                            (*k).to_owned(),
                            json!({
                                "mean": p.mean(),
                                "variance": p.variance(),
                                "ess": p.effective_sample_size(),
                            }),
                        )
                    })
                    .collect();
                Ok(Value::Object(configs))
            }
        }
    }

    // ── brain.events ──────────────────────────────────────────────────────

    pub(crate) async fn handle_events(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct EventsParams {
            limit: Option<u32>,
        }
        let p: EventsParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let limit = p.limit.unwrap_or(20).min(100);
        let ns = token.namespace().as_str().to_string();

        let store = self.runtime.events(token)?;
        let filter = EventFilter {
            verbs: vec![
                "recall".into(),
                "search".into(),
                "brain.feedback".into(),
                "brain.emit".into(), // retained for backward-compat queries
                "get".into(),
                "remember".into(),
            ],
            ..EventFilter::default()
        };
        let _ = ns;
        let page = store
            .query_events(filter, PageRequest { offset: 0, limit })
            .await
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let events: Vec<Value> = page
            .items
            .iter()
            .map(|e| {
                json!({
                    "id": e.id.to_string(),
                    "verb": e.verb,
                    "outcome": e.outcome,
                    "target_id": e.target_id.map(|t| t.to_string()),
                    "duration_us": e.duration_us,
                    "created_at": micros_to_iso(e.created_at),
                    "payload": e.payload,
                })
            })
            .collect();

        Ok(json!({
            "count": events.len(),
            "events": events,
        }))
    }

    // ── brain.event_counts ──────────────────────────────────────────────────
    //
    // ADR-103 Stage 1 (#724 Ask A): a windowed, per-actor, per-kind, per-verb
    // read verb over the event plane, additionally surfacing `work_class`
    // (ADR-103-resource-attribution-model.md:303) via `counts_by_work_class`.
    // `work_class` is read from `payload.work_class` first — the shape the
    // three Phase* events (`PhaseStarted`/`PhaseCompleted`/`PhaseCancelled`,
    // `khive_storage::telemetry`) use today — falling back to
    // `payload.resource.work_class` for the not-yet-emitted future shape
    // where a generic audit row carries a `resource` sub-object (ADR-103
    // Stage 1's "resource payload enrichment", still open). Events with
    // neither path present are simply not counted in the split.
    // `cost_unit` (ADR-103 Amendment 1, `payload.resource.cost_unit`,
    // stamped on every successful verb dispatch since PR #927 via
    // `khive_runtime::cost_unit::resource_payload`) sums into
    // `total_cost_unit` and `cost_unit_by_verb`, both omitted rather than
    // zero-filled when no event in the window carries it. Amendment 1's
    // "absence has exactly two meanings" rule means a missing
    // `resource.cost_unit` is either a pre-Amendment-1 event or an
    // errored/denied dispatch — this verb does not distinguish the two, it
    // simply excludes the event from the sum, matching the `work_class`
    // split's existing exclusion convention.
    //
    // Named `brain.event_counts` rather than the literal
    // `brain.events(...)` shape sketched on the issue — that name is already
    // taken by the `Visibility::Subhandler` debug-listing verb above, which an
    // MCP-boundary test (`subhandler_verbs_are_blocked_at_mcp_boundary`)
    // requires to stay internal-only, and which several existing tests depend
    // on for its `{count, events}` shape. Reusing the name would either break
    // that invariant or overload one verb with two incompatible response
    // shapes; a second, public verb name keeps both surfaces intact.

    /// Cap on events aggregated by a single `brain.event_counts` window query.
    /// `EventStore` has no SQL-side grouped-count primitive today (`count_events`
    /// returns one scalar per filter, not a GROUP BY), so this verb fetches a
    /// bounded page via `query_events` and aggregates by kind/actor/verb/profile
    /// in the handler. A window wider than this cap still returns counts up to the
    /// cap and sets `truncated: true` plus the true `window_event_total` rather
    /// than silently reporting a partial total as complete. At current event
    /// volumes this bound is not expected to bind in practice; widening the
    /// storage trait with a grouped-count method was intentionally deferred
    /// rather than contorting `EventStore` for a case that is not yet load-bearing.
    const MAX_WINDOW_EVENTS: u32 = 50_000;

    pub(crate) async fn handle_event_counts(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct EventCountsParams {
            actor: Option<String>,
            kind: Option<String>,
            // `Option`, not a required `String`: a bare-missing `since` must go through
            // the same named-field-plus-example-format error as a malformed one, not
            // serde's generic "missing field `since`" message.
            since: Option<String>,
            until: Option<String>,
        }
        let p: EventCountsParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let since_raw = p.since.as_deref().ok_or_else(|| {
            RuntimeError::InvalidInput(
                "missing `since`: required ISO-8601/RFC-3339 datetime; expected e.g. \
                 \"2026-07-01T00:00:00Z\""
                    .to_string(),
            )
        })?;
        let since_us = parse_rfc3339_micros("since", since_raw)?;
        let until_us = match p.until.as_deref() {
            Some(u) => parse_rfc3339_micros("until", u)?,
            None => Utc::now().timestamp_micros(),
        };

        let kind_filter = match p.kind.as_deref() {
            Some(k) => Some(
                k.parse::<khive_types::EventKind>()
                    .map_err(|e| RuntimeError::InvalidInput(format!("invalid `kind`: {e}")))?,
            ),
            None => None,
        };

        // Stored actor strings are prefixed (`actor:<kind>:<id>`). Callers naturally pass the
        // bare seat form (e.g. "lambda:khive"), which would silently match nothing against an
        // exact-match filter. Match either spelling by expanding the filter to both forms —
        // `EventFilter.actors` is an IN-list, so this is a pure OR, never a guess. A caller who
        // already passes the stored `actor:`-prefixed form keeps exact-match behavior.
        let actor_filters: Vec<String> = match p.actor.as_deref() {
            Some(a) if a.starts_with("actor:") => vec![a.to_string()],
            Some(a) => vec![a.to_string(), format!("actor:{a}")],
            None => Vec::new(),
        };

        let store = self.runtime.events(token)?;
        let filter = EventFilter {
            actors: actor_filters,
            kinds: kind_filter.into_iter().collect(),
            // Half-open window [since, until): `EventFilter.after` is a strict
            // `created_at > after`, so subtracting one microsecond from `since`
            // makes the boundary itself inclusive; `before` is already a strict
            // `created_at < before`, which is exactly the exclusive `until` we want.
            after: Some(since_us - 1),
            before: Some(until_us),
            ..EventFilter::default()
        };

        let page = store
            .query_events(
                filter,
                PageRequest {
                    offset: 0,
                    limit: Self::MAX_WINDOW_EVENTS,
                },
            )
            .await
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let window_event_total = page.total.unwrap_or(page.items.len() as u64);
        let truncated = window_event_total > page.items.len() as u64;

        let mut counts_by_kind: std::collections::BTreeMap<String, u64> =
            std::collections::BTreeMap::new();
        let mut counts_by_actor: std::collections::BTreeMap<String, u64> =
            std::collections::BTreeMap::new();
        let mut counts_by_verb: std::collections::BTreeMap<String, u64> =
            std::collections::BTreeMap::new();
        let mut by_profile: std::collections::BTreeMap<String, u64> =
            std::collections::BTreeMap::new();
        let mut counts_by_work_class: std::collections::BTreeMap<String, u64> =
            std::collections::BTreeMap::new();
        let mut total_cost_unit: i64 = 0;
        let mut cost_unit_by_verb: std::collections::BTreeMap<String, i64> =
            std::collections::BTreeMap::new();

        for event in &page.items {
            *counts_by_kind
                .entry(event.kind.name().to_string())
                .or_insert(0) += 1;
            *counts_by_actor.entry(event.actor.clone()).or_insert(0) += 1;
            *counts_by_verb.entry(event.verb.clone()).or_insert(0) += 1;
            if event.kind == khive_types::EventKind::FeedbackExplicit {
                let profile = event
                    .payload
                    .get("served_by_profile_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unspecified")
                    .to_string();
                *by_profile.entry(profile).or_insert(0) += 1;
            }
            if let Some(work_class) = event_work_class(&event.payload) {
                *counts_by_work_class
                    .entry(work_class.to_string())
                    .or_insert(0) += 1;
            }
            if let Some(cost_unit) = event_cost_unit(&event.payload) {
                total_cost_unit = total_cost_unit.saturating_add(cost_unit);
                let entry = cost_unit_by_verb.entry(event.verb.clone()).or_insert(0);
                *entry = entry.saturating_add(cost_unit);
            }
        }

        let mut result = json!({
            "since": micros_to_iso(since_us),
            "until": micros_to_iso(until_us),
            "actor": p.actor,
            "kind": p.kind,
            "total": page.items.len() as u64,
            "counts_by_kind": counts_by_kind,
            "counts_by_actor": counts_by_actor,
            "counts_by_verb": counts_by_verb,
            "truncated": truncated,
            "window_event_total": window_event_total,
        });
        if !by_profile.is_empty() {
            result["by_profile"] = json!(by_profile);
        }
        if !counts_by_work_class.is_empty() {
            result["counts_by_work_class"] = json!(counts_by_work_class);
        }
        if !cost_unit_by_verb.is_empty() {
            result["total_cost_unit"] = json!(total_cost_unit);
            result["cost_unit_by_verb"] = json!(cost_unit_by_verb);
        }
        Ok(result)
    }

    // ── brain.profiles ────────────────────────────────────────────────────

    pub(crate) async fn handle_profiles(&self, params: Value) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ProfilesParams {
            lifecycle: Option<String>,
        }
        let p: ProfilesParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let state = self.state.lock().unwrap();
        // Only expose the three public-facing lifecycle states.
        // `defined` and `registered` are internal implementation states that
        // callers cannot filter on; listing them in the error would confuse users.
        let filter_lc: Option<ProfileLifecycle> = p
            .lifecycle
            .as_deref()
            .map(|s| match s {
                "active" => Ok(ProfileLifecycle::Active),
                "inactive" => Ok(ProfileLifecycle::Inactive),
                "archived" => Ok(ProfileLifecycle::Archived),
                other => Err(RuntimeError::InvalidInput(format!(
                    "invalid lifecycle {other:?}; expected one of 'active', 'inactive', 'archived'"
                ))),
            })
            .transpose()?;

        // Sort by id for deterministic output regardless of HashMap iteration order.
        let mut profiles: Vec<&ProfileRecord> = state
            .profiles
            .values()
            .filter(|r| filter_lc.as_ref().is_none_or(|lc| &r.lifecycle == lc))
            .collect();
        profiles.sort_by(|a, b| a.id.cmp(&b.id));

        let items: Vec<Value> = profiles
            .iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "description": r.description,
                    "consumer_kind": r.consumer_kind,
                    "state_class": r.state_class,
                    "lifecycle": r.lifecycle,
                    "total_events": r.total_events,
                    "exploration_epoch": r.exploration_epoch,
                    "created_at": r.created_at,
                })
            })
            .collect();

        Ok(json!({ "count": items.len(), "profiles": items }))
    }

    // ── brain.profile ─────────────────────────────────────────────────────

    pub(crate) async fn handle_profile(&self, params: Value) -> Result<Value, RuntimeError> {
        // Accept both `profile_id` (canonical) and `id` (legacy alias).
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ProfileParams {
            profile_id: Option<String>,
            id: Option<String>,
        }
        let p: ProfileParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
        let profile_id = p
            .profile_id
            .or(p.id)
            .ok_or_else(|| RuntimeError::InvalidInput("missing field `profile_id`".into()))?;

        let state = self.state.lock().unwrap();
        let record = state
            .profiles
            .get(&profile_id)
            .ok_or_else(|| RuntimeError::NotFound(format!("profile {:?}", profile_id)))?;

        // Build per-section posterior summary for the response.
        let section_summary = if let Some(ss) = state.section_states.get(&profile_id) {
            let weights = derive_deterministic_weights(ss);
            let mut sections_json: serde_json::Map<String, Value> =
                serde_json::Map::with_capacity(ss.posteriors.len());
            for (section, posterior) in &ss.posteriors {
                let w = weights.get(section).copied().unwrap_or(0.0);
                sections_json.insert(
                    section.as_str().to_owned(),
                    json!({
                        "alpha": posterior.alpha(),
                        "beta": posterior.beta(),
                        "mean": posterior.mean(),
                        "variance": posterior.variance(),
                        "ess": posterior.effective_sample_size(),
                        "weight": w,
                    }),
                );
            }
            Value::Object(sections_json)
        } else {
            Value::Null
        };

        Ok(json!({
            "id": record.id,
            "description": record.description,
            "consumer_kind": record.consumer_kind,
            "state_class": record.state_class,
            "lifecycle": record.lifecycle,
            "total_events": record.total_events,
            "exploration_epoch": record.exploration_epoch,
            "created_at": record.created_at,
            "state_snapshot": record.state_snapshot,
            "section_posteriors": section_summary,
        }))
    }

    // ── brain.resolve ─────────────────────────────────────────────────────

    pub(crate) async fn handle_resolve(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ResolveParams {
            actor: Option<String>,
            namespace: Option<String>,
            consumer_kind: String,
        }
        let p: ResolveParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        // #741: an omitted `actor` defaults to the caller's dispatch identity so
        // this introspection verb reports what the serve path (#708) actually
        // does. Anonymous callers stay `None` and match only wildcard bindings;
        // an explicit `actor` param wins so evaluation tooling can query other
        // identities.
        let actor = match p.actor.as_deref() {
            Some(a) => Some(a),
            None => token.actor().binding_id(),
        };

        let state = self.state.lock().unwrap();
        // Return requested_consumer_kind + matched_consumer_kind + matched_binding so
        // callers can distinguish a real binding match from a system-default fallback.
        // ADR-035 tier-2 requires matched_binding=true; a false value means tier-3 fires.
        match state.resolve_with_match(actor, p.namespace.as_deref(), &p.consumer_kind) {
            Some((record, matched_kind, matched_binding)) => Ok(json!({
                "resolved_profile_id": record.id,
                "lifecycle": record.lifecycle,
                "requested_consumer_kind": p.consumer_kind,
                "matched_consumer_kind": matched_kind,
                "matched_binding": matched_binding,
            })),
            None => Err(RuntimeError::NotFound(format!(
                "no profile resolved for consumer_kind={:?}",
                p.consumer_kind
            ))),
        }
    }

    // ── brain.activate / deactivate / archive ─────────────────────────────

    pub(crate) async fn handle_activate(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        self.set_lifecycle(token, params, ProfileLifecycle::Active)
            .await
    }

    pub(crate) async fn handle_deactivate(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        self.set_lifecycle(token, params, ProfileLifecycle::Inactive)
            .await
    }

    pub(crate) async fn handle_archive(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        self.set_lifecycle(token, params, ProfileLifecycle::Archived)
            .await
    }

    /// Route a profile lifecycle transition through the durable-write helper
    /// (issue #457): the transition is validated and applied against a
    /// proposed state copy, and only takes effect in `self.state` after the
    /// brain event-log append + snapshot upsert commit in one transaction.
    pub(crate) async fn set_lifecycle(
        &self,
        token: &NamespaceToken,
        params: Value,
        lifecycle: ProfileLifecycle,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LifecycleParams {
            profile_id: String,
        }
        let p: LifecycleParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let event_kind = match lifecycle {
            ProfileLifecycle::Active => "brain.activate",
            ProfileLifecycle::Inactive => "brain.deactivate",
            ProfileLifecycle::Archived => "brain.archive",
            // set_lifecycle is only ever invoked (via handle_activate /
            // handle_deactivate / handle_archive) with one of the three
            // targets above; other lifecycle values are registry-only states
            // that no handler transitions to here.
            ProfileLifecycle::Defined | ProfileLifecycle::Registered => "brain.set_lifecycle",
        };
        let profile_id = p.profile_id;
        let event_payload = json!({ "profile_id": profile_id, "lifecycle": lifecycle.clone() });

        crate::persist::persist_brain_state_mutation(
            self.runtime.sql().as_ref(),
            token,
            &self.persistence,
            &self.state,
            crate::persist::BrainMutationEvent {
                profile_id: profile_id.clone(),
                event_kind: event_kind.to_string(),
                payload: event_payload,
            },
            ENTITY_CACHE_CAPACITY,
            move |state: &mut khive_brain_core::BrainState| -> Result<Value, RuntimeError> {
                let record = state
                    .profiles
                    .get_mut(&profile_id)
                    .ok_or_else(|| RuntimeError::NotFound(format!("profile {:?}", profile_id)))?;

                // Lifecycle DAG: defined → registered → active ⟷ inactive → archived
                // Terminal state: archived is read-only/audit-retained.
                // No transition OUT of archived is legal.
                // Active → archived is also illegal; must deactivate first.
                match (&record.lifecycle, &lifecycle) {
                    // archived is terminal — nothing leaves it
                    (ProfileLifecycle::Archived, _) => {
                        return Err(RuntimeError::InvalidInput(format!(
                            "profile {:?} is archived; archive is terminal and no transition is permitted",
                            profile_id
                        )));
                    }
                    // active → archived is illegal (must go through inactive first)
                    (ProfileLifecycle::Active, ProfileLifecycle::Archived) => {
                        return Err(RuntimeError::InvalidInput(format!(
                            "profile {:?} is active; deactivate it before archiving",
                            profile_id
                        )));
                    }
                    // all other transitions are permitted
                    _ => {}
                }

                record.lifecycle = lifecycle.clone();
                Ok(json!({
                    "profile_id": profile_id,
                    "lifecycle": lifecycle,
                }))
            },
        )
        .await
    }

    // ── brain.reset ───────────────────────────────────────────────────────

    pub(crate) async fn handle_reset(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        // profile_id is optional; defaults to "balanced-recall-v1".
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ResetParams {
            profile_id: Option<String>,
        }
        let p: ResetParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(format!("brain.reset: {e}")))?;
        let profile_id = p
            .profile_id
            .unwrap_or_else(|| "balanced-recall-v1".to_string());
        let event_payload = json!({ "profile_id": profile_id });

        // Route through the durable-write helper (issue #457): reset is
        // applied to a proposed state copy and only takes effect after the
        // brain event-log append + snapshot upsert commit.
        crate::persist::persist_brain_state_mutation(
            self.runtime.sql().as_ref(),
            token,
            &self.persistence,
            &self.state,
            crate::persist::BrainMutationEvent {
                profile_id: profile_id.clone(),
                event_kind: "brain.reset".to_string(),
                payload: event_payload,
            },
            ENTITY_CACHE_CAPACITY,
            move |state: &mut khive_brain_core::BrainState| -> Result<Value, RuntimeError> {
                // Validate profile exists.
                let lifecycle = state
                    .profiles
                    .get(&profile_id)
                    .ok_or_else(|| RuntimeError::NotFound(format!("profile {:?}", profile_id)))?
                    .lifecycle
                    .clone();

                // Reject archived profiles — archive is terminal and read-only.
                if lifecycle == ProfileLifecycle::Archived {
                    return Err(RuntimeError::InvalidInput(format!(
                        "profile {:?} is archived; archive is terminal and reset is not permitted",
                        profile_id
                    )));
                }

                if profile_id == "balanced-recall-v1" {
                    state.reset_posteriors();
                    // Sync profile record after reset so brain.profile reflects the restored
                    // domain-informed priors, not stale pre-reset values.
                    sync_balanced_recall_record(state);
                } else if state.profile_states.contains_key(&profile_id) {
                    // User-created Bayesian profile — reset its own posteriors.
                    state.reset_profile_posteriors(&profile_id);
                } else {
                    // Profile exists in registry but has no live state (e.g. non-Bayesian).
                    // Increment exploration_epoch on the record to mark the reset event.
                    if let Some(record) = state.profiles.get_mut(&profile_id) {
                        record.exploration_epoch += 1;
                    }
                }

                let epoch = if profile_id == "balanced-recall-v1" {
                    state.balanced_recall.exploration_epoch
                } else {
                    state.profiles[&profile_id].exploration_epoch
                };

                Ok(json!({
                    "reset": true,
                    "profile_id": profile_id,
                    "exploration_epoch": epoch,
                }))
            },
        )
        .await
    }

    /// Resolve the effective serving profile for feedback attribution
    /// (ADR-035 tiers 1-2, #697): explicit `served_by_profile_id` wins outright;
    /// otherwise resolve an actor+namespace-scoped binding via the same
    /// `resolve_with_match` table `brain.resolve` uses, before falling back to
    /// the system default. `brain.auto_feedback` forwards into `handle_feedback`
    /// unresolved so it inherits this without duplicating the logic.
    ///
    /// Consumer kind is fixed to `recall`: feedback routed directly through
    /// `brain.feedback`/`brain.auto_feedback` (as opposed to `memory.feedback`,
    /// which resolves its own tier and passes an explicit profile) always
    /// originates from the recall serve loop, so it must resolve against the
    /// same binding bucket the serve path used.
    fn resolve_effective_feedback_profile(
        &self,
        token: &NamespaceToken,
        explicit: Option<&str>,
    ) -> String {
        if let Some(profile_id) = explicit {
            return profile_id.to_string();
        }
        let actor = token.actor().binding_id();
        let namespace = token.namespace().as_str();
        let state = self.state.lock().unwrap();
        match state.resolve_with_match(actor, Some(namespace), ConsumerKind::Recall.as_str()) {
            Some((record, _matched_kind, true)) => record.id.clone(),
            _ => "balanced-recall-v1".to_string(),
        }
    }

    // ── brain.feedback ────────────────────────────────────────────────────

    pub(crate) async fn handle_feedback(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let feedback_start = Instant::now();

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct FeedbackParams {
            target_id: String,
            signal: String,
            served_by_profile_id: Option<String>,
            section_signals: Option<serde_json::Value>,
            // ADR-081 §6: scorer provenance, additive and optional. Must be
            // supplied together (validated just below) — one without the other
            // is rejected rather than silently coerced.
            scorer_run_id: Option<String>,
            serve_ledger_id: Option<String>,
        }
        let p: FeedbackParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        // ADR-081 §6: together-or-rejected. Absent-both is the unchanged,
        // ordinary feedback path; present-both enables dedup + ledger backfill
        // below. Exactly one present is invalid input.
        if p.scorer_run_id.is_some() != p.serve_ledger_id.is_some() {
            return Err(RuntimeError::InvalidInput(
                "scorer_run_id and serve_ledger_id must be supplied together".to_string(),
            ));
        }

        let target: uuid::Uuid =
            resolve_auto_feedback_target(&self.runtime, token, &p.target_id).await?;

        let signal = match p.signal.as_str() {
            "useful" => "useful",
            "not_useful" => "not_useful",
            "wrong" => "wrong",
            // Semantic event taxonomy names are also valid.
            "explicit_positive" => "explicit_positive",
            "explicit_negative" => "explicit_negative",
            "implicit_positive" => "implicit_positive",
            "implicit_negative" => "implicit_negative",
            "correction" => "correction",
            other => {
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown signal {other:?}; valid: useful | not_useful | wrong | \
                     explicit_positive | explicit_negative | implicit_positive | \
                     implicit_negative | correction"
                )))
            }
        };

        // Resolve the target by UUID with no namespace filter (ADR-007 Rule 2 /
        // PR-A1: by-ID ops are namespace-agnostic; authorization is the Gate's,
        // not a post-fetch namespace check). Rule 3b recall fans out actor-stamped
        // memories from other namespaces by design, so a primary-only check would
        // reject the feedback loop's own recalled targets. NotFound only for a
        // genuinely absent UUID.
        use khive_runtime::Resolved;
        // ADR-041 permits both entity and note signal targets; the resolved
        // substrate is threaded onto the emitted event below (#831) so the decoder can tell entity and
        // note signal observations apart instead of hard-coding entity.
        let target_substrate = match self
            .runtime
            .resolve_by_id(token, target)
            .await
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?
        {
            Some(Resolved::Entity(_)) => khive_types::SubstrateKind::Entity,
            Some(Resolved::Note(_)) => khive_types::SubstrateKind::Note,
            _ => {
                return Err(RuntimeError::NotFound(format!(
                    "target_id {target:?} not found"
                )));
            }
        };

        // Compute the effective serving profile (explicit, else a matching
        // actor+namespace binding, else the system default — #697), then
        // validate that it exists in the registry and is not Archived.
        let effective_profile =
            self.resolve_effective_feedback_profile(token, p.served_by_profile_id.as_deref());
        let effective_profile = effective_profile.as_str();
        {
            let state = self.state.lock().unwrap();
            match state.profiles.get(effective_profile) {
                None => {
                    return Err(RuntimeError::NotFound(format!(
                        "serving profile {:?} not found in profile registry",
                        effective_profile
                    )));
                }
                Some(rec) if rec.lifecycle == khive_brain_core::ProfileLifecycle::Archived => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "serving profile {:?} is archived; feedback cannot credit archived profiles",
                        effective_profile
                    )));
                }
                Some(_) => {}
            }
        }

        // Validate section_signals up front using the shared validator.
        // Malformed input is rejected here rather than silently dropped during replay.
        if let Some(ref ss) = p.section_signals {
            crate::validate_section_signals(ss)?;
        }

        let sql = self.runtime.sql();
        let now_us = Utc::now().timestamp_micros();

        // ADR-081 §6: when both scorer_run_id and serve_ledger_id are supplied,
        // resolve dedup + the accounting_profile_id fail-safe against the serve
        // ledger before the fold gate runs.
        let mut forced_zero_weight = false;
        if let (Some(scorer_run_id), Some(serve_ledger_id)) =
            (p.scorer_run_id.as_deref(), p.serve_ledger_id.as_deref())
        {
            match crate::serve_ledger::resolve(sql.as_ref(), serve_ledger_id, scorer_run_id).await?
            {
                crate::serve_ledger::ServeLedgerResolution::AlreadyGraded => {
                    return Ok(json!({
                        "emitted": false,
                        "deduped": true,
                        "verb": "brain.feedback",
                        "signal": signal,
                        "target_id": target.to_string(),
                        "serve_ledger_id": serve_ledger_id,
                        "scorer_run_id": scorer_run_id,
                    }));
                }
                crate::serve_ledger::ServeLedgerResolution::NotFound => {
                    return Err(RuntimeError::NotFound(format!(
                        "serve_ledger_id {:?} not found",
                        serve_ledger_id
                    )));
                }
                crate::serve_ledger::ServeLedgerResolution::Proceed {
                    accounting_profile_id,
                } => {
                    // ADR-081 §4 fail-safe: an implicit event whose serve row has
                    // no resolvable profile is recorded at zero weight — never
                    // folded under a guessed profile.
                    if accounting_profile_id.is_none() {
                        forced_zero_weight = true;
                    }
                }
            }
        }

        // ADR-081 §2/§6: when both scorer fields are present, the dedup claim
        // is made atomically inside the SAME `BEGIN IMMEDIATE` transaction as
        // the fold gate's mass check-and-write AND
        // the durable feedback event append (fold_gate.rs) — the `resolve`
        // check above is a non-atomic fast path only (it still handles the
        // common sequential case and the NotFound / forced-zero-weight
        // determination); this is the authoritative correctness mechanism
        // under concurrent duplicate submissions.
        let dedup_key: Option<(&str, &str)> =
            match (p.scorer_run_id.as_deref(), p.serve_ledger_id.as_deref()) {
                (Some(r), Some(l)) => Some((r, l)),
                _ => None,
            };

        // Base feedback payload, shared by every signal kind. The gated
        // (implicit) path below adds a "gate" key inside the atomic unit;
        // the ungated path (explicit/correction) adds nothing further.
        let mut base_data = json!({"signal": signal});
        if let Some(ref profile_id) = p.served_by_profile_id {
            base_data["served_by_profile_id"] = json!(profile_id);
        }
        if let Some(ref ss) = p.section_signals {
            base_data["section_signals"] = ss.clone();
        }
        if let (Some(ref scorer_run_id), Some(ref serve_ledger_id)) =
            (p.scorer_run_id.as_ref(), p.serve_ledger_id.as_ref())
        {
            base_data["scorer_run_id"] = json!(scorer_run_id);
            base_data["serve_ledger_id"] = json!(serve_ledger_id);
        }

        // ADR-081 §2: the bounded-mass fold gate applies only to implicit
        // signals. Explicit/correction signals are never gated (they are the
        // clamp's own comparator, ADR-081 §1) and — ADR-081 §6 — need not
        // join the event append into the fold transaction: there is no
        // dedup claim to keep consistent for them, so their append path
        // below is unchanged from before this fix.
        let is_gated_implicit = matches!(
            FeedbackEventKind::from_signal_str(signal),
            Some(FeedbackEventKind::ImplicitPositive) | Some(FeedbackEventKind::ImplicitNegative)
        );

        let event = if is_gated_implicit {
            let nominal_weight = FeedbackEventKind::from_signal_str(signal)
                .expect("is_gated_implicit implies from_signal_str is Some")
                .update_weight();
            // The forced-zero fail-safe path now
            // runs through the SAME atomic claim+append unit as the nominal
            // path below — only the mass fold write itself is skipped — so
            // it participates in the dedup claim instead of bypassing it.
            let gate_mode = if forced_zero_weight {
                crate::fold_gate::FeedbackGateMode::ForcedZero
            } else {
                crate::fold_gate::FeedbackGateMode::Nominal(nominal_weight)
            };

            let namespace = token.namespace().as_str().to_string();
            // ADR-096 per-request identity: stamp the resolved caller actor,
            // not a hardcoded pack name — matches the `kind:id` convention
            // the generic Audit event already uses (khive-runtime pack.rs
            // `build_audit_storage_event`). `ActorRef::anonymous()` resolves
            // to the explicit `"anonymous:local"` string rather than
            // silently mislabeling the event, so unresolved-actor calls are
            // still distinguishable from configured caller attribution.
            let actor_label = format!("{}:{}", token.actor().kind, token.actor().id);
            // `apply_fold_gate_and_append_event`'s `build_event` closure is
            // now required to be `'static` (ADR-067 Component A, Fork C
            // slice 2 — it is boxed into an `AtomicUnitOp` and may run
            // inside the writer task's `spawn_blocking`), so it must own
            // its captures rather than borrow `namespace`/`base_data` — a
            // separate `namespace_for_event` clone avoids conflicting with
            // the `&namespace` borrow passed as this call's own argument.
            let namespace_for_event = namespace.clone();
            let actor_label_for_event = actor_label.clone();
            let base_data_for_event = base_data.clone();
            let outcome = crate::fold_gate::apply_fold_gate_and_append_event(
                sql.as_ref(),
                &namespace,
                effective_profile,
                &target.to_string(),
                gate_mode,
                now_us,
                dedup_key,
                move |fold_outcome, forced_zero| {
                    let mut data = base_data_for_event;
                    let (effective_weight, mass_before, mass_after) = match fold_outcome {
                        Some(o) => (o.effective_weight, o.mass_before, o.mass_after),
                        None => (0.0, 0.0, 0.0),
                    };
                    data["gate"] = json!({
                        "effective_weight": effective_weight,
                        "mass_before": mass_before,
                        "mass_after": mass_after,
                        "forced_zero_weight": forced_zero,
                    });
                    let duration_us = feedback_start.elapsed().as_micros().max(1) as i64;
                    Event::new(
                        namespace_for_event,
                        "brain.feedback",
                        khive_types::EventKind::FeedbackExplicit,
                        target_substrate,
                        actor_label_for_event,
                    )
                    .with_target(target)
                    .with_payload(data)
                    .with_duration_us(duration_us)
                },
            )
            .await?;

            match outcome {
                // This is now the ONLY place a scorer-tagged
                // implicit call returns `deduped` — reached only when the
                // atomic unit's claim conflicted, meaning either a prior call
                // already committed claim+fold+event together, or (never)
                // a partially-failed prior attempt, since a failed attempt
                // rolls back its claim too.
                crate::fold_gate::GateAndAppendOutcome::Deduped => {
                    return Ok(json!({
                        "emitted": false,
                        "deduped": true,
                        "verb": "brain.feedback",
                        "signal": signal,
                        "target_id": target.to_string(),
                        "serve_ledger_id": p.serve_ledger_id,
                        "scorer_run_id": p.scorer_run_id,
                    }));
                }
                crate::fold_gate::GateAndAppendOutcome::Applied(result) => result.event,
            }
        } else {
            // Unchanged: explicit/correction signals (and non-scorer implicit
            // feedback would also land here if it ever reached this branch,
            // but `is_gated_implicit` already routes all implicit signals
            // above) append through the ordinary `EventStore` path, in its
            // own transaction — ADR-081 §6 does not require these to join
            // the fold gate's atomic unit.
            let duration_us = feedback_start.elapsed().as_micros().max(1) as i64;
            let event = Event::new(
                token.namespace().as_str().to_string(),
                "brain.feedback",
                khive_types::EventKind::FeedbackExplicit,
                target_substrate,
                format!("{}:{}", token.actor().kind, token.actor().id),
            )
            .with_target(target)
            .with_payload(base_data)
            .with_duration_us(duration_us);

            let store = self.runtime.events(token)?;
            store
                .append_event(event.clone())
                .await
                .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
            event
        };

        let serving_profile_owned = effective_profile.to_string();

        let brain_signal = interpret(&event);

        // Issue #458: apply the posterior mutation and make its durability
        // part of the success contract. `persist_brain_state_mutation` runs
        // this closure against a *proposed* state copy, then commits the
        // brain event-log append + snapshot upsert in one transaction —
        // `self.state` is only replaced with the proposed copy after that
        // commit succeeds. If persistence fails, this returns `Err` and
        // `self.state` is left completely untouched (no phantom in-memory
        // posterior update that vanishes on restart).
        crate::persist::persist_brain_state_mutation(
            self.runtime.sql().as_ref(),
            token,
            &self.persistence,
            &self.state,
            crate::persist::BrainMutationEvent {
                profile_id: serving_profile_owned.clone(),
                event_kind: event.verb.clone(),
                payload: serde_json::to_value(&event)
                    .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?,
            },
            ENTITY_CACHE_CAPACITY,
            {
                let brain_signal = brain_signal.clone();
                let serving_profile_owned = serving_profile_owned.clone();
                move |state: &mut khive_brain_core::BrainState| -> Result<(), RuntimeError> {
                    let serving_profile = serving_profile_owned.as_str();

                    if serving_profile == "balanced-recall-v1" {
                        state.balanced_recall.apply_signal(&brain_signal);
                        sync_balanced_recall_record(state);
                    } else if state.profile_states.contains_key(serving_profile) {
                        let ps = state
                            .profile_states
                            .get_mut(serving_profile)
                            .expect("key checked above");
                        ps.apply_signal(&brain_signal);
                        let snap = serde_json::to_value(ps.to_snapshot()).ok();
                        let total = ps.total_events;
                        if let Some(record) = state.profiles.get_mut(serving_profile) {
                            record.total_events = total;
                            record.state_snapshot = snap;
                        }
                    } else {
                        state.balanced_recall.apply_signal(&brain_signal);
                        sync_balanced_recall_record(state);
                    }

                    // Seed and backfill section posteriors, then apply the signal.
                    // Shared contract with the replay path — see `ensure_section_state_seeded`.
                    {
                        let section_state = crate::ensure_section_state_seeded(
                            &mut state.section_states,
                            &serving_profile_owned,
                        );
                        section_state.apply_signal(&brain_signal);
                    }

                    Ok(())
                }
            },
        )
        .await?;

        // lattice-router: build the context vector from the now-published live
        // state and forward through the fann network. This is a best-effort
        // routing computation independent of durability, so it reads from
        // `self.state` only after the mutation above has durably committed.
        // Consumption of routed weights lands with the engine route() seam (#343) and the
        // compose value-gate (#346); no per-event stdout/stderr spam.
        #[cfg(feature = "lattice-router")]
        {
            let state = self.state.lock().unwrap();
            let serving_profile = serving_profile_owned.as_str();
            let (rel, sal, temp) = if serving_profile == "balanced-recall-v1" {
                (
                    state.balanced_recall.relevance.clone(),
                    state.balanced_recall.salience.clone(),
                    state.balanced_recall.temporal.clone(),
                )
            } else if let Some(ps) = state.profile_states.get(serving_profile) {
                (
                    ps.relevance.clone(),
                    ps.salience.clone(),
                    ps.temporal.clone(),
                )
            } else {
                (
                    state.balanced_recall.relevance.clone(),
                    state.balanced_recall.salience.clone(),
                    state.balanced_recall.temporal.clone(),
                )
            };
            let sec = state
                .section_states
                .get(serving_profile)
                .map(|ss| ss.posteriors.clone());
            drop(state);
            let ctx = build_context_vector(&rel, &sal, &temp, sec.as_ref());
            let _routed = route_via_fann(&ctx);
        }

        // ADR-081 §6: "the fold... backfills the ledger row's grade." Runs after
        // the fold itself so a ledger-write failure never blocks the feedback
        // event from landing — intentionally non-fatal, unlike the fail-closed
        // brain-state persistence above (#458).
        if let (Some(scorer_run_id), Some(serve_ledger_id)) =
            (p.scorer_run_id.as_deref(), p.serve_ledger_id.as_deref())
        {
            if let Err(e) = crate::serve_ledger::backfill_grade(
                sql.as_ref(),
                serve_ledger_id,
                signal,
                now_us,
                scorer_run_id,
            )
            .await
            {
                eprintln!("[brain] serve ledger grade backfill failed (non-fatal): {e}");
            }
        }

        Ok(json!({
            "emitted": true,
            "event_id": event.id.to_string(),
            "verb": "brain.feedback",
            "signal": signal,
            "target_id": target.to_string(),
        }))
    }

    // ── brain.auto_feedback ───────────────────────────────────────────────

    /// Emit implicit feedback for the first `memory.recall` result (accepts 8-char prefix or full UUID).
    pub(crate) async fn handle_auto_feedback(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct AutoFeedbackParams {
            query: String,
            results: Vec<AutoFeedbackResult>,
            signal: Option<String>,
            served_by_profile_id: Option<String>,
            // ADR-081 §6: forwarded verbatim to brain.feedback, which owns the
            // together-or-rejected validation and the dedup/fold-gate logic.
            scorer_run_id: Option<String>,
            serve_ledger_id: Option<String>,
        }

        #[derive(Deserialize)]
        struct AutoFeedbackResult {
            id: String,
        }

        let p: AutoFeedbackParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
        if p.query.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "auto_feedback: `query` must not be empty".into(),
            ));
        }

        let Some(first) = p.results.first() else {
            return Ok(json!({
                "emitted": false,
                "verb": "brain.auto_feedback",
                "reason": "no_results",
            }));
        };

        let target = resolve_auto_feedback_target(&self.runtime, token, &first.id).await?;

        let mut feedback_params = json!({
            "target_id": target.to_string(),
            "signal": p.signal.as_deref().unwrap_or("implicit_positive"),
        });
        if let Some(ref profile_id) = p.served_by_profile_id {
            feedback_params["served_by_profile_id"] = json!(profile_id);
        }
        if let Some(ref scorer_run_id) = p.scorer_run_id {
            feedback_params["scorer_run_id"] = json!(scorer_run_id);
        }
        if let Some(ref serve_ledger_id) = p.serve_ledger_id {
            feedback_params["serve_ledger_id"] = json!(serve_ledger_id);
        }

        let mut out = self.handle_feedback(token, feedback_params).await?;
        out["verb"] = json!("brain.auto_feedback");
        out["feedback_verb"] = json!("brain.feedback");
        out["result_count"] = json!(p.results.len());
        Ok(out)
    }

    // ── brain.record_serve ────────────────────────────────────────────────

    /// ADR-081 §4/§5 (#394): append one serve-ledger row per target id. Never
    /// propagates a per-target write failure as a batch error — a serve-ledger
    /// append is best-effort accounting, not the recall response itself, so a
    /// single row's failure (or an exact-key duplicate, tolerated as `skipped`)
    /// must not poison the rest of the batch.
    pub(crate) async fn handle_record_serve(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RecordServeParams {
            consumer_kind: String,
            served_by_profile_id: Option<String>,
            target_ids: Vec<String>,
            query_raw: String,
            served_at: Option<i64>,
        }
        let p: RecordServeParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        if p.target_ids.is_empty() {
            return Ok(json!({
                "ok": true,
                "written": 0,
                "skipped": 0,
                "verb": "brain.record_serve",
            }));
        }

        let namespace = token.namespace().as_str().to_string();
        let query_class = crate::serve_ledger::compute_query_class(&p.query_raw);
        let served_at = p.served_at.unwrap_or_else(|| Utc::now().timestamp_micros());
        let sql = self.runtime.sql();

        let mut written = 0usize;
        let mut skipped = 0usize;
        for target_id in &p.target_ids {
            let row_id = uuid::Uuid::new_v4().to_string();
            match crate::serve_ledger::record_serve(
                sql.as_ref(),
                &row_id,
                &namespace,
                &p.consumer_kind,
                p.served_by_profile_id.as_deref(),
                None,
                None,
                target_id,
                &query_class,
                &p.query_raw,
                served_at,
            )
            .await
            {
                Ok(true) => written += 1,
                Ok(false) => skipped += 1,
                Err(e) => {
                    eprintln!(
                        "[brain] serve ledger write failed for target {target_id} (non-fatal): {e}"
                    );
                }
            }
        }

        Ok(json!({
            "ok": true,
            "written": written,
            "skipped": skipped,
            "query_class": query_class,
            "verb": "brain.record_serve",
        }))
    }

    // ── brain.emit (deprecated) ───────────────────────────────────────────

    /// Deprecated alias for `brain.feedback`; routes to `handle_feedback`.
    pub(crate) async fn handle_emit(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        self.handle_feedback(token, params).await
    }

    // ── brain.bind ────────────────────────────────────────────────────────

    pub(crate) async fn handle_bind(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct BindParams {
            profile_id: String,
            actor: Option<String>,
            namespace: Option<String>,
            consumer_kind: Option<String>,
            priority: Option<i32>,
        }
        let p: BindParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let actor = p.actor.unwrap_or_else(|| "*".into());
        let namespace = p.namespace.unwrap_or_else(|| "*".into());
        let consumer_kind = p.consumer_kind.unwrap_or_else(|| "*".into());

        // Validate that '*' is not used as a real value ('*' is reserved as the wildcard sentinel)
        for (field, val) in [
            ("actor", &actor),
            ("namespace", &namespace),
            ("consumer_kind", &consumer_kind),
        ] {
            if val.as_str() != "*" && val.contains('*') {
                return Err(RuntimeError::InvalidInput(format!(
                    "{field}: '*' is reserved as the wildcard sentinel and cannot appear inside a real value"
                )));
            }
        }

        // Secret gate: scan arbitrary text fields before writing.
        // Wildcard sentinel `*` is safe; real values are scanned.
        if actor != "*" {
            khive_runtime::secret_gate::check(&actor)?;
        }
        if namespace != "*" {
            khive_runtime::secret_gate::check(&namespace)?;
        }
        if consumer_kind != "*" {
            khive_runtime::secret_gate::check(&consumer_kind)?;
        }

        let profile_id = p.profile_id;
        let priority = p.priority.unwrap_or(0);
        let event_payload = json!({
            "profile_id": profile_id,
            "actor": actor,
            "namespace": namespace,
            "consumer_kind": consumer_kind,
            "priority": priority,
        });

        // Route through the durable-write helper (issue #457): the binding
        // change is applied to a proposed state copy and only takes effect
        // after the brain event-log append + snapshot upsert commit.
        crate::persist::persist_brain_state_mutation(
            self.runtime.sql().as_ref(),
            token,
            &self.persistence,
            &self.state,
            crate::persist::BrainMutationEvent {
                profile_id: profile_id.clone(),
                event_kind: "brain.bind".to_string(),
                payload: event_payload,
            },
            ENTITY_CACHE_CAPACITY,
            move |state: &mut khive_brain_core::BrainState| -> Result<Value, RuntimeError> {
                // Verify the profile exists and is not archived (archived = terminal, no new bindings).
                match state.profiles.get(&profile_id) {
                    None => {
                        return Err(RuntimeError::NotFound(format!("profile {:?}", profile_id)));
                    }
                    Some(record) if record.lifecycle == ProfileLifecycle::Archived => {
                        return Err(RuntimeError::InvalidInput(format!(
                            "profile {:?} is archived; bindings to archived profiles are not permitted",
                            profile_id
                        )));
                    }
                    Some(_) => {}
                }

                // Remove any existing binding for the same (actor, namespace, consumer_kind)
                state.bindings.retain(|b| {
                    !(b.actor == actor
                        && b.namespace == namespace
                        && b.consumer_kind == consumer_kind)
                });

                state.bindings.push(ProfileBinding {
                    actor: actor.clone(),
                    namespace: namespace.clone(),
                    consumer_kind: consumer_kind.clone(),
                    profile_id: profile_id.clone(),
                    priority,
                    created_at: Utc::now(),
                });

                Ok(json!({
                    "bound": true,
                    "profile_id": profile_id,
                    "actor": actor,
                    "namespace": namespace,
                    "consumer_kind": consumer_kind,
                }))
            },
        )
        .await
    }

    // ── brain.unbind ──────────────────────────────────────────────────────

    pub(crate) async fn handle_unbind(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct UnbindParams {
            profile_id: Option<String>,
            actor: Option<String>,
            namespace: Option<String>,
            consumer_kind: Option<String>,
        }
        let p: UnbindParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        // Require at least one non-null filter to prevent accidental bulk-delete.
        if p.profile_id.is_none()
            && p.actor.is_none()
            && p.namespace.is_none()
            && p.consumer_kind.is_none()
        {
            return Err(RuntimeError::InvalidInput(
                "unbind requires at least one filter; pass profile_id, actor, namespace, or consumer_kind".into(),
            ));
        }

        let event_payload = json!({
            "profile_id": p.profile_id,
            "actor": p.actor,
            "namespace": p.namespace,
            "consumer_kind": p.consumer_kind,
        });
        let event_profile_id = p.profile_id.clone().unwrap_or_else(|| "*".to_string());

        // Route through the durable-write helper (issue #457): the binding
        // removal is applied to a proposed state copy and only takes effect
        // after the brain event-log append + snapshot upsert commit.
        crate::persist::persist_brain_state_mutation(
            self.runtime.sql().as_ref(),
            token,
            &self.persistence,
            &self.state,
            crate::persist::BrainMutationEvent {
                profile_id: event_profile_id,
                event_kind: "brain.unbind".to_string(),
                payload: event_payload,
            },
            ENTITY_CACHE_CAPACITY,
            move |state: &mut khive_brain_core::BrainState| -> Result<Value, RuntimeError> {
                let before = state.bindings.len();

                state.bindings.retain(|b| {
                    let pid_match = p.profile_id.as_ref().is_none_or(|id| &b.profile_id == id);
                    let actor_match = p.actor.as_ref().is_none_or(|a| &b.actor == a);
                    let ns_match = p.namespace.as_ref().is_none_or(|n| &b.namespace == n);
                    let kind_match = p
                        .consumer_kind
                        .as_ref()
                        .is_none_or(|k| &b.consumer_kind == k);
                    // Retain if this binding does NOT match ALL of the provided filters.
                    // A filter that is absent (None) matches everything — only bindings
                    // satisfying every supplied criterion are removed.
                    !(pid_match && actor_match && ns_match && kind_match)
                });

                let removed = before - state.bindings.len();
                Ok(json!({ "unbound": removed }))
            },
        )
        .await
    }

    // ── brain.bindings ────────────────────────────────────────────────────

    pub(crate) async fn handle_bindings(&self, params: Value) -> Result<Value, RuntimeError> {
        // Inspection verb — list binding rows, optionally filtered.
        #[derive(Deserialize)]
        struct BindingsParams {
            profile_id: Option<String>,
            actor: Option<String>,
            namespace: Option<String>,
            consumer_kind: Option<String>,
        }
        let p: BindingsParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let state = self.state.lock().unwrap();
        let rows: Vec<Value> = state
            .bindings
            .iter()
            .filter(|b| {
                p.profile_id.as_ref().is_none_or(|id| &b.profile_id == id)
                    && p.actor.as_ref().is_none_or(|a| &b.actor == a)
                    && p.namespace.as_ref().is_none_or(|n| &b.namespace == n)
                    && p.consumer_kind
                        .as_ref()
                        .is_none_or(|k| &b.consumer_kind == k)
            })
            .map(|b| {
                json!({
                    "profile_id": b.profile_id,
                    "actor": b.actor,
                    "namespace": b.namespace,
                    "consumer_kind": b.consumer_kind,
                    "priority": b.priority,
                    "created_at": b.created_at,
                })
            })
            .collect();

        Ok(json!({ "count": rows.len(), "bindings": rows }))
    }

    // ── brain.create_profile ──────────────────────────────────────────────

    pub(crate) async fn handle_create_profile(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        // Allow external agents to create new profiles via MCP.
        // seed_priors: optional object for seeding section priors, or null for defaults.
        #[derive(Deserialize)]
        struct CreateProfileParams {
            name: String,
            description: Option<String>,
            consumer_kind: Option<String>,
            seed_priors: Option<serde_json::Value>,
        }
        let p: CreateProfileParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        // Validate profile-id grammar: trim, reject empty, enforce ASCII alphanumeric + hyphen.
        let name = p.name.trim().to_string();
        if name.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "name must not be empty or whitespace-only".into(),
            ));
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(RuntimeError::InvalidInput(format!(
                "name {:?} is invalid; profile IDs must match [a-zA-Z0-9-]+ (alphanumeric and hyphens only)",
                name
            )));
        }
        let p_name = name;

        // Validate consumer_kind — reject empty/whitespace and wildcard sentinel.
        let consumer_kind = p.consumer_kind.unwrap_or_else(|| "recall".into());
        let ck_trimmed = consumer_kind.trim();
        if ck_trimmed.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "consumer_kind must not be empty or whitespace".into(),
            ));
        }
        if ck_trimmed == "*" {
            return Err(RuntimeError::InvalidInput(
                "consumer_kind '*' is the wildcard sentinel and is not permitted for profile creation; provide a specific operation kind (e.g. \"recall\", \"search\")"
                    .into(),
            ));
        }

        let description = p
            .description
            .unwrap_or_else(|| format!("User-created profile: {}", p_name));

        // Secret gate: scan caller-supplied text before any write.
        // `p_name` is already constrained to [a-zA-Z0-9-]+ and cannot carry a secret.
        khive_runtime::secret_gate::check(&description)?;
        khive_runtime::secret_gate::check(&consumer_kind)?;
        if let Some(ref seed) = p.seed_priors {
            khive_runtime::secret_gate::check_json(seed)?;
        }
        let seed_priors = p.seed_priors;

        let event_payload = json!({
            "profile_id": p_name,
            "consumer_kind": consumer_kind,
            "description": description,
        });

        // Route through the durable-write helper (issue #457): the new
        // profile/state/section rows are applied to a proposed state copy
        // and only take effect after the brain event-log append + snapshot
        // upsert commit — so a persistence failure never leaves a phantom
        // profile that vanishes on restart.
        crate::persist::persist_brain_state_mutation(
            self.runtime.sql().as_ref(),
            token,
            &self.persistence,
            &self.state,
            crate::persist::BrainMutationEvent {
                profile_id: p_name.clone(),
                event_kind: "brain.create_profile".to_string(),
                payload: event_payload,
            },
            ENTITY_CACHE_CAPACITY,
            move |state: &mut khive_brain_core::BrainState| -> Result<Value, RuntimeError> {
                if state.profiles.contains_key(&p_name) {
                    return Err(RuntimeError::InvalidInput(format!(
                        "profile {:?} already exists",
                        p_name
                    )));
                }

                // Initialize live BalancedRecallState for this profile so that reset and
                // feedback can route to its actual posteriors rather than a metadata-only record.
                let ps = khive_brain_core::BalancedRecallState::new(ENTITY_CACHE_CAPACITY);
                let snap = serde_json::to_value(ps.to_snapshot()).ok();

                let record = ProfileRecord {
                    id: p_name.clone(),
                    description: description.clone(),
                    consumer_kind: consumer_kind.clone(),
                    state_class: "Bayesian".into(),
                    lifecycle: ProfileLifecycle::Inactive,
                    created_at: Utc::now(),
                    state_snapshot: snap,
                    total_events: 0,
                    exploration_epoch: 0,
                };

                // Seed section posteriors: parse explicit section_posteriors object if provided,
                // else use default informative priors.
                let section_state = if let Some(ref seed) = seed_priors {
                    if let Some(sp_obj) = seed.get("section_posteriors").and_then(|v| v.as_object())
                    {
                        let mut priors = std::collections::HashMap::new();
                        for (key, val) in sp_obj {
                            let st: SectionType = key.parse().map_err(|_| {
                                RuntimeError::InvalidInput(format!("unknown section type: {key:?}"))
                            })?;
                            let alpha =
                                val.get("alpha").and_then(|v| v.as_f64()).ok_or_else(|| {
                                    RuntimeError::InvalidInput(format!(
                                        "missing or invalid alpha for section {key:?}"
                                    ))
                                })?;
                            let beta =
                                val.get("beta").and_then(|v| v.as_f64()).ok_or_else(|| {
                                    RuntimeError::InvalidInput(format!(
                                        "missing or invalid beta for section {key:?}"
                                    ))
                                })?;
                            let posterior = khive_brain_core::BetaPosterior::try_new(alpha, beta)
                                .map_err(|e| {
                                RuntimeError::InvalidInput(format!(
                                    "invalid seed_priors for section {key:?}: {e}"
                                ))
                            })?;
                            let ess = posterior.effective_sample_size();
                            if ess > DEFAULT_ESS_CAP {
                                return Err(RuntimeError::InvalidInput(format!(
                                    "seed_priors for section {key:?}: alpha+beta ({ess}) exceeds \
                                     maximum allowed ESS ({DEFAULT_ESS_CAP}); use values where \
                                     alpha + beta <= {DEFAULT_ESS_CAP}"
                                )));
                            }
                            priors.insert(st, posterior);
                        }
                        SectionPosteriorState::from_priors(priors)
                    } else {
                        return Err(RuntimeError::InvalidInput(
                            "seed_priors must contain a 'section_posteriors' object".into(),
                        ));
                    }
                } else {
                    SectionPosteriorState::new()
                };

                state.profiles.insert(p_name.clone(), record);
                state.profile_states.insert(p_name.clone(), ps);
                state.section_states.insert(p_name.clone(), section_state);

                Ok(json!({
                    "created": true,
                    "profile_id": p_name,
                    "consumer_kind": consumer_kind,
                    "lifecycle": "inactive",
                    "description": description,
                }))
            },
        )
        .await
    }

    // ── brain.register_adapter ────────────────────────────────────────────

    pub(crate) async fn handle_register_adapter(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RegisterAdapterParams {
            adapter_id: String,
            content_hash: String,
            base_model_revision: String,
            metadata: Option<serde_json::Value>,
        }
        let p: RegisterAdapterParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let active_revision = std::env::var("KHIVE_BRAIN_BASE_MODEL_REVISION")
            .unwrap_or_else(|_| DEFAULT_BASE_MODEL_REVISION.to_string());

        if p.base_model_revision != active_revision {
            return Err(RuntimeError::InvalidInput(format!(
                "base_model_revision mismatch: expected {:?}, got {:?}",
                active_revision, p.base_model_revision
            )));
        }

        let mut props = serde_json::json!({
            "content_hash": p.content_hash,
            "base_model_revision": p.base_model_revision,
        });
        if let Some(serde_json::Value::Object(meta)) = p.metadata {
            let props_obj = props.as_object_mut().unwrap();
            for (k, v) in meta {
                if k != "content_hash" && k != "base_model_revision" {
                    props_obj.insert(k, v);
                }
            }
        }

        self.runtime
            .create_entity(
                token,
                "artifact",
                Some("adapter"),
                &p.adapter_id,
                None,
                Some(props),
                vec![],
            )
            .await?;

        Ok(json!({
            "registered": true,
            "adapter_id": p.adapter_id,
            "content_hash": p.content_hash,
            "base_model_revision": p.base_model_revision,
        }))
    }
}

/// Parse an ISO-8601/RFC-3339 datetime string into a microsecond epoch,
/// naming the offending field/value in the error rather than a bare parse
/// failure (`brain.event_counts`, ADR-103 Stage 1).
fn parse_rfc3339_micros(field: &'static str, value: &str) -> Result<i64, RuntimeError> {
    chrono::DateTime::parse_from_rfc3339(value.trim())
        .map(|dt| dt.with_timezone(&Utc).timestamp_micros())
        .map_err(|e| {
            RuntimeError::InvalidInput(format!(
                "invalid `{field}`: {value:?} is not a valid ISO-8601/RFC-3339 datetime ({e}); \
                 expected e.g. \"2026-07-01T00:00:00Z\""
            ))
        })
}

/// Extract `work_class` from an event payload for `brain.event_counts`'
/// `counts_by_work_class` split (ADR-103 Stage 1, ADR-103-resource-attribution-model.md:303).
///
/// Checks two paths, phase-payload first:
/// 1. `payload.work_class` — the shape `PhaseStarted`/`PhaseCompleted`/`PhaseCancelled`
///    events use today (`khive_storage::telemetry::{PhaseStartedPayload, ...}`).
/// 2. `payload.resource.work_class` — the shape successful-dispatch audit rows write:
///    `khive_runtime::cost_unit` constructs the resource object with `work_class` and the
///    runtime persists it on successful audit rows. Phase payloads keep precedence when
///    both paths are present.
///
/// Returns `None` (silently excluded from the split, not an error) when neither path is
/// present — the overwhelming majority of event kinds today.
fn event_work_class(payload: &Value) -> Option<&str> {
    payload
        .get("work_class")
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .get("resource")
                .and_then(|resource| resource.get("work_class"))
                .and_then(Value::as_str)
        })
}

/// Extract `cost_unit` from an event payload for `brain.event_counts`'
/// `total_cost_unit` / `cost_unit_by_verb` aggregation (ADR-103 Amendment 1).
///
/// Reads `payload.resource.cost_unit` only — unlike `work_class`, `cost_unit`
/// has no top-level payload shape: it is emitted exclusively by
/// `khive_runtime::cost_unit::resource_payload` on the per-dispatch audit
/// row's `resource` sub-object, never by the Phase* background-work events.
///
/// Returns `None` (silently excluded from the sum, not an error or a zero)
/// when the field is absent, which per Amendment 1's "absence has exactly
/// two meanings" rule is either a pre-Amendment-1 event or an
/// errored/denied dispatch.
fn event_cost_unit(payload: &Value) -> Option<i64> {
    payload
        .get("resource")
        .and_then(|resource| resource.get("cost_unit"))
        .and_then(Value::as_i64)
}

// ── lattice-router seam (#345 M1 / #352 M2) ──────────────────────────────────

/// Context-vector dimension for the lattice-fann router seam.
/// Layout:
///   [0..2]  = relevance  {mean, ess}
///   [2..4]  = salience   {mean, ess}
///   [4..6]  = temporal   {mean, ess}
///   [6..16] = 10 section posterior means in `SectionType::all()` order
#[cfg(feature = "lattice-router")]
const ROUTER_CONTEXT_DIM: usize = 16;

/// Build a 16-element context vector from live brain posteriors.
///
/// ESS values are stored as raw f32 casts.  Normalization is deferred to the
/// engine `route()` seam (#343) once the network is trained; documenting the
/// omission here so it is not forgotten.
///
/// When `sections` is `None` the 10 section slots are filled from
/// `SectionPosteriorState::default_priors()` means — deterministically neutral,
/// never arbitrary zeros.
#[cfg(feature = "lattice-router")]
fn build_context_vector(
    relevance: &BetaPosterior,
    salience: &BetaPosterior,
    temporal: &BetaPosterior,
    sections: Option<&std::collections::HashMap<SectionType, BetaPosterior>>,
) -> [f32; ROUTER_CONTEXT_DIM] {
    let mut v = [0.0f32; ROUTER_CONTEXT_DIM];
    v[0] = relevance.mean() as f32;
    v[1] = relevance.effective_sample_size() as f32;
    v[2] = salience.mean() as f32;
    v[3] = salience.effective_sample_size() as f32;
    v[4] = temporal.mean() as f32;
    v[5] = temporal.effective_sample_size() as f32;
    // Section slots: deterministic ordering via SectionType::all().
    // Pre-compute default priors unconditionally so that slots absent from a
    // caller-supplied partial map fall back to the configured prior mean, not
    // an arbitrary 0.5 that would disagree with the seeded posteriors.
    let default_priors = SectionPosteriorState::default_priors();
    let posteriors: &std::collections::HashMap<SectionType, BetaPosterior> =
        sections.unwrap_or(&default_priors);
    for (i, st) in SectionType::all().iter().enumerate() {
        let mean = posteriors
            .get(st)
            .or_else(|| default_priors.get(st))
            .map_or(0.5, |p| p.mean());
        v[6 + i] = mean as f32;
    }
    v
}

/// M1 seam: links lattice-fann and produces routed mixture weights.
/// The real engine `route(context_vector, available_adapters)` (lattice mixture-runtime,
/// #343) swaps in here when shipped; M1 proves the dependency links and the seam runs.
#[cfg(feature = "lattice-router")]
fn route_via_fann(context: &[f32]) -> Vec<f32> {
    use lattice_fann::{Activation, NetworkBuilder};
    const ADAPTER_SLOTS: usize = 4;
    let mut network = match NetworkBuilder::new()
        .input(ROUTER_CONTEXT_DIM)
        .hidden(8, Activation::ReLU)
        .output(ADAPTER_SLOTS, Activation::Softmax)
        .build()
    {
        Ok(n) => n,
        Err(e) => {
            eprintln!("[brain] lattice-router: network build failed (non-fatal): {e}");
            return Vec::new();
        }
    };
    match network.forward(context) {
        Ok(weights) => weights.to_vec(),
        Err(e) => {
            eprintln!("[brain] lattice-router: forward failed (non-fatal): {e}");
            Vec::new()
        }
    }
}

// ── brain.auto_feedback helpers ───────────────────────────────────────────────

/// Resolve an `id` from `memory.recall` output to a full UUID.
///
/// Accepts a 36-char UUID directly, or an 8-char hex prefix (Agent-mode short
/// form). Returns `InvalidInput` if neither form matches or the prefix is
/// ambiguous.
pub(crate) async fn resolve_auto_feedback_target(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    raw: &str,
) -> Result<uuid::Uuid, RuntimeError> {
    if let Ok(uuid) = raw.parse::<uuid::Uuid>() {
        return Ok(uuid);
    }
    if raw.len() >= 8 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return runtime
            .resolve_prefix(token, raw)
            .await
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "auto_feedback: no record matches id prefix: {raw:?}"
                ))
            });
    }
    Err(RuntimeError::InvalidInput(format!(
        "auto_feedback: invalid id {raw:?}; expected full UUID or 8-char hex prefix"
    )))
}

// ── DispatchHook impl ─────────────────────────────────────────────────────────

/// `BrainPack` as a post-dispatch hook.
///
/// When registered via `VerbRegistryBuilder::with_dispatch_hook`, every
/// successful verb dispatch calls `on_dispatch` with a synthesized `Event`.
/// The event is fed into `BalancedRecallFold::reduce`, updating the brain's
/// posteriors in real time — no polling required.
#[async_trait]
impl DispatchHook for BrainPack {
    async fn on_dispatch(&self, view: &EventView) {
        if view.event.verb.starts_with("brain.") {
            return;
        }

        let _gate = self.dispatch_gate.lock().await;

        let signal = interpret(&view.event);

        // Route the signal to the state bucket that owns view.event.namespace.
        // No event is silently dropped: cold and saved namespaces are updated
        // inside PersistenceTracker; only when the namespace is the active slot
        // do we need to apply to the shared BrainState.
        let target = {
            let mut tracker = self.persistence.lock().unwrap();
            tracker.route_signal(&view.event.namespace, &signal, ENTITY_CACHE_CAPACITY)
        };

        if matches!(target, crate::persist::ApplyTarget::ActiveSlot) {
            let mut state = self.state.lock().unwrap();
            state.balanced_recall.apply_signal(&signal);
            sync_balanced_recall_record(&mut state);
        }
    }
}

// ── PackRuntime dispatch ──────────────────────────────────────────────────────

#[async_trait]
impl khive_runtime::pack::PackRuntime for BrainPack {
    fn name(&self) -> &str {
        <BrainPack as khive_types::Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <BrainPack as khive_types::Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <BrainPack as khive_types::Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        BRAIN_HANDLERS
    }

    fn requires(&self) -> &'static [&'static str] {
        <BrainPack as khive_types::Pack>::REQUIRES
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        // Serialise the (ensure_loaded → handler) pair under the dispatch gate
        // so no concurrent dispatch for a different namespace can swap the
        // single shared BrainState slot between the two steps.
        //
        // Lock order: dispatch_gate (outermost) → persistence → state.
        // Nothing inside ensure_loaded or any handler acquires dispatch_gate,
        // so there is no lock-order cycle.
        let _gate = self.dispatch_gate.lock().await;

        self.ensure_loaded(token).await?;

        // In test builds, fire the interleaving hook (if any) between
        // ensure_loaded returning and the handler acquiring self.state.
        // This lets tests prove that without the gate a concurrent namespace
        // swap would corrupt the handler's view.
        #[cfg(test)]
        {
            let hook = crate::pack::DISPATCH_INTERLEAVE_HOOK.lock().unwrap().take();
            if let Some(h) = hook {
                let _ = h.reached_tx.send(());
                let _ = h.proceed_rx.await;
            }
        }

        match verb {
            // Assertive
            "brain.state" => self.handle_state(params).await,
            "brain.config" => self.handle_config(params).await,
            "brain.events" => self.handle_events(token, params).await,
            "brain.event_counts" => self.handle_event_counts(token, params).await,
            "brain.profiles" => self.handle_profiles(params).await,
            "brain.profile" => self.handle_profile(params).await,
            "brain.resolve" => self.handle_resolve(token, params).await,
            "brain.bindings" => self.handle_bindings(params).await,
            // Commissive
            "brain.activate" => self.handle_activate(token, params).await,
            "brain.deactivate" => self.handle_deactivate(token, params).await,
            "brain.archive" => self.handle_archive(token, params).await,
            "brain.reset" => self.handle_reset(token, params).await,
            "brain.feedback" => self.handle_feedback(token, params).await,
            "brain.auto_feedback" => self.handle_auto_feedback(token, params).await,
            "brain.record_serve" => self.handle_record_serve(token, params).await,
            // Declaration
            "brain.bind" => self.handle_bind(token, params).await,
            "brain.unbind" => self.handle_unbind(token, params).await,
            "brain.create_profile" => self.handle_create_profile(token, params).await,
            "brain.register_adapter" => self.handle_register_adapter(token, params).await,
            // Legacy
            "brain.emit" => self.handle_emit(token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "brain pack does not handle verb {verb:?}"
            ))),
        }
    }
}

#[cfg(all(test, feature = "lattice-router"))]
mod router_tests {
    use super::*;

    #[test]
    fn route_via_fann_emits_normalized_mixture() {
        let rel = BetaPosterior::new(7.0, 3.0);
        let sal = BetaPosterior::new(2.0, 8.0);
        let temp = BetaPosterior::new(1.0, 9.0);
        let ctx = build_context_vector(&rel, &sal, &temp, None);
        let weights = route_via_fann(&ctx);
        assert_eq!(weights.len(), 4);
        let sum: f32 = weights.iter().sum();
        assert!(
            (sum - 1.0).abs() < 1e-3,
            "softmax weights must sum to ~1, got {sum}"
        );
    }

    #[test]
    fn context_vector_reflects_posterior_state() {
        // High-relevance state.
        let rel_high = BetaPosterior::new(90.0, 10.0);
        let sal = BetaPosterior::new(2.0, 8.0);
        let temp = BetaPosterior::new(1.0, 9.0);
        let v_high = build_context_vector(&rel_high, &sal, &temp, None);

        // Low-relevance state.
        let rel_low = BetaPosterior::new(1.0, 9.0);
        let v_low = build_context_vector(&rel_low, &sal, &temp, None);

        // Vectors must differ.
        assert_ne!(
            v_high, v_low,
            "context vectors must differ for different posteriors"
        );

        // Dimension must be 16.
        assert_eq!(v_high.len(), ROUTER_CONTEXT_DIM);
        assert_eq!(v_low.len(), ROUTER_CONTEXT_DIM);

        // Slot 0 = relevance mean; slot 1 = relevance ESS — verify they mirror the input.
        assert!(
            (v_high[0] - rel_high.mean() as f32).abs() < 1e-5,
            "slot 0 must equal relevance mean (high); got {}",
            v_high[0]
        );
        assert!(
            (v_high[1] - rel_high.effective_sample_size() as f32).abs() < 1e-5,
            "slot 1 must equal relevance ESS; got {}",
            v_high[1]
        );
        assert!(
            (v_low[0] - rel_low.mean() as f32).abs() < 1e-5,
            "slot 0 must equal relevance mean (low); got {}",
            v_low[0]
        );

        // Slot 0 must differ substantially between high and low states.
        assert!(
            (v_high[0] - v_low[0]).abs() > 0.5,
            "relevance mean slot must differ by >0.5; high={} low={}",
            v_high[0],
            v_low[0]
        );

        // Section slots [6..16] filled from default priors when sections=None — never zero.
        for (i, &val) in v_high.iter().enumerate().skip(6) {
            assert!(
                val > 0.0,
                "section slot {i} must be non-zero (default priors); got {val}",
            );
        }
    }

    /// Regression gate: a `Some(map)` that omits one SectionType must fill the
    /// missing slot from the default prior mean, never from the bare value 0.5.
    ///
    /// Formalism has `BetaPosterior(1.5, 4.0)` → mean ≈ 0.273, which differs
    /// from 0.5 by more than 0.1 and is easy to distinguish.
    #[test]
    fn build_context_vector_uses_prior_mean_for_missing_section_slot() {
        // Build a partial posteriors map that intentionally omits Formalism.
        let mut partial = SectionPosteriorState::default_priors();
        partial.remove(&SectionType::Formalism);
        assert!(
            !partial.contains_key(&SectionType::Formalism),
            "Formalism must be absent from the test map before calling the helper"
        );

        let neutral = BetaPosterior::new(2.0, 2.0);
        let v = build_context_vector(&neutral, &neutral, &neutral, Some(&partial));

        let formalism_idx = SectionType::all()
            .iter()
            .position(|st| *st == SectionType::Formalism)
            .expect("Formalism must be in SectionType::all()");
        let slot_val = v[6 + formalism_idx];

        let prior_mean = SectionPosteriorState::default_priors()
            .get(&SectionType::Formalism)
            .expect("default_priors must include Formalism")
            .mean() as f32;

        assert!(
            (slot_val - prior_mean).abs() < 1e-5,
            "missing Formalism slot must use prior mean ({prior_mean:.4}), not bare 0.5; \
             got {slot_val:.4}"
        );
        assert!(
            (slot_val - 0.5_f32).abs() > 0.1,
            "slot must NOT fall back to the bare 0.5 value; got {slot_val:.4}"
        );
    }
}
