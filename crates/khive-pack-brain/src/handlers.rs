//! Verb handler implementations for `BrainPack`.

use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use khive_runtime::time_anchor::anchor_date_to_earliest_instant;
use khive_runtime::{
    micros_to_iso, DispatchHook, EventAttribution, EventView, KhiveRuntime, Namespace,
    NamespaceToken, RuntimeError, VerbRegistry,
};
use khive_storage::event::{Event, EventFilter};
use khive_storage::types::PageRequest;
use khive_types::{HandlerDef, IdResolutionMode};

mod auto_feedback;
mod brain_handlers;
mod event_counts;
mod event_page;
#[cfg(feature = "lattice-router")]
mod router;

#[cfg(feature = "lattice-router")]
use router::*;

pub(crate) use event_counts::*;

#[cfg(test)]
#[path = "handlers/event_counts_read_budget_tests.rs"]
mod event_counts_read_budget_tests;

use crate::event::interpret;
pub(crate) use crate::event_counts_grouping::EventCountsAccumulator;
use crate::event_read_scope::event_actor_read_scope;
use crate::{sync_balanced_recall_record, BrainPack, ENTITY_CACHE_CAPACITY};
use khive_brain_core::derive_deterministic_weights;
#[cfg(feature = "lattice-router")]
use khive_brain_core::BetaPosterior;
use khive_brain_core::{
    ConsumerKind, FeedbackEventKind, ProfileBinding, ProfileLifecycle, ProfileRecord,
    SectionPosteriorState, SectionType, ServeAttribution, DEFAULT_ESS_CAP,
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
            resolution_mode: IdResolutionMode::NotApplicable,
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
            resolution_mode: IdResolutionMode::NotApplicable,
        }],
    },
    event_page::HANDLER,
    // MAINTENANCE, deliberately kept out of the description: the event plane is
    // ADR-103 Stage 1 (#724 Ask A) and the cost_unit stamp is ADR-103 Amendment 1,
    // landed in PR #927.
    HandlerDef {
        name: "brain.event_counts",
        description: "Windowed event counts grouped by kind, actor, and verb over the event \
            plane. Optional group_by=[\"verb\",\"actor\"] emits a nested counts_by_verb_and_actor \
            map over the same events; when truncated it is named counts_by_verb_and_actor_page_scoped. \
            Only that ordered pair is supported; omission/null adds no cross. Grouping reuses the \
            existing actor scope and event-row caps, with no distinct-cell budget. Use kind=\"audit\" \
            for a dispatch-audit census and exhaustive=true when the complete window is needed. \
            Non-success audit rows additionally split by their known payload.error_class in \
            counts_by_error_class; missing, malformed or unknown classes contribute only to \
            unclassified_error_count. Both keys are absent when no failed audit row was read, \
            and both use a _page_scoped suffix when truncated. Success and non-audit rows \
            never contribute, even if their payload carries error_class. Stored rows are unchanged. \
            feedback_explicit events additionally split by \
            served_by_profile_id (by_profile), originating verb \
            (feedback_by_originating_verb), by signal (counts_by_signal), and by profile crossed \
            with signal (by_profile_and_signal, keyed by served_by_profile_id then signal, \
            for per-profile negative-share); events \
            carrying a work_class (today: phase_started / \
            phase_completed / phase_cancelled payloads, checked before any future \
            payload.resource.work_class) split by counts_by_work_class. Events carrying \
            payload.resource.cost_unit (stamped on every successful verb dispatch) sum into \
            total_cost_unit and cost_unit_by_verb; events with no resource.cost_unit (events \
            older than that stamp, or errored/denied dispatches) simply \
            do not contribute. total_cost_unit is omitted, not zero-filled, when no event in the \
            window carries cost_unit. When truncated=true, every scalar total (total, \
            total_cost_unit) is over the fetched page only, same as the other counts_by_* \
            fields — and, to keep a page-scoped number from being read as the real window total, \
            each is renamed to an explicitly incomplete key (total_page_scoped, \
            total_cost_unit_page_scoped) instead of being returned under its normal name. \
            window_event_total always carries the true count regardless. \
            `kind`/`actor` filters are applied in SQL before the internal window cap \
            — `window_event_total` reflects the filtered total as counted at its own read \
            instant (an independent read from the returned rows) even when \
            truncated=true. For a per-actor count of a specific low-frequency kind, pass \
            `kind=` explicitly rather than reading it out of an unfiltered call's \
            counts_by_kind/counts_by_actor breakdown, which is capped over the mixed-kind \
            window and can undercount a low-frequency kind relative to noisier ones. The \
            unfiltered default view (no `kind`) segregates the high-volume `audit` kind from \
            the shared truncation budget so it cannot crowd other kinds out of counts_by_kind. \
            Pass exhaustive=true for a non-sampled full-window aggregate (paginates \
            internally; still one call) instead of a single bounded page — higher cost, use \
            for coverage-panel/audit-style queries. Aggregation is a point-in-time view of \
            the live event plane: rows appended while the call paginates may be excluded; \
            bound `until` in the past for a closed population. Exhaustive windows matching more than \
            2,000,000 events are rejected rather than returned as partial aggregates; narrow \
            since/until or add actor/kind filters. Scope defaults to the caller; named foreign \
            actors must be visible, and all_actors=true requires the serving brain.fleet_readers \
            allowlist. The window covers exactly one namespace: the caller's own, or the one named \
            by an explicit `namespace=` argument. The result's `scope` states which namespace the \
            answer covered and which others this caller may ask for the same way; rows written to \
            any other namespace are absent from these counts rather than counted as zero. \
            Counts consume KHIVE_REQUEST_READ_TIMEOUT_SECS (default 30s, maximum 86400s), \
            retaining any earlier request deadline across both stores and events-daemon IPC. \
            The daemon stops and settles the count when the caller's budget expires and the \
            connection closes; it does not enforce an independent deadline. A typed storage \
            timeout names the budget; no partial successful window_event_total is returned.",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            khive_types::ParamDef {
                name: "since",
                param_type: "string",
                required: true,
                description: "Window start, ISO-8601/RFC-3339 datetime (e.g. \"2026-07-01T00:00:00Z\"). Inclusive. A date-only value (\"2026-07-01\") anchors to that day's earliest instant in the configured display timezone.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "until",
                param_type: "string",
                required: false,
                description: "Window end, ISO-8601/RFC-3339 datetime. Exclusive. Defaults to now. A date-only value covers the whole named day: it anchors to the NEXT day's earliest instant in the configured display timezone.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "actor",
                param_type: "string",
                required: false,
                description: "Defaults to the caller; a named foreign actor must be visible to the caller. \
                    Ordinary actor ids match actor: plus the unchanged id; a historical bare alias also \
                    matches only when the id has no reserved runtime kind prefix (actor:, anonymous:, agent:). \
                    An explicit reserved-prefix value matches exactly and checks structural caller identity; \
                    visibility uses the id after one actor: prefix, otherwise the unchanged label. Default-scoped \
                    counts use one caller-label key.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "all_actors",
                param_type: "boolean",
                required: false,
                description: "Default false; true reads all actors only when the caller's actor id is in the serving brain.fleet_readers config and cannot be combined with actor.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "kind",
                param_type: "string",
                required: false,
                description: "Filter to a single EventKind (e.g. \"recall_executed\", \"feedback_explicit\"). Omit for all kinds.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "group_by",
                param_type: "array",
                required: false,
                description: "Optional ordered pair [\"verb\", \"actor\"] only. Returns a nested verb→actor count map over the same authorized event window; uses the _page_scoped key when truncated. Omission or null emits no cross. Reversed, repeated, unknown, or other dimensions are rejected.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "exhaustive",
                param_type: "boolean",
                required: false,
                description: "When true, paginate through every matching event instead of a \
                    single bounded page, returning a non-sampled full-window aggregate \
                    over a best-effort live view in one call (rows appended while the \
                    call paginates may be included or excluded). Higher cost than the default bounded page \
                    (internally issues multiple storage queries); intended for coverage-panel \
                    / audit-style queries over large windows. Windows above the 2,000,000-event \
                    exhaustive limit are rejected; narrow since/until or add filters. Default \
                    false.",
                resolution_mode: IdResolutionMode::NotApplicable,
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
            resolution_mode: IdResolutionMode::NotApplicable,
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
            resolution_mode: IdResolutionMode::NotApplicable,
        }],
    },
    HandlerDef {
        name: "brain.resolve",
        description: "Show which profile would serve the caller; a registered consumer kind without a profile returns matched_binding=false and null profile fields. A named foreign actor must be visible to the caller.",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            khive_types::ParamDef {
                name: "consumer_kind",
                param_type: "string",
                required: true,
                description: "Verb or operation type the caller is about to perform (e.g. \"recall\").",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "actor",
                param_type: "string",
                required: false,
                description: "Defaults to the caller's dispatch identity; a named foreign actor must be visible to the caller, and omitted anonymous actors match only wildcard bindings.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Namespace for binding resolution. Defaults to \"*\" wildcard match.",
                resolution_mode: IdResolutionMode::NotApplicable,
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
            resolution_mode: IdResolutionMode::NotApplicable,
        }],
    },
    HandlerDef {
        name: "brain.deactivate",
        description: "Move a profile to Inactive (lifecycle transition; retain state)",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[khive_types::ParamDef {
            name: "profile_id",
            param_type: "string",
            required: true,
            description: "Profile ID to deactivate.",
            resolution_mode: IdResolutionMode::NotApplicable,
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
            resolution_mode: IdResolutionMode::NotApplicable,
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
            description: "Profile ID to reset (must exist and be non-archived). Defaults to \"balanced-recall-v1\". Use brain.profiles() to list profiles.",
            resolution_mode: IdResolutionMode::NotApplicable,
        }],
    },
    HandlerDef {
        name: "brain.feedback",
        description: "Emit a FeedbackExplicit event into the shared log",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            khive_types::ParamDef {
                name: "id",
                param_type: "uuid",
                required: true,
                description: "Complete UUID or globally unique 8+ hex prefix of the memory note \
                              or entity the feedback applies to. Prefix resolution applies no \
                              namespace filter at all, which is a wider reach than the caller's \
                              visible set. Legacy `target_id` is also accepted; supplying both `id` and `target_id` is rejected.",
                resolution_mode: IdResolutionMode::UnscopedById,
            },
            khive_types::ParamDef {
                name: "signal",
                param_type: "string",
                required: true,
                description: "Feedback signal: \"useful\" | \"not_useful\" | \"wrong\" | \"explicit_positive\" | \"explicit_negative\" | \"implicit_positive\" | \"implicit_negative\" | \"correction\".",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "served_by_profile_id",
                param_type: "string",
                required: false,
                description: "Profile ID that served the result being rated. A known ID wins; an unknown ID falls back only to the caller's matching recall binding, otherwise not_found. The resolved ID is recorded in the event payload.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "serve_attribution",
                param_type: "string",
                required: false,
                description: "Serve-time attribution state: \"profile\" | \"unattributed\" | \"unspecified\". Unattributed implicit feedback is forced to zero weight; explicit/correction feedback is rejected. It never falls back to a binding/default profile.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "section_signals",
                param_type: "object",
                required: false,
                description: "Per-section feedback signals: {\"section_name\": \"useful\"|\"not_useful\"|\"wrong\"}. For knowledge_compose profiles.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "scorer_run_id",
                param_type: "string",
                required: false,
                // MAINTENANCE, deliberately kept out of the description: ADR-081.
                description: "Scorer pass identifier, half of the (scorer_run_id, serve_ledger_id) dedup key. Must be supplied together with serve_ledger_id and an implicit signal. The feedback target and profile must match the serve row.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "serve_ledger_id",
                param_type: "string",
                required: false,
                // MAINTENANCE, deliberately kept out of the description: ADR-081.
                description: "Id of the brain_serve_ledger row being graded. Must be supplied together with scorer_run_id and an implicit signal; binds the feedback target and profile, backfills the row's grade, and gates dedup.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "brain.auto_feedback",
        description: "Emit caller-attributed feedback for one recall result. Omitting signal \
            records no judgment and emits no feedback event; when signal is present, target_id must \
            identify exactly one object in results. signal=unjudged records serve telemetry \
            without training or consuming a serve-ledger grade.",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            khive_types::ParamDef {
                name: "query",
                param_type: "string",
                required: true,
                description: "Recall query that produced the results.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "results",
                param_type: "array",
                required: true,
                description: "Recall result objects retained as candidate context: each object has id (UUID or compact id), optionally full_id and served_by_profile_id. Match target_id by id or full_id on exactly one result object; equal aliases on that object count once. Supplied full_id must be a full UUID and determines canonical attribution; otherwise id is resolved. Bare id strings are rejected. No result is credited by rank position.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "target_id",
                param_type: "string",
                required: false,
                description: "Exact id (full UUID or compact id) or full_id of the result being judged. Required when signal is supplied and must match exactly one result object across both aliases; equal aliases on that object count once. Supplied full_id determines canonical attribution and must be a full UUID; otherwise id is resolved.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "signal",
                param_type: "string",
                required: false,
                description: "Feedback signal. unjudged appends feedback_unjudged telemetry without training; useful, not_useful, and wrong record judgments. Omission means abstain: no feedback event or posterior update.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "served_by_profile_id",
                param_type: "string",
                required: false,
                description: "Profile ID that served the recall. Judgments default like brain.feedback; unjudged preserves available provenance without resolving a default.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "serve_attribution",
                param_type: "string",
                required: false,
                description: "Serve-time attribution state. Top-level attribution fields form one pair; when neither is supplied, both are copied from the selected recall result.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "scorer_run_id",
                param_type: "string",
                required: false,
                // MAINTENANCE, deliberately kept out of the description: ADR-081.
                description: "Scorer pass identifier. Supply with serve_ledger_id. unjudged preserves the pair without claiming it; useful, not_useful, wrong, and implicit judgments claim it atomically.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "serve_ledger_id",
                param_type: "string",
                required: false,
                // MAINTENANCE, deliberately kept out of the description: ADR-081.
                description: "Serve row provenance, supplied with scorer_run_id. The selected target and profile must match. unjudged leaves its grade free for a later judgment, deduplicated by the pair.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                // MAINTENANCE, deliberately kept out of the description: this is the
                // ADR-007 Rev 6 escape hatch.
                description: "Exact feedback namespace override. The event and posterior fold are scoped to exactly this namespace; the default namespace state is unchanged. Invalid values are rejected.",
                resolution_mode: IdResolutionMode::NotApplicable,
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
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "served_by_profile_id",
                param_type: "string",
                required: false,
                description: "Profile ID resolved at serve time. Omitted when unresolved.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "target_ids",
                param_type: "array",
                required: true,
                description: "Note/entity ids that were served; one ledger row per id.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "query_raw",
                param_type: "string",
                required: true,
                description: "Raw query text; query_class is derived from this deterministically.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "served_at",
                param_type: "integer",
                required: false,
                description: "Serve timestamp in epoch microseconds. Defaults to now.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // MAINTENANCE, deliberately kept out of the description: this reuses the existing
    // ADR-103 Stage 1 PhaseStarted/work_class vocabulary rather than adding an EventKind
    // variant, which is why the work_class is a payload value and not a kind.
    HandlerDef {
        name: "brain.mark_turn",
        description: "Emit a PhaseStarted event (work_class=\"actor_turn\") carrying the \
            calling actor and a timestamp. A per-actor per-work-unit marker: \
            callers invoke this once per bounded unit of work (a wake, a turn) so \
            brain.event_counts's counts_by_work_class[\"actor_turn\"], grouped by actor, \
            gives a discipline-ratio denominator (e.g. feedback_explicit / actor_turn) that \
            is not biased toward whichever actor issues the most raw verb calls. \
            Best-effort — never fails the caller's turn.",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[khive_types::ParamDef {
            name: "label",
            param_type: "string",
            required: false,
            description: "Free-form label for this unit of work (e.g. \"wake\", \"turn\"), \
                recorded in the event payload's `phase` field for debugging. Does not affect \
                the work_class grouping, which stays fixed at \"actor_turn\".",
            resolution_mode: IdResolutionMode::NotApplicable,
        }],
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
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "actor",
                param_type: "string",
                required: false,
                description: "Actor identifier to match. Default \"*\" (all actors). Cannot contain \"*\" inside a real value.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Namespace to match. Default \"*\" (all namespaces).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "consumer_kind",
                param_type: "string",
                required: false,
                description: "Registered brain consumer kind to match. Default \"*\" (all kinds). Unknown kinds are rejected with the loaded valid set.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "priority",
                param_type: "integer",
                required: false,
                description: "Binding priority; higher wins when multiple bindings match (default 0).",
                resolution_mode: IdResolutionMode::NotApplicable,
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
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "actor",
                param_type: "string",
                required: false,
                description: "Remove bindings for this actor.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Remove bindings for this namespace.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "consumer_kind",
                param_type: "string",
                required: false,
                description: "Remove bindings for this consumer_kind.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "brain.bindings",
        description: "List the caller's profile binding rows; a named foreign actor must be visible to the caller.",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            khive_types::ParamDef {
                name: "profile_id",
                param_type: "string",
                required: false,
                description: "Filter bindings by profile ID.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "actor",
                param_type: "string",
                required: false,
                description: "Defaults to the caller's actor label; a named foreign actor must be visible to the caller.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Filter bindings by namespace.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "consumer_kind",
                param_type: "string",
                required: false,
                description: "Filter bindings by consumer_kind.",
                resolution_mode: IdResolutionMode::NotApplicable,
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
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "description",
                param_type: "string",
                required: false,
                description: "Human-readable description for this profile.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "consumer_kind",
                param_type: "string",
                required: false,
                description: "Operation kind this profile targets (e.g. \"recall\"). Default \"recall\".",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "seed_priors",
                param_type: "object",
                required: false,
                description: "Optional section priors only: {\"section_posteriors\": {\"overview\": {\"alpha\": 2.0, \"beta\": 2.0}}. Recall priors cannot be seeded at creation; other top-level keys are rejected.",
                resolution_mode: IdResolutionMode::NotApplicable,
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
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "content_hash",
                param_type: "string",
                required: true,
                description: "Content hash of the adapter weights for integrity verification.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "base_model_revision",
                param_type: "string",
                required: true,
                description: "Base model revision the adapter was trained against. Must match the active revision or registration is rejected.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "metadata",
                param_type: "object",
                required: false,
                description: "Optional additional metadata merged into entity properties.",
                resolution_mode: IdResolutionMode::NotApplicable,
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
                description: "Complete UUID or globally unique 8+ hex prefix of the record the \
                              feedback applies to. Prefix resolution is namespace-unfiltered \
                              under ADR-007.",
                resolution_mode: IdResolutionMode::UnscopedById,
            },
            khive_types::ParamDef {
                name: "signal",
                param_type: "string",
                required: true,
                description: "Feedback signal: \"useful\" | \"not_useful\" | \"wrong\". Deprecated: use brain.feedback instead.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            khive_types::ParamDef {
                name: "served_by_profile_id",
                param_type: "string",
                required: false,
                description: "Profile ID that served the result.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
];

// ── brain.auto_feedback helpers ───────────────────────────────────────────────

/// Resolve a recall/search handle to a full UUID under the dispatch token.
///
/// UUID parsing is namespace-blind. Prefix lookup spans the registry's configured
/// backends, preserves distinct-UUID ambiguity and propagates backend errors.
/// The feedback handler subsequently admits only live KG entities and notes;
/// event, edge, and private-table targets remain ineligible.
pub(crate) async fn resolve_auto_feedback_target(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    raw: &str,
) -> Result<uuid::Uuid, RuntimeError> {
    if let Ok(uuid) = raw.parse::<uuid::Uuid>() {
        return Ok(uuid);
    }
    if raw.len() >= 8 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return registry
            .resolve_kg_read_prefix(runtime, token, raw, false)
            .await?
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
/// successful non-brain verb dispatch calls `on_dispatch` with a synthetic
/// `EventView` whose observations are empty. The event is interpreted and
/// routed through `apply_dispatch_signal` for the active namespace, or queued
/// for its cold namespace. This path does not call `BalancedRecallFold::reduce`,
/// append an event, or write a snapshot, so it provides no durable replay.
#[async_trait]
impl DispatchHook for BrainPack {
    async fn on_dispatch(&self, view: &EventView) {
        if view.event.verb.starts_with("brain.") {
            return;
        }
        let signal = interpret(&view.event);
        if matches!(signal, khive_brain_core::BrainSignal::Irrelevant) {
            return;
        }

        // The gate is held over whole brain handlers to prevent cross-namespace
        // slot swaps. A normal verb must never wait behind one of those handlers.
        // A contended signal keeps its namespace and serving attribution in a
        // bounded handoff; the worker later routes it under the same gate.
        match self.dispatch_gate.try_lock() {
            Ok(_gate) if !self.hook_queue.worker_running() => {
                self.apply_hook_signal(&view.event.namespace, &signal);
            }
            Ok(_gate) => {
                self.defer_hook_signal(view.event.namespace.clone(), signal);
            }
            Err(_) => {
                self.defer_hook_signal(view.event.namespace.clone(), signal);
            }
        }
    }
}

// ── PackRuntime dispatch ──────────────────────────────────────────────────────

#[async_trait]
impl khive_runtime::pack::PackRuntime for BrainPack {
    khive_runtime::pack_runtime_metadata!();

    async fn apply_profile_section_feedback(
        &self,
        token: &NamespaceToken,
        profile_id: &str,
        section_signals: Value,
        target_attribution: Option<String>,
    ) -> Result<Value, RuntimeError> {
        self.apply_section_feedback(token, profile_id, section_signals, target_attribution)
            .await
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
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

        // A contended post-dispatch hook may have elected its background
        // worker while this dispatch was waiting for the gate. If we win the
        // gate first, apply that queued evidence before the handler observes
        // state. The worker remains elected and will drain later arrivals.
        for (namespace, signal) in self.hook_queue.drain_pending() {
            self.apply_hook_signal(&namespace, &signal);
        }

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
            "brain.event_page" => self.handle_event_page(token, params).await,
            "brain.event_counts" => self.handle_event_counts(token, params).await,
            "brain.profiles" => self.handle_profiles(params).await,
            "brain.profile" => self.handle_profile(params).await,
            "brain.resolve" => self.handle_resolve(token, params, registry).await,
            "brain.bindings" => self.handle_bindings(token, params).await,
            // Commissive
            "brain.activate" => self.handle_activate(token, params).await,
            "brain.deactivate" => self.handle_deactivate(token, params).await,
            "brain.archive" => self.handle_archive(token, params).await,
            "brain.reset" => self.handle_reset(token, params).await,
            "brain.feedback" => self.handle_feedback(token, params, registry).await,
            "brain.auto_feedback" => self.handle_auto_feedback(token, params, registry).await,
            "brain.record_serve" => self.handle_record_serve(token, params).await,
            "brain.mark_turn" => self.handle_mark_turn(token, params).await,
            // Declaration
            "brain.bind" => self.handle_bind(token, params, registry).await,
            "brain.unbind" => self.handle_unbind(token, params).await,
            "brain.create_profile" => self.handle_create_profile(token, params).await,
            "brain.register_adapter" => self.handle_register_adapter(token, params).await,
            // Legacy
            "brain.emit" => self.handle_emit(token, params, registry).await,
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

        // Dimension is 6 scalar slots plus one slot per section type.
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

        // Every section slot [6..] is filled from default priors when sections=None — never zero.
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
