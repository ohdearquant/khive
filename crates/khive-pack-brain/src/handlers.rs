//! Verb handler implementations for `BrainPack`.

use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use khive_fold::{Fold, FoldContext};
use khive_runtime::{
    micros_to_iso, DispatchHook, EventView, KhiveRuntime, NamespaceToken, RuntimeError,
    VerbRegistry,
};
use khive_storage::event::{Event, EventFilter};
use khive_storage::types::PageRequest;
use khive_types::HandlerDef;

use crate::section::derive_deterministic_weights;
use crate::state::{
    ProfileBinding, ProfileLifecycle, ProfileRecord, SectionPosteriorState, SectionType,
};
use crate::{sync_balanced_recall_record, BrainPack, ENTITY_CACHE_CAPACITY};

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
                description: "Caller actor identifier. Defaults to \"*\" wildcard match.",
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
        description: "Move a profile to Active (start live update loop)",
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
        description: "Reset posteriors to priors (preserves event history)",
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
                description: "Recall result objects; the first object's note_id is credited.",
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
                    "alpha": posterior.alpha,
                    "beta": posterior.beta,
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
                        "alpha": posterior.alpha,
                        "beta": posterior.beta,
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

    pub(crate) async fn handle_resolve(&self, params: Value) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ResolveParams {
            actor: Option<String>,
            namespace: Option<String>,
            consumer_kind: String,
        }
        let p: ResolveParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let state = self.state.lock().unwrap();
        // Return requested_consumer_kind + matched_consumer_kind separately so
        // callers can distinguish a wildcard-binding match ("*") from an exact match.
        match state.resolve_with_match(p.actor.as_deref(), p.namespace.as_deref(), &p.consumer_kind)
        {
            Some((record, matched_kind)) => Ok(json!({
                "resolved_profile_id": record.id,
                "lifecycle": record.lifecycle,
                "requested_consumer_kind": p.consumer_kind,
                "matched_consumer_kind": matched_kind,
            })),
            None => Err(RuntimeError::NotFound(format!(
                "no profile resolved for consumer_kind={:?}",
                p.consumer_kind
            ))),
        }
    }

    // ── brain.activate / deactivate / archive ─────────────────────────────

    pub(crate) async fn handle_activate(&self, params: Value) -> Result<Value, RuntimeError> {
        self.set_lifecycle(params, ProfileLifecycle::Active).await
    }

    pub(crate) async fn handle_deactivate(&self, params: Value) -> Result<Value, RuntimeError> {
        self.set_lifecycle(params, ProfileLifecycle::Inactive).await
    }

    pub(crate) async fn handle_archive(&self, params: Value) -> Result<Value, RuntimeError> {
        self.set_lifecycle(params, ProfileLifecycle::Archived).await
    }

    pub(crate) async fn set_lifecycle(
        &self,
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

        let mut state = self.state.lock().unwrap();
        let record = state
            .profiles
            .get_mut(&p.profile_id)
            .ok_or_else(|| RuntimeError::NotFound(format!("profile {:?}", p.profile_id)))?;

        // Lifecycle DAG: defined → registered → active ⟷ inactive → archived
        // Terminal state: archived is read-only/audit-retained.
        // No transition OUT of archived is legal.
        // Active → archived is also illegal; must deactivate first.
        match (&record.lifecycle, &lifecycle) {
            // archived is terminal — nothing leaves it
            (ProfileLifecycle::Archived, _) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "profile {:?} is archived; archive is terminal and no transition is permitted",
                    p.profile_id
                )));
            }
            // active → archived is illegal (must go through inactive first)
            (ProfileLifecycle::Active, ProfileLifecycle::Archived) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "profile {:?} is active; deactivate it before archiving",
                    p.profile_id
                )));
            }
            // all other transitions are permitted
            _ => {}
        }

        record.lifecycle = lifecycle.clone();
        Ok(json!({
            "profile_id": p.profile_id,
            "lifecycle": lifecycle,
        }))
    }

    // ── brain.reset ───────────────────────────────────────────────────────

    pub(crate) async fn handle_reset(&self, params: Value) -> Result<Value, RuntimeError> {
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
        let mut state = self.state.lock().unwrap();

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
            sync_balanced_recall_record(&mut state);
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
        }
        let p: FeedbackParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let target: uuid::Uuid = p
            .target_id
            .parse()
            .map_err(|e| RuntimeError::InvalidInput(format!("invalid target_id: {e}")))?;

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

        // Validate target_id resolves to an existing record in this namespace.
        let resolved = self
            .runtime
            .resolve(token, target)
            .await
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
        if resolved.is_none() {
            return Err(RuntimeError::NotFound(format!(
                "target_id {:?} not found in namespace {:?}",
                target,
                token.namespace().as_str()
            )));
        }

        // Compute the effective serving profile (explicit or default), then validate
        // that it exists in the registry and is not Archived.
        let effective_profile = p
            .served_by_profile_id
            .as_deref()
            .unwrap_or("balanced-recall-v1");
        {
            let state = self.state.lock().unwrap();
            match state.profiles.get(effective_profile) {
                None => {
                    return Err(RuntimeError::NotFound(format!(
                        "serving profile {:?} not found in profile registry",
                        effective_profile
                    )));
                }
                Some(rec) if rec.lifecycle == crate::state::ProfileLifecycle::Archived => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "serving profile {:?} is archived; feedback cannot credit archived profiles",
                        effective_profile
                    )));
                }
                Some(_) => {}
            }
        }

        let mut data = json!({"signal": signal});
        if let Some(ref profile_id) = p.served_by_profile_id {
            data["served_by_profile_id"] = json!(profile_id);
        }
        if let Some(ref ss) = p.section_signals {
            data["section_signals"] = ss.clone();
        }

        let duration_us = feedback_start.elapsed().as_micros().max(1) as i64;
        let event = Event::new(
            token.namespace().as_str().to_string(),
            "brain.feedback",
            khive_types::EventKind::FeedbackExplicit,
            khive_types::SubstrateKind::Event,
            "brain",
        )
        .with_target(target)
        .with_payload(data)
        .with_duration_us(duration_us);

        let store = self.runtime.events(token)?;
        store
            .append_event(event.clone())
            .await
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let serving_profile_owned = p
            .served_by_profile_id
            .as_deref()
            .unwrap_or("balanced-recall-v1")
            .to_string();

        {
            let ctx = FoldContext::new();
            let mut state = self.state.lock().unwrap();
            let serving_profile = serving_profile_owned.as_str();

            if serving_profile == "balanced-recall-v1" {
                let current_recall = std::mem::replace(
                    &mut state.balanced_recall,
                    crate::state::BalancedRecallState::new(0),
                );
                let updated = self.fold.reduce(current_recall, &event, &ctx);
                state.balanced_recall = updated;
                sync_balanced_recall_record(&mut state);
            } else if state.profile_states.contains_key(serving_profile) {
                let current = state
                    .profile_states
                    .remove(serving_profile)
                    .expect("key checked above");
                let updated = self.fold.reduce(current, &event, &ctx);
                let snap = serde_json::to_value(updated.to_snapshot()).ok();
                let total = updated.total_events;
                state
                    .profile_states
                    .insert(serving_profile.to_string(), updated);
                if let Some(record) = state.profiles.get_mut(serving_profile) {
                    record.total_events = total;
                    record.state_snapshot = snap;
                }
            } else {
                let current_recall = std::mem::replace(
                    &mut state.balanced_recall,
                    crate::state::BalancedRecallState::new(0),
                );
                let updated = self.fold.reduce(current_recall, &event, &ctx);
                state.balanced_recall = updated;
                sync_balanced_recall_record(&mut state);
            }

            if let Some(section_state) = state.section_states.remove(serving_profile) {
                let updated = self.section_fold.reduce(section_state, &event, &ctx);
                state
                    .section_states
                    .insert(serving_profile.to_string(), updated);
            }
        }

        // Persist feedback to brain_event_log; batch-upsert snapshot when dirty threshold reached.
        if let Err(e) = crate::persist::persist_after_feedback(
            &self.runtime,
            token,
            &self.persistence,
            &self.state,
            &event,
            &serving_profile_owned,
        )
        .await
        {
            eprintln!("[brain] persistence failed (non-fatal): {e}");
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
        }

        #[derive(Deserialize)]
        struct AutoFeedbackResult {
            note_id: String,
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

        let target = resolve_auto_feedback_target(&self.runtime, token, &first.note_id).await?;

        let mut feedback_params = json!({
            "target_id": target.to_string(),
            "signal": p.signal.as_deref().unwrap_or("implicit_positive"),
        });
        if let Some(ref profile_id) = p.served_by_profile_id {
            feedback_params["served_by_profile_id"] = json!(profile_id);
        }

        let mut out = self.handle_feedback(token, feedback_params).await?;
        out["verb"] = json!("brain.auto_feedback");
        out["feedback_verb"] = json!("brain.feedback");
        out["result_count"] = json!(p.results.len());
        Ok(out)
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

    pub(crate) async fn handle_bind(&self, params: Value) -> Result<Value, RuntimeError> {
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

        let mut state = self.state.lock().unwrap();

        // Verify the profile exists and is not archived (archived = terminal, no new bindings).
        match state.profiles.get(&p.profile_id) {
            None => {
                return Err(RuntimeError::NotFound(format!(
                    "profile {:?}",
                    p.profile_id
                )));
            }
            Some(record) if record.lifecycle == ProfileLifecycle::Archived => {
                return Err(RuntimeError::InvalidInput(format!(
                    "profile {:?} is archived; bindings to archived profiles are not permitted",
                    p.profile_id
                )));
            }
            Some(_) => {}
        }

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

        // Remove any existing binding for the same (actor, namespace, consumer_kind)
        state.bindings.retain(|b| {
            !(b.actor == actor && b.namespace == namespace && b.consumer_kind == consumer_kind)
        });

        state.bindings.push(ProfileBinding {
            actor: actor.clone(),
            namespace: namespace.clone(),
            consumer_kind: consumer_kind.clone(),
            profile_id: p.profile_id.clone(),
            priority: p.priority.unwrap_or(0),
            created_at: Utc::now(),
        });

        Ok(json!({
            "bound": true,
            "profile_id": p.profile_id,
            "actor": actor,
            "namespace": namespace,
            "consumer_kind": consumer_kind,
        }))
    }

    // ── brain.unbind ──────────────────────────────────────────────────────

    pub(crate) async fn handle_unbind(&self, params: Value) -> Result<Value, RuntimeError> {
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

        let mut state = self.state.lock().unwrap();
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

    pub(crate) async fn handle_create_profile(&self, params: Value) -> Result<Value, RuntimeError> {
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

        let mut state = self.state.lock().unwrap();

        if state.profiles.contains_key(&p_name) {
            return Err(RuntimeError::InvalidInput(format!(
                "profile {:?} already exists",
                p_name
            )));
        }

        // Initialize live BalancedRecallState for this profile so that reset and
        // feedback can route to its actual posteriors rather than a metadata-only record.
        let ps = crate::state::BalancedRecallState::new(ENTITY_CACHE_CAPACITY);
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
        let section_state = if let Some(ref seed) = p.seed_priors {
            if let Some(sp_obj) = seed.get("section_posteriors").and_then(|v| v.as_object()) {
                let mut priors = std::collections::HashMap::new();
                for (key, val) in sp_obj {
                    let st: SectionType = key.parse().map_err(|_| {
                        RuntimeError::InvalidInput(format!("unknown section type: {key:?}"))
                    })?;
                    let alpha = val.get("alpha").and_then(|v| v.as_f64()).ok_or_else(|| {
                        RuntimeError::InvalidInput(format!(
                            "missing or invalid alpha for section {key:?}"
                        ))
                    })?;
                    let beta = val.get("beta").and_then(|v| v.as_f64()).ok_or_else(|| {
                        RuntimeError::InvalidInput(format!(
                            "missing or invalid beta for section {key:?}"
                        ))
                    })?;
                    if alpha <= 0.0 || beta <= 0.0 {
                        return Err(RuntimeError::InvalidInput(format!(
                            "alpha and beta must be positive for section {key:?}; got alpha={alpha}, beta={beta}"
                        )));
                    }
                    priors.insert(st, crate::state::BetaPosterior::new(alpha, beta));
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
    }
}

// ── brain.auto_feedback helpers ───────────────────────────────────────────────

/// Resolve a `note_id` from `memory.recall` output to a full UUID.
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
                    "auto_feedback: no record matches note_id prefix: {raw:?}"
                ))
            });
    }
    Err(RuntimeError::InvalidInput(format!(
        "auto_feedback: invalid note_id {raw:?}; expected full UUID or 8-char hex prefix"
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
        // Brain observes pack events only — it must never process its own
        // state-transition events. Skipping brain.* verbs here prevents
        // double-counting: handle_feedback already calls fold.reduce directly,
        // so the hook firing afterward would increment total_events a second time.
        if view.event.verb.starts_with("brain.") {
            return;
        }

        let ctx = FoldContext::new();
        let mut state = self.state.lock().unwrap();
        let current = std::mem::replace(
            &mut state.balanced_recall,
            crate::state::BalancedRecallState::new(0),
        );
        let updated = self.fold.reduce(current, &view.event, &ctx);
        state.balanced_recall = updated;

        // Sync profile record after every hook fire so that brain.profile
        // reflects the live total_events and state_snapshot.
        sync_balanced_recall_record(&mut state);
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
        self.ensure_loaded(token).await?;
        match verb {
            // Assertive
            "brain.state" => self.handle_state(params).await,
            "brain.config" => self.handle_config(params).await,
            "brain.events" => self.handle_events(token, params).await,
            "brain.profiles" => self.handle_profiles(params).await,
            "brain.profile" => self.handle_profile(params).await,
            "brain.resolve" => self.handle_resolve(params).await,
            "brain.bindings" => self.handle_bindings(params).await,
            // Commissive
            "brain.activate" => self.handle_activate(params).await,
            "brain.deactivate" => self.handle_deactivate(params).await,
            "brain.archive" => self.handle_archive(params).await,
            "brain.reset" => self.handle_reset(params).await,
            "brain.feedback" => self.handle_feedback(token, params).await,
            "brain.auto_feedback" => self.handle_auto_feedback(token, params).await,
            // Declaration
            "brain.bind" => self.handle_bind(params).await,
            "brain.unbind" => self.handle_unbind(params).await,
            "brain.create_profile" => self.handle_create_profile(params).await,
            // Legacy
            "brain.emit" => self.handle_emit(token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "brain pack does not handle verb {verb:?}"
            ))),
        }
    }
}
