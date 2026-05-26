pub mod event;
pub mod fold;
pub mod state;
pub mod tunable;

use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use khive_fold::{Fold, FoldContext};
use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    DispatchHook, EventView, KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry,
};
use khive_storage::event::{Event, EventFilter};
use khive_storage::types::PageRequest;
use khive_types::{HandlerDef, Pack, ParamDef, VerbCategory, Visibility};

use crate::fold::BalancedRecallFold;
use crate::state::{BrainState, ProfileBinding, ProfileLifecycle, ProfileRecord};

const ENTITY_CACHE_CAPACITY: usize = 10_000;

// ── Profile record sync helper ────────────────────────────────────────────────

/// Sync the `balanced-recall-v1` profile record to match the live
/// `balanced_recall` state.
///
/// Fix #356 (MAJ-003): called from both `handle_feedback` and `on_dispatch`
/// so the record is never stale regardless of which path updated the state.
/// Fix #295: also called from `handle_reset` so the profile record reflects
/// restored domain-informed priors immediately after reset.
fn sync_balanced_recall_record(state: &mut BrainState) {
    let total_ev = state.balanced_recall.total_events;
    let snap_val = serde_json::to_value(state.balanced_recall.to_snapshot()).ok();
    if let Some(record) = state.profiles.get_mut("balanced-recall-v1") {
        record.total_events = total_ev;
        record.state_snapshot = snap_val;
    }
}

// ── Handler table ─────────────────────────────────────────────────────────────

/// Brain pack verb surface per ADR-032 §11.
///
/// Visibility::Verb  = exposed on the MCP `request` tool.
/// Visibility::Subhandler = internal / operator-only.
///
/// ADR-025: illocutionary classification applied.
static BRAIN_HANDLERS: &[HandlerDef] = &[
    // ── Assertive (read) verbs ────────────────────────────────────────────
    HandlerDef {
        name: "brain.state",
        description: "Return current BrainState snapshot for inspection",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Assertive,
        params: &[],
    },
    HandlerDef {
        name: "brain.config",
        description: "Return projected config for a named pack parameter",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Assertive,
        params: &[ParamDef {
            name: "parameter",
            param_type: "string",
            required: false,
            description: "Specific parameter to query: \"recall::relevance_weight\" | \"recall::salience_weight\" | \"recall::temporal_weight\". Omit to return all.",
        }],
    },
    HandlerDef {
        name: "brain.events",
        description: "List recent brain-relevant events for debugging",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Assertive,
        params: &[ParamDef {
            name: "limit",
            param_type: "integer",
            required: false,
            description: "Maximum events to return (default 20, max 100).",
        }],
    },
    HandlerDef {
        name: "brain.profiles",
        description: "List profiles, optionally filtered by lifecycle",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[ParamDef {
            name: "lifecycle",
            param_type: "string",
            required: false,
            description: "Filter profiles by lifecycle state: \"active\" | \"inactive\" | \"archived\". Omit to return all.",
        }],
    },
    HandlerDef {
        name: "brain.profile",
        description: "Profile metadata, latest snapshot, current state summary",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[ParamDef {
            name: "profile_id",
            param_type: "string",
            required: true,
            description: "Profile ID string (e.g. \"balanced-recall-v1\"). NOT a UUID — use the string identifier. Alias: id.",
        }],
    },
    HandlerDef {
        name: "brain.resolve",
        description: "Show which profile would serve a caller context",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "consumer_kind",
                param_type: "string",
                required: true,
                description: "Verb or operation type the caller is about to perform (e.g. \"recall\").",
            },
            ParamDef {
                name: "actor",
                param_type: "string",
                required: false,
                description: "Caller actor identifier. Defaults to \"*\" wildcard match.",
            },
            ParamDef {
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
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[ParamDef {
            name: "profile_id",
            param_type: "string",
            required: true,
            description: "Profile ID to activate (e.g. \"balanced-recall-v1\").",
        }],
    },
    HandlerDef {
        name: "brain.deactivate",
        description: "Move a profile to Inactive (stop live updates, retain state)",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[ParamDef {
            name: "profile_id",
            param_type: "string",
            required: true,
            description: "Profile ID to deactivate.",
        }],
    },
    HandlerDef {
        name: "brain.archive",
        description: "Move a profile to Archived (read-only, audit-retained)",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[ParamDef {
            name: "profile_id",
            param_type: "string",
            required: true,
            description: "Profile ID to archive.",
        }],
    },
    HandlerDef {
        name: "brain.reset",
        description: "Reset posteriors to priors (preserves event history)",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[ParamDef {
            name: "profile_id",
            param_type: "string",
            required: true,
            description: "Profile ID to reset (must exist and be active). Use brain.profiles() to list profiles.",
        }],
    },
    HandlerDef {
        name: "brain.feedback",
        description: "Emit a FeedbackExplicit event into the shared log",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "target_id",
                param_type: "uuid",
                required: true,
                description: "UUID of the memory note or entity the feedback applies to.",
            },
            ParamDef {
                name: "signal",
                param_type: "string",
                required: true,
                description: "Feedback signal: \"useful\" | \"not_useful\" | \"wrong\".",
            },
            ParamDef {
                name: "served_by_profile_id",
                param_type: "string",
                required: false,
                description: "Profile ID that served the result being rated. Recorded in the event payload.",
            },
        ],
    },
    // ── Declaration verbs ─────────────────────────────────────────────────
    HandlerDef {
        name: "brain.bind",
        description: "Write a row in the profile resolution table",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "profile_id",
                param_type: "string",
                required: true,
                description: "Profile ID to bind (must exist).",
            },
            ParamDef {
                name: "actor",
                param_type: "string",
                required: false,
                description: "Actor identifier to match. Default \"*\" (all actors). Cannot contain \"*\" inside a real value.",
            },
            ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Namespace to match. Default \"*\" (all namespaces).",
            },
            ParamDef {
                name: "consumer_kind",
                param_type: "string",
                required: false,
                description: "Verb / operation kind to match. Default \"*\" (all kinds).",
            },
            ParamDef {
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
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "profile_id",
                param_type: "string",
                required: false,
                description: "Remove bindings for this profile ID. All filters use AND semantics. At least one filter is required.",
            },
            ParamDef {
                name: "actor",
                param_type: "string",
                required: false,
                description: "Remove bindings for this actor.",
            },
            ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Remove bindings for this namespace.",
            },
            ParamDef {
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
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "profile_id",
                param_type: "string",
                required: false,
                description: "Filter bindings by profile ID.",
            },
            ParamDef {
                name: "actor",
                param_type: "string",
                required: false,
                description: "Filter bindings by actor.",
            },
            ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Filter bindings by namespace.",
            },
            ParamDef {
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
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "name",
                param_type: "string",
                required: true,
                description: "Profile ID / name (alphanumeric, hyphens allowed, e.g. \"my-profile-v1\"). Must be unique.",
            },
            ParamDef {
                name: "description",
                param_type: "string",
                required: false,
                description: "Human-readable description for this profile.",
            },
            ParamDef {
                name: "consumer_kind",
                param_type: "string",
                required: false,
                description: "Operation kind this profile targets (e.g. \"recall\"). Default \"recall\".",
            },
        ],
    },
    // ── Legacy / internal ─────────────────────────────────────────────────
    HandlerDef {
        name: "brain.emit",
        description: "Manually emit a feedback event (deprecated; use brain.feedback)",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "target_id",
                param_type: "uuid",
                required: true,
                description: "UUID of the record the feedback applies to.",
            },
            ParamDef {
                name: "signal",
                param_type: "string",
                required: true,
                description: "Feedback signal: \"useful\" | \"not_useful\" | \"wrong\". Deprecated: use brain.feedback instead.",
            },
            ParamDef {
                name: "served_by_profile_id",
                param_type: "string",
                required: false,
                description: "Profile ID that served the result.",
            },
        ],
    },
];

// ── BrainPack ─────────────────────────────────────────────────────────────────

/// Brain pack — profile-oriented auto-tuning (ADR-032).
///
/// `BrainState` holds the profile registry. `BalancedRecallFold` drives the
/// v1 default profile. The old scalar `BrainState` design is superseded; see
/// ADR-032 §1 and the migration notes in `state.rs`.
pub struct BrainPack {
    runtime: KhiveRuntime,
    /// Profile registry + active balanced-recall state.
    state: Mutex<BrainState>,
    /// Fold for the built-in `balanced-recall-v1` profile.
    fold: BalancedRecallFold,
}

impl Pack for BrainPack {
    const NAME: &'static str = "brain";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = BRAIN_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

impl BrainPack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        let fold = BalancedRecallFold::new(ENTITY_CACHE_CAPACITY);
        let state = BrainState::new(ENTITY_CACHE_CAPACITY);
        Self {
            runtime,
            state: Mutex::new(state),
            fold,
        }
    }

    /// Public snapshot of the current `BrainState`.
    pub fn snapshot(&self) -> crate::state::BrainStateSnapshot {
        self.state.lock().unwrap().to_snapshot()
    }

    // ── brain.state ───────────────────────────────────────────────────────

    async fn handle_state(&self, _params: Value) -> Result<Value, RuntimeError> {
        let state = self.state.lock().unwrap();
        let snapshot = state.to_snapshot();
        serde_json::to_value(&snapshot).map_err(|e| RuntimeError::InvalidInput(e.to_string()))
    }

    // ── brain.config ──────────────────────────────────────────────────────

    async fn handle_config(&self, params: Value) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
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

    async fn handle_events(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
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
                    "created_at": e.created_at,
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

    async fn handle_profiles(&self, params: Value) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        struct ProfilesParams {
            lifecycle: Option<String>,
        }
        let p: ProfilesParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let state = self.state.lock().unwrap();
        // UE5-H1: only expose the three public-facing lifecycle states.
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

        let profiles: Vec<&ProfileRecord> = state
            .profiles
            .values()
            .filter(|r| filter_lc.as_ref().is_none_or(|lc| &r.lifecycle == lc))
            .collect();

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

    async fn handle_profile(&self, params: Value) -> Result<Value, RuntimeError> {
        // H4: accept both `profile_id` (canonical) and `id` (legacy alias).
        #[derive(Deserialize)]
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
        }))
    }

    // ── brain.resolve ─────────────────────────────────────────────────────

    async fn handle_resolve(&self, params: Value) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        struct ResolveParams {
            actor: Option<String>,
            namespace: Option<String>,
            consumer_kind: String,
        }
        let p: ResolveParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let state = self.state.lock().unwrap();
        // H3: return requested_consumer_kind + matched_consumer_kind separately so
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

    async fn handle_activate(&self, params: Value) -> Result<Value, RuntimeError> {
        self.set_lifecycle(params, ProfileLifecycle::Active).await
    }

    async fn handle_deactivate(&self, params: Value) -> Result<Value, RuntimeError> {
        self.set_lifecycle(params, ProfileLifecycle::Inactive).await
    }

    async fn handle_archive(&self, params: Value) -> Result<Value, RuntimeError> {
        self.set_lifecycle(params, ProfileLifecycle::Archived).await
    }

    async fn set_lifecycle(
        &self,
        params: Value,
        lifecycle: ProfileLifecycle,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
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

        // Lifecycle DAG (ADR-032 §10):
        //   defined   → registered → active ⟷ inactive → archived
        //                         ↗
        //
        // Terminal state: archived is read-only/audit-retained.
        // No transition OUT of archived is legal.
        //
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

    async fn handle_reset(&self, params: Value) -> Result<Value, RuntimeError> {
        // C1: profile_id is required; validate existence and reject archived.
        #[derive(Deserialize)]
        struct ResetParams {
            profile_id: String,
        }
        let p: ResetParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let mut state = self.state.lock().unwrap();

        // Validate profile exists.
        let lifecycle = state
            .profiles
            .get(&p.profile_id)
            .ok_or_else(|| RuntimeError::NotFound(format!("profile {:?}", p.profile_id)))?
            .lifecycle
            .clone();

        // Reject archived profiles — archive is terminal and read-only.
        if lifecycle == ProfileLifecycle::Archived {
            return Err(RuntimeError::InvalidInput(format!(
                "profile {:?} is archived; archive is terminal and reset is not permitted",
                p.profile_id
            )));
        }

        if p.profile_id == "balanced-recall-v1" {
            state.reset_posteriors();
            // Fix #295: sync profile record after reset so brain.profile reflects
            // the restored domain-informed priors, not stale pre-reset values.
            sync_balanced_recall_record(&mut state);
        } else if state.profile_states.contains_key(&p.profile_id) {
            // User-created Bayesian profile — reset its own posteriors.
            state.reset_profile_posteriors(&p.profile_id);
        } else {
            // Profile exists in registry but has no live state (e.g. non-Bayesian).
            // Increment exploration_epoch on the record to mark the reset event.
            if let Some(record) = state.profiles.get_mut(&p.profile_id) {
                record.exploration_epoch += 1;
            }
        }

        let epoch = if p.profile_id == "balanced-recall-v1" {
            state.balanced_recall.exploration_epoch
        } else {
            state.profiles[&p.profile_id].exploration_epoch
        };

        Ok(json!({
            "reset": true,
            "profile_id": p.profile_id,
            "exploration_epoch": epoch,
        }))
    }

    // ── brain.feedback ────────────────────────────────────────────────────

    async fn handle_feedback(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        struct FeedbackParams {
            target_id: String,
            signal: String,
            served_by_profile_id: Option<String>,
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
            other => {
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown signal {other:?}; valid: useful | not_useful | wrong"
                )))
            }
        };

        // C4: validate target_id resolves to an existing record in this namespace.
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

        // C4: compute the effective serving profile (explicit or default), then validate
        // that it exists in the registry and is not Archived (ADR-032 §1106).
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

        let event = Event::new(
            token.namespace().as_str().to_string(),
            "brain.feedback",
            khive_types::EventKind::FeedbackExplicit,
            khive_types::SubstrateKind::Event,
            "brain",
        )
        .with_target(target)
        .with_payload(data);

        let store = self.runtime.events(token)?;
        store
            .append_event(event.clone())
            .await
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let ctx = FoldContext::new();
        let mut state = self.state.lock().unwrap();

        // Route feedback to the served profile's state, or to the built-in default.
        let serving_profile = p
            .served_by_profile_id
            .as_deref()
            .unwrap_or("balanced-recall-v1");

        if serving_profile == "balanced-recall-v1" {
            // Update the built-in balanced-recall state.
            let current_recall = std::mem::replace(
                &mut state.balanced_recall,
                crate::state::BalancedRecallState::new(0),
            );
            let updated = self.fold.reduce(current_recall, &event, &ctx);
            state.balanced_recall = updated;
            // Fix #356 (MAJ-003): sync profile record metadata via shared helper.
            sync_balanced_recall_record(&mut state);
        } else if state.profile_states.contains_key(serving_profile) {
            // Update the user-created profile's own state.
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
            // Profile exists in registry but has no live state — fall back to built-in.
            let current_recall = std::mem::replace(
                &mut state.balanced_recall,
                crate::state::BalancedRecallState::new(0),
            );
            let updated = self.fold.reduce(current_recall, &event, &ctx);
            state.balanced_recall = updated;
            sync_balanced_recall_record(&mut state);
        }

        Ok(json!({
            "emitted": true,
            "event_id": event.id.to_string(),
            "verb": "brain.feedback",
            "signal": signal,
            "target_id": target.to_string(),
        }))
    }

    // ── brain.emit (deprecated) ───────────────────────────────────────────

    /// Deprecated: use `brain.feedback`. Kept for backward-compat; routes to
    /// `handle_feedback` with the same parameters.
    async fn handle_emit(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        self.handle_feedback(token, params).await
    }

    // ── brain.bind ────────────────────────────────────────────────────────

    async fn handle_bind(&self, params: Value) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
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

        // Verify the profile exists and is not archived (C3: archived = terminal, no new bindings).
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

        // Validate that '*' is not used as a real value (ADR-032 §10 wildcard sentinel)
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

    async fn handle_unbind(&self, params: Value) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        struct UnbindParams {
            profile_id: Option<String>,
            actor: Option<String>,
            namespace: Option<String>,
            consumer_kind: Option<String>,
        }
        let p: UnbindParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        // C2: require at least one non-null filter to prevent accidental bulk-delete.
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

    async fn handle_bindings(&self, params: Value) -> Result<Value, RuntimeError> {
        // H2: inspection verb — list binding rows, optionally filtered.
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

    async fn handle_create_profile(&self, params: Value) -> Result<Value, RuntimeError> {
        // H1: allow external agents to create new profiles via MCP.
        #[derive(Deserialize)]
        struct CreateProfileParams {
            name: String,
            description: Option<String>,
            consumer_kind: Option<String>,
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

        // High: validate consumer_kind — reject empty/whitespace and wildcard sentinel.
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

        state.profiles.insert(p_name.clone(), record);
        state.profile_states.insert(p_name.clone(), ps);

        Ok(json!({
            "created": true,
            "profile_id": p_name,
            "consumer_kind": consumer_kind,
            "lifecycle": "inactive",
            "description": description,
        }))
    }
}

// ── Inventory self-registration ───────────────────────────────────────────────

struct BrainPackFactory;

impl khive_runtime::PackFactory for BrainPackFactory {
    fn name(&self) -> &'static str {
        "brain"
    }

    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(BrainPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&BrainPackFactory) }

// ── PackRuntime impl ──────────────────────────────────────────────────────────

#[async_trait]
impl PackRuntime for BrainPack {
    fn name(&self) -> &str {
        <BrainPack as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <BrainPack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <BrainPack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        BRAIN_HANDLERS
    }

    fn requires(&self) -> &'static [&'static str] {
        <BrainPack as Pack>::REQUIRES
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
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
        // Fix #357 (MAJ-004): Brain observes pack events only — it must never
        // process its own state-transition events (ADR-032 §1). Skipping
        // brain.* verbs here prevents double-counting: handle_feedback already
        // calls fold.reduce directly, so the hook firing afterward would
        // increment total_events a second time.
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

        // Fix #356 (MAJ-003): sync profile record after every hook fire so that
        // brain.profile reflects the live total_events and state_snapshot.
        sync_balanced_recall_record(&mut state);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::{Namespace, VerbRegistryBuilder};
    use serde_json::json;

    fn make_pack() -> (BrainPack, KhiveRuntime) {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let pack = BrainPack::new(rt.clone());
        (pack, rt)
    }

    fn empty_registry() -> VerbRegistry {
        VerbRegistryBuilder::new()
            .build()
            .expect("empty registry builds successfully")
    }

    /// Create a real entity in the runtime and return its UUID string.
    /// Used by feedback tests that need a valid target_id (C4 validation).
    async fn create_test_entity(rt: &KhiveRuntime, token: &NamespaceToken) -> String {
        let entity = rt
            .create_entity(token, "concept", None, "test-target", None, None, vec![])
            .await
            .expect("create test entity");
        entity.id.to_string()
    }

    #[tokio::test]
    async fn dispatch_unknown_verb_returns_invalid_input() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let err = pack
            .dispatch(
                "brain.unknown",
                json!({}),
                &registry,
                &rt.authorize(Namespace::local()),
            )
            .await
            .unwrap_err();
        if let RuntimeError::InvalidInput(msg) = &err {
            assert!(
                msg.contains("brain.unknown"),
                "expected verb name in error: {msg}"
            );
        } else {
            panic!("expected InvalidInput, got {err:?}");
        }
    }

    #[tokio::test]
    async fn dispatch_reset_returns_true_and_increments_epoch() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let result = pack
            .dispatch(
                "brain.reset",
                json!({"profile_id": "balanced-recall-v1"}),
                &registry,
                &rt.authorize(Namespace::local()),
            )
            .await
            .unwrap();
        assert_eq!(result["reset"], json!(true));
        assert_eq!(result["exploration_epoch"], json!(1u64));
        assert_eq!(result["profile_id"], json!("balanced-recall-v1"));
    }

    #[tokio::test]
    async fn dispatch_reset_no_args_returns_invalid_input() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let err = pack
            .dispatch(
                "brain.reset",
                json!({}),
                &registry,
                &rt.authorize(Namespace::local()),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "reset with no args must return InvalidInput, got {err:?}"
        );
    }

    #[tokio::test]
    async fn dispatch_reset_nonexistent_profile_returns_not_found() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let err = pack
            .dispatch(
                "brain.reset",
                json!({"profile_id": "ghost-profile"}),
                &registry,
                &rt.authorize(Namespace::local()),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::NotFound(_)),
            "reset on nonexistent profile must return NotFound, got {err:?}"
        );
    }

    #[tokio::test]
    async fn dispatch_reset_archived_profile_returns_invalid_input() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Archive the profile via the lifecycle DAG
        pack.dispatch(
            "brain.deactivate",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
        pack.dispatch(
            "brain.archive",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        let err = pack
            .dispatch(
                "brain.reset",
                json!({"profile_id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        if let RuntimeError::InvalidInput(msg) = &err {
            assert!(
                msg.contains("archived") || msg.contains("terminal"),
                "reset on archived profile must mention 'archived' or 'terminal'; got: {msg}"
            );
        } else {
            panic!("reset on archived profile must return InvalidInput, got {err:?}");
        }
    }

    #[tokio::test]
    async fn dispatch_feedback_invalid_signal_returns_invalid_input() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let target = "00000000-0000-0000-0000-000000000001";
        let err = pack
            .dispatch(
                "brain.feedback",
                json!({"target_id": target, "signal": "bad_signal"}),
                &registry,
                &rt.authorize(Namespace::local()),
            )
            .await
            .unwrap_err();
        if let RuntimeError::InvalidInput(msg) = &err {
            assert!(
                msg.contains("bad_signal"),
                "expected signal name in error: {msg}"
            );
            assert!(
                msg.contains("valid"),
                "expected hint about valid values: {msg}"
            );
        } else {
            panic!("expected InvalidInput, got {err:?}");
        }
    }

    #[tokio::test]
    async fn dispatch_state_returns_snapshot_fields() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let result = pack
            .dispatch(
                "brain.state",
                json!({}),
                &registry,
                &rt.authorize(Namespace::local()),
            )
            .await
            .unwrap();
        assert!(result.get("profiles").is_some(), "missing profiles");
        assert!(
            result.get("balanced_recall").is_some(),
            "missing balanced_recall"
        );
        assert!(result.get("bindings").is_some(), "missing bindings");
    }

    #[tokio::test]
    async fn dispatch_profiles_returns_default_profile() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let result = pack
            .dispatch(
                "brain.profiles",
                json!({}),
                &registry,
                &rt.authorize(Namespace::local()),
            )
            .await
            .unwrap();
        let profiles = result["profiles"].as_array().unwrap();
        assert!(!profiles.is_empty(), "expected at least one profile");
        assert_eq!(profiles[0]["id"], json!("balanced-recall-v1"));
    }

    #[tokio::test]
    async fn dispatch_profiles_filtered_by_lifecycle() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let result = pack
            .dispatch(
                "brain.profiles",
                json!({"lifecycle": "active"}),
                &registry,
                &rt.authorize(Namespace::local()),
            )
            .await
            .unwrap();
        let profiles = result["profiles"].as_array().unwrap();
        for p in profiles {
            assert_eq!(p["lifecycle"], json!("active"));
        }
    }

    #[tokio::test]
    async fn dispatch_profile_returns_profile_details() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let result = pack
            .dispatch(
                "brain.profile",
                json!({"id": "balanced-recall-v1"}),
                &registry,
                &rt.authorize(Namespace::local()),
            )
            .await
            .unwrap();
        assert_eq!(result["id"], json!("balanced-recall-v1"));
        assert_eq!(result["state_class"], json!("Bayesian"));
        assert_eq!(result["consumer_kind"], json!("recall"));
    }

    #[tokio::test]
    async fn dispatch_profile_not_found_returns_not_found() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let err = pack
            .dispatch(
                "brain.profile",
                json!({"id": "nonexistent"}),
                &registry,
                &rt.authorize(Namespace::local()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeError::NotFound(_)));
    }

    #[tokio::test]
    async fn dispatch_resolve_returns_default_profile_for_recall() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let result = pack
            .dispatch(
                "brain.resolve",
                json!({"consumer_kind": "recall"}),
                &registry,
                &rt.authorize(Namespace::local()),
            )
            .await
            .unwrap();
        assert_eq!(result["resolved_profile_id"], json!("balanced-recall-v1"));
    }

    #[tokio::test]
    async fn dispatch_activate_and_deactivate_profile() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Deactivate the default profile
        let result = pack
            .dispatch(
                "brain.deactivate",
                json!({"profile_id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(result["lifecycle"], json!("inactive"));

        // Verify via brain.profile
        let state = pack
            .dispatch(
                "brain.profile",
                json!({"id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(state["lifecycle"], json!("inactive"));

        // Reactivate
        let result = pack
            .dispatch(
                "brain.activate",
                json!({"profile_id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(result["lifecycle"], json!("active"));
    }

    #[tokio::test]
    async fn dispatch_archive_profile() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Lifecycle DAG requires active → inactive before archiving.
        pack.dispatch(
            "brain.deactivate",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        let result = pack
            .dispatch(
                "brain.archive",
                json!({"profile_id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(result["lifecycle"], json!("archived"));
    }

    #[tokio::test]
    async fn dispatch_activate_nonexistent_profile_returns_not_found() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let err = pack
            .dispatch(
                "brain.activate",
                json!({"profile_id": "ghost-profile"}),
                &registry,
                &rt.authorize(Namespace::local()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeError::NotFound(_)));
    }

    #[tokio::test]
    async fn dispatch_bind_and_resolve_explicit_binding() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Bind balanced-recall-v1 for actor "agent-x"
        let result = pack
            .dispatch(
                "brain.bind",
                json!({
                    "profile_id": "balanced-recall-v1",
                    "actor": "agent-x",
                    "consumer_kind": "recall"
                }),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(result["bound"], json!(true));
        assert_eq!(result["actor"], json!("agent-x"));

        // Resolve — should return the explicitly bound profile
        let resolved = pack
            .dispatch(
                "brain.resolve",
                json!({"actor": "agent-x", "consumer_kind": "recall"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(resolved["resolved_profile_id"], json!("balanced-recall-v1"));
    }

    #[tokio::test]
    async fn dispatch_bind_nonexistent_profile_returns_not_found() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let err = pack
            .dispatch(
                "brain.bind",
                json!({"profile_id": "ghost", "consumer_kind": "recall"}),
                &registry,
                &rt.authorize(Namespace::local()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeError::NotFound(_)));
    }

    #[tokio::test]
    async fn dispatch_unbind_removes_binding() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Add a binding
        pack.dispatch(
            "brain.bind",
            json!({"profile_id": "balanced-recall-v1", "actor": "agent-y", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        // Remove it
        let result = pack
            .dispatch(
                "brain.unbind",
                json!({"actor": "agent-y"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(result["unbound"], json!(1u64));
    }

    // ── UE5-H1: brain.profiles lifecycle filter public API ───────────────────
    //
    // The public lifecycle filter must only accept 'active', 'inactive', 'archived'.
    // Internal states 'defined' and 'registered' must not appear in the error message.

    #[tokio::test]
    async fn ue5_h1_invalid_lifecycle_error_lists_only_public_states() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        let err = pack
            .dispatch(
                "brain.profiles",
                json!({"lifecycle": "deleted"}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        if let RuntimeError::InvalidInput(msg) = &err {
            // Must mention valid states
            assert!(
                msg.contains("active"),
                "UE5-H1: error must list 'active'; got: {msg}"
            );
            assert!(
                msg.contains("inactive"),
                "UE5-H1: error must list 'inactive'; got: {msg}"
            );
            assert!(
                msg.contains("archived"),
                "UE5-H1: error must list 'archived'; got: {msg}"
            );
            // Must NOT leak internal states
            assert!(
                !msg.contains("defined"),
                "UE5-H1: error must NOT expose internal 'defined' state; got: {msg}"
            );
            assert!(
                !msg.contains("registered"),
                "UE5-H1: error must NOT expose internal 'registered' state; got: {msg}"
            );
        } else {
            panic!("UE5-H1: expected InvalidInput, got {err:?}");
        }
    }

    #[tokio::test]
    async fn ue5_h1_internal_lifecycle_values_rejected() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        for internal_state in ["defined", "registered"] {
            let err = pack
                .dispatch(
                    "brain.profiles",
                    json!({"lifecycle": internal_state}),
                    &registry,
                    &token,
                )
                .await
                .unwrap_err();
            assert!(
                matches!(err, RuntimeError::InvalidInput(_)),
                "UE5-H1: internal lifecycle '{internal_state}' must be rejected, got {err:?}"
            );
        }
    }

    // ── B-C1 regression: lifecycle terminal-state enforcement ─────────────────
    //
    // archived is terminal: no transition out of archived is permitted.
    // active → archived is illegal: must deactivate first.
    // active ⟷ inactive is the only reversible pair.

    #[tokio::test]
    async fn b_c1_archived_activate_is_rejected() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Deactivate first (active → inactive), then archive (inactive → archived)
        pack.dispatch(
            "brain.deactivate",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
        pack.dispatch(
            "brain.archive",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        // Attempt to activate an archived profile — must fail
        let err = pack
            .dispatch(
                "brain.activate",
                json!({"profile_id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        if let RuntimeError::InvalidInput(msg) = &err {
            assert!(
                msg.contains("terminal") || msg.contains("archived"),
                "B-C1: error must mention 'terminal' or 'archived'; got: {msg}"
            );
        } else {
            panic!("B-C1: expected InvalidInput, got {err:?}");
        }
    }

    #[tokio::test]
    async fn b_c1_archived_deactivate_is_rejected() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Get to archived via active → inactive → archived
        pack.dispatch(
            "brain.deactivate",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
        pack.dispatch(
            "brain.archive",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        // Attempt deactivate on archived profile — must fail
        let err = pack
            .dispatch(
                "brain.deactivate",
                json!({"profile_id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "B-C1: deactivate on archived must return InvalidInput, got {err:?}"
        );
    }

    #[tokio::test]
    async fn b_c1_active_to_archived_direct_is_rejected() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Profile starts active — direct archive must fail (must go through inactive)
        let err = pack
            .dispatch(
                "brain.archive",
                json!({"profile_id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        if let RuntimeError::InvalidInput(msg) = &err {
            assert!(
                msg.contains("deactivate") || msg.contains("inactive"),
                "B-C1: active→archived error must hint at deactivate; got: {msg}"
            );
        } else {
            panic!("B-C1: expected InvalidInput for active→archived, got {err:?}");
        }
    }

    #[tokio::test]
    async fn b_c1_inactive_to_archived_is_permitted() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // active → inactive → archived: legal path
        pack.dispatch(
            "brain.deactivate",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
        let result = pack
            .dispatch(
                "brain.archive",
                json!({"profile_id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(
            result["lifecycle"],
            json!("archived"),
            "B-C1: inactive→archived must succeed"
        );
    }

    // Regression test for MAJ-002: unbind with multiple filters must use AND semantics,
    // removing only the binding that satisfies ALL supplied criteria.
    #[tokio::test]
    async fn dispatch_unbind_uses_and_not_or() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // binding 1: ns=A, profile=P1 (the one we want to remove)
        pack.dispatch(
            "brain.bind",
            json!({"profile_id": "balanced-recall-v1", "namespace": "ns-a", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        // binding 2: ns=B, profile=P1 (must survive)
        pack.dispatch(
            "brain.bind",
            json!({"profile_id": "balanced-recall-v1", "namespace": "ns-b", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        // Unbind using both filters: only binding-1 should be removed
        let result = pack
            .dispatch(
                "brain.unbind",
                json!({"namespace": "ns-a", "profile_id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(
            result["unbound"],
            json!(1u64),
            "should remove exactly one binding"
        );

        // binding-2 (ns-b) must still exist
        let state = pack.state.lock().unwrap();
        let remaining: Vec<_> = state
            .bindings
            .iter()
            .filter(|b| b.namespace == "ns-b")
            .collect();
        assert_eq!(remaining.len(), 1, "ns-b binding must survive the unbind");
    }

    #[tokio::test]
    async fn dispatch_config_all_parameters() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let result = pack
            .dispatch(
                "brain.config",
                json!({}),
                &registry,
                &rt.authorize(Namespace::local()),
            )
            .await
            .unwrap();
        let obj = result.as_object().unwrap();
        assert!(obj.contains_key("recall::relevance_weight"));
        assert!(obj.contains_key("recall::salience_weight"));
        assert!(obj.contains_key("recall::temporal_weight"));
    }

    #[tokio::test]
    async fn dispatch_config_single_parameter() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let result = pack
            .dispatch(
                "brain.config",
                json!({"parameter": "recall::relevance_weight"}),
                &registry,
                &rt.authorize(Namespace::local()),
            )
            .await
            .unwrap();
        assert_eq!(result["parameter"], json!("recall::relevance_weight"));
        // Prior is Beta(7,3): mean = 0.7
        let mean = result["mean"].as_f64().unwrap();
        assert!((mean - 0.7).abs() < 1e-6);
    }

    // ── Regression tests (issues #355, #356, #357, #295) ──────────────────────

    // #356 (MAJ-003): profile_record.total_events must stay in sync with
    // balanced_recall.total_events via BOTH the handle_feedback path AND the
    // on_dispatch hook path.  The previous fix only wired the sync helper; this
    // test pins that removing EITHER call would be caught.
    //
    // Part A — handle_feedback path (unchanged from before).
    // Part B — on_dispatch path: introduce a deliberate desync by reaching into
    //   the live state directly, then fire on_dispatch to verify the sync
    //   corrects it.  This would fail if sync_balanced_recall_record is removed
    //   from on_dispatch.
    #[tokio::test]
    async fn test_356_profile_record_total_events_synced_after_feedback() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());
        let target = create_test_entity(&rt, &token).await;

        // Part A: handle_feedback path.
        for _ in 0..3 {
            pack.dispatch(
                "brain.feedback",
                json!({"target_id": target, "signal": "useful"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        }

        let snap = pack.snapshot();
        let live_total = snap.balanced_recall.total_events;

        let record_result = pack
            .dispatch(
                "brain.profile",
                json!({"id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        let record_total = record_result["total_events"].as_u64().unwrap();

        assert_eq!(
            live_total, record_total,
            "#356 part-A: profile_record.total_events ({record_total}) must equal \
             balanced_recall.total_events ({live_total}) after feedback calls"
        );
        assert_eq!(live_total, 3, "expected exactly 3 events from part A");

        // Part B: on_dispatch path.
        // Deliberately desync the record by bumping balanced_recall.total_events
        // directly (simulating what would happen if only on_dispatch updated the
        // live state but the sync call were missing).
        {
            let mut state = pack.state.lock().unwrap();
            state.balanced_recall.total_events += 7; // introduce desync
                                                     // Profile record still says `live_total` at this point.
        }

        // Fire on_dispatch with an irrelevant (non-brain) verb event — this is
        // exactly what the runtime hook does for every non-brain verb dispatch.
        // The sync helper inside on_dispatch must correct the desync.
        let hook_event = {
            use khive_types::{EventKind, SubstrateKind};
            let mut e = khive_storage::event::Event::new(
                "local",
                "search",
                EventKind::Audit,
                SubstrateKind::Event,
                "kg",
            );
            e.outcome = khive_types::EventOutcome::Success;
            e
        };
        let hook_view = khive_runtime::EventView {
            event: hook_event,
            observations: Vec::new(),
        };
        pack.on_dispatch(&hook_view).await;

        // After on_dispatch, the record must reflect the new (desynced) live total.
        let after_hook = pack
            .dispatch(
                "brain.profile",
                json!({"id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        let after_total = after_hook["total_events"].as_u64().unwrap();
        let live_after = pack.snapshot().balanced_recall.total_events;
        assert_eq!(
            after_total, live_after,
            "#356 part-B: on_dispatch sync must correct desync; \
             record shows {after_total}, live state shows {live_after}"
        );
    }

    // #357 (MAJ-004): brain.feedback must NOT double-count total_events.
    // The dispatch hook fires for brain.feedback — it must be skipped so the
    // fold.reduce in handle_feedback is the single source of truth.
    #[tokio::test]
    async fn test_357_feedback_no_double_count() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let brain = std::sync::Arc::new(BrainPack::new(rt.clone()));
        let token = rt.authorize(Namespace::local());

        // Create a real entity so C4 target_id validation passes.
        let target = create_test_entity(&rt, &token).await;

        // Build a registry WITH the hook so we can trigger the double-count path.
        let mut builder = VerbRegistryBuilder::new();
        let hook: std::sync::Arc<dyn DispatchHook> = brain.clone();
        builder.with_dispatch_hook(hook);
        let registry = builder.build().expect("registry builds");

        // Dispatch brain.feedback through the registry (hook is registered).
        brain
            .dispatch(
                "brain.feedback",
                json!({"target_id": target, "signal": "useful"}),
                &registry,
                &token,
            )
            .await
            .unwrap();

        let snap = brain.snapshot();
        assert_eq!(
            snap.balanced_recall.total_events, 1,
            "#357: total_events must be 1 after one brain.feedback call, got {} \
             (double-count if 2)",
            snap.balanced_recall.total_events
        );
    }

    // #295: brain.reset must restore domain-informed priors, not Beta(1,1).
    //
    // Strengthened per codex P12 Medium: this test now exercises the full
    // production path — handle_reset → reset_posteriors → sync helper — and
    // verifies that ALL three profile record fields (total_events,
    // exploration_epoch, state_snapshot) reflect the restored priors.
    //
    // It also creates a stale record via hook-only updates (bypassing
    // handle_feedback) before the reset, so the desync is real and not
    // incidentally corrected by the feedback path.
    #[tokio::test]
    async fn test_295_reset_restores_domain_priors_not_uniform() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Step 1: accumulate state via hook-only updates (no handle_feedback).
        // This simulates the common case where brain observes external pack
        // events rather than explicit feedback calls.
        let hook_event = |verb: &str| {
            use khive_types::{EventKind, SubstrateKind};
            let mut e = khive_storage::event::Event::new(
                "local",
                verb,
                EventKind::Audit,
                SubstrateKind::Event,
                "kg",
            );
            e.outcome = khive_types::EventOutcome::Success;
            e
        };

        // Fire 4 hook events for a non-brain verb (simulates external recall/search).
        for _ in 0..4 {
            let view = khive_runtime::EventView {
                event: hook_event("search"),
                observations: Vec::new(),
            };
            pack.on_dispatch(&view).await;
        }

        // Step 2: also call handle_feedback directly to move salience away from prior.
        // C4: create a real entity so target_id validation passes.
        let target = create_test_entity(&rt, &token).await;
        for _ in 0..5 {
            pack.dispatch(
                "brain.feedback",
                json!({"target_id": target, "signal": "useful"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        }

        // Verify state before reset: posteriors have moved, total_events > 0.
        let before = pack.snapshot();
        assert!(
            before.balanced_recall.salience.alpha > 2.0,
            "salience.alpha must have grown past prior after useful feedback"
        );
        assert!(
            before.balanced_recall.total_events >= 9,
            "expected at least 9 total events (4 hook + 5 feedback), got {}",
            before.balanced_recall.total_events
        );
        // Verify record was kept in sync before reset (both paths called sync helper).
        let pre_reset_record = pack
            .dispatch(
                "brain.profile",
                json!({"id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(
            pre_reset_record["total_events"].as_u64().unwrap(),
            before.balanced_recall.total_events,
            "#295 pre-reset: profile record total_events out of sync before reset"
        );

        // Step 3: call handle_reset via the production path (dispatch → handle_reset).
        let reset_result = pack
            .dispatch(
                "brain.reset",
                json!({"profile_id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(reset_result["reset"], json!(true));

        // Verify exploration_epoch incremented (reset_posteriors contract).
        let epoch_after = reset_result["exploration_epoch"].as_u64().unwrap();
        assert!(
            epoch_after > 0,
            "#295: exploration_epoch must increment after reset"
        );

        // Step 4: after reset, posteriors must be domain-informed priors — NOT Beta(1,1).
        let after = pack.snapshot();

        // salience prior = Beta(2,8)
        assert!(
            (after.balanced_recall.salience.alpha - 2.0).abs() < 1e-12,
            "#295: salience.alpha must be 2.0 after reset, got {}",
            after.balanced_recall.salience.alpha
        );
        assert!(
            (after.balanced_recall.salience.beta - 8.0).abs() < 1e-12,
            "#295: salience.beta must be 8.0 after reset, got {}",
            after.balanced_recall.salience.beta
        );

        // temporal prior = Beta(1,9)
        assert!(
            (after.balanced_recall.temporal.alpha - 1.0).abs() < 1e-12,
            "#295: temporal.alpha must be 1.0 after reset, got {}",
            after.balanced_recall.temporal.alpha
        );
        assert!(
            (after.balanced_recall.temporal.beta - 9.0).abs() < 1e-12,
            "#295: temporal.beta must be 9.0 after reset, got {}",
            after.balanced_recall.temporal.beta
        );

        // relevance prior = Beta(7,3)
        assert!(
            (after.balanced_recall.relevance.alpha - 7.0).abs() < 1e-12,
            "#295: relevance.alpha must be 7.0 after reset"
        );

        // Step 5: brain.profile must reflect the reset state — ALL three fields.
        // This pins the sync_balanced_recall_record call inside handle_reset.
        // Removing that call would cause this assertion to fail.
        let record = pack
            .dispatch(
                "brain.profile",
                json!({"id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();

        // total_events: after reset the state is a fresh BalancedRecallState
        // (total_events = 0), so the record must reflect that.
        let record_total = record["total_events"].as_u64().unwrap();
        assert_eq!(
            record_total, after.balanced_recall.total_events,
            "#295: profile record total_events ({record_total}) must match \
             live state ({}) after reset",
            after.balanced_recall.total_events
        );

        // exploration_epoch: record must match the live state.
        let record_epoch = record["exploration_epoch"].as_u64().unwrap();
        assert_eq!(
            record_epoch, epoch_after,
            "#295: profile record exploration_epoch ({record_epoch}) must match \
             reset result ({epoch_after})"
        );

        // state_snapshot: salience.alpha must be the prior value.
        let snap = &record["state_snapshot"];
        let sal_alpha = snap["salience"]["alpha"].as_f64().unwrap();
        assert!(
            (sal_alpha - 2.0).abs() < 1e-12,
            "#295: brain.profile state_snapshot salience.alpha must be 2.0 after reset, \
             got {sal_alpha}"
        );
    }

    // #355 (regression — real dispatch path): temporal posterior must update
    // when a recall hits via the on_dispatch hook carrying real hit/latency.
    //
    // This test exercises the production wiring added in the P12 codex fix:
    // the runtime now embeds duration_us + target_id in the hook event for
    // "recall" verbs.  Simulates that by constructing the hook event the way
    // the runtime now would, then verifies temporal.alpha increments.
    #[tokio::test]
    async fn test_355_posteriors_update_after_dispatch_via_hook() {
        let (pack, _rt) = make_pack();
        let before = pack.snapshot();
        let tmp_alpha_before = before.balanced_recall.temporal.alpha;
        let tmp_beta_before = before.balanced_recall.temporal.beta;

        // Simulate the runtime hook event for a fast recall hit:
        // duration_us ≤ 50_000 (fast) and target_id is present (hit).
        let target_id = uuid::Uuid::new_v4();
        let fast_hit_event = {
            use khive_types::{EventKind, SubstrateKind};
            let mut e = khive_storage::event::Event::new(
                "local",
                "recall",
                EventKind::Audit,
                SubstrateKind::Event,
                "memory",
            );
            e.outcome = khive_types::EventOutcome::Success;
            e.target_id = Some(target_id);
            e.duration_us = 10_000; // 10 ms — fast hit
            e
        };
        let view = khive_runtime::EventView {
            event: fast_hit_event,
            observations: Vec::new(),
        };
        pack.on_dispatch(&view).await;

        let after_fast = pack.snapshot();
        assert!(
            (after_fast.balanced_recall.temporal.alpha - (tmp_alpha_before + 1.0)).abs() < 1e-12,
            "#355: fast recall hit must increment temporal.alpha via hook: expected {}, got {}",
            tmp_alpha_before + 1.0,
            after_fast.balanced_recall.temporal.alpha
        );
        assert!(
            (after_fast.balanced_recall.temporal.beta - tmp_beta_before).abs() < 1e-12,
            "#355: fast hit must NOT increment temporal.beta"
        );

        // Simulate a slow recall hit (duration_us > 50_000) → temporal failure.
        let slow_hit_event = {
            use khive_types::{EventKind, SubstrateKind};
            let mut e = khive_storage::event::Event::new(
                "local",
                "recall",
                EventKind::Audit,
                SubstrateKind::Event,
                "memory",
            );
            e.outcome = khive_types::EventOutcome::Success;
            e.target_id = Some(target_id);
            e.duration_us = 100_000; // 100 ms — slow
            e
        };
        let view2 = khive_runtime::EventView {
            event: slow_hit_event,
            observations: Vec::new(),
        };
        pack.on_dispatch(&view2).await;

        let after_slow = pack.snapshot();
        assert!(
            (after_slow.balanced_recall.temporal.beta - (tmp_beta_before + 1.0)).abs() < 1e-12,
            "#355: slow recall hit must increment temporal.beta via hook: expected {}, got {}",
            tmp_beta_before + 1.0,
            after_slow.balanced_recall.temporal.beta
        );

        // Simulate a recall miss (no target_id) → temporal failure.
        let miss_event = {
            use khive_types::{EventKind, SubstrateKind};
            let mut e = khive_storage::event::Event::new(
                "local",
                "recall",
                EventKind::Audit,
                SubstrateKind::Event,
                "memory",
            );
            e.outcome = khive_types::EventOutcome::Success;
            // target_id = None → RecallMiss
            e
        };
        let view3 = khive_runtime::EventView {
            event: miss_event,
            observations: Vec::new(),
        };
        pack.on_dispatch(&view3).await;

        let after_miss = pack.snapshot();
        assert!(
            (after_miss.balanced_recall.temporal.beta - (tmp_beta_before + 2.0)).abs() < 1e-12,
            "#355: recall miss must further increment temporal.beta: expected {}, got {}",
            tmp_beta_before + 2.0,
            after_miss.balanced_recall.temporal.beta
        );
    }

    // ── Wave-4 Critical regressions (C1-C4) ──────────────────────────────────

    // C2: brain.unbind with zero filters must be rejected.
    #[tokio::test]
    async fn w4_c2_unbind_no_filter_is_rejected() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Add a binding so there is something to accidentally wipe.
        pack.dispatch(
            "brain.bind",
            json!({"profile_id": "balanced-recall-v1", "actor": "agent-z", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        let err = pack
            .dispatch("brain.unbind", json!({}), &registry, &token)
            .await
            .unwrap_err();
        if let RuntimeError::InvalidInput(msg) = &err {
            assert!(
                msg.contains("filter") || msg.contains("profile_id") || msg.contains("actor"),
                "C2: zero-filter unbind must mention required filter; got: {msg}"
            );
        } else {
            panic!("C2: zero-filter unbind must return InvalidInput, got {err:?}");
        }

        // Binding must still be intact.
        let state = pack.state.lock().unwrap();
        assert!(
            !state.bindings.is_empty(),
            "C2: binding must survive the rejected unbind"
        );
    }

    // C3: brain.bind must reject archived profiles.
    #[tokio::test]
    async fn w4_c3_bind_archived_profile_is_rejected() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Archive the only profile.
        pack.dispatch(
            "brain.deactivate",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
        pack.dispatch(
            "brain.archive",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        let err = pack
            .dispatch(
                "brain.bind",
                json!({"profile_id": "balanced-recall-v1", "consumer_kind": "recall"}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        if let RuntimeError::InvalidInput(msg) = &err {
            assert!(
                msg.contains("archived"),
                "C3: bind to archived profile must mention 'archived'; got: {msg}"
            );
        } else {
            panic!("C3: bind to archived profile must return InvalidInput, got {err:?}");
        }
    }

    // C3: brain.resolve must skip bindings pointing at archived profiles.
    #[tokio::test]
    async fn w4_c3_resolve_skips_archived_binding() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Force a binding directly into state (bypassing handle_bind guard) to simulate
        // a pre-existing binding that was created before the profile was archived.
        {
            let mut state = pack.state.lock().unwrap();
            state.bindings.push(crate::state::ProfileBinding {
                actor: "*".into(),
                namespace: "*".into(),
                consumer_kind: "recall".into(),
                profile_id: "balanced-recall-v1".into(),
                priority: 100,
                created_at: chrono::Utc::now(),
            });
            // Archive the profile in-state.
            state
                .profiles
                .get_mut("balanced-recall-v1")
                .unwrap()
                .lifecycle = ProfileLifecycle::Archived;
        }

        // Resolve must NOT return the archived profile.
        let err = pack
            .dispatch(
                "brain.resolve",
                json!({"consumer_kind": "recall"}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::NotFound(_)),
            "C3: resolve with only archived binding must return NotFound, got {err:?}"
        );
    }

    // C4: brain.feedback must reject nonexistent target_id.
    #[tokio::test]
    async fn w4_c4_feedback_rejects_nonexistent_target() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        let err = pack
            .dispatch(
                "brain.feedback",
                json!({"target_id": "00000000-0000-0000-0000-000000000000", "signal": "useful"}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::NotFound(_)),
            "C4: feedback with nonexistent target_id must return NotFound, got {err:?}"
        );
    }

    // C4: brain.feedback must reject nonexistent served_by_profile_id.
    #[tokio::test]
    async fn w4_c4_feedback_rejects_nonexistent_profile() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Create a real entity for the valid target_id.
        let target = create_test_entity(&rt, &token).await;

        let err = pack
            .dispatch(
                "brain.feedback",
                json!({"target_id": target, "signal": "useful", "served_by_profile_id": "fake-profile-xyz"}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::NotFound(_)),
            "C4: feedback with nonexistent served_by_profile_id must return NotFound, got {err:?}"
        );
    }

    // C4: brain.feedback with valid target and known profile must succeed.
    #[tokio::test]
    async fn w4_c4_feedback_accepts_valid_target_and_profile() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        let target = create_test_entity(&rt, &token).await;

        let result = pack
            .dispatch(
                "brain.feedback",
                json!({"target_id": target, "signal": "useful", "served_by_profile_id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(result["emitted"], json!(true));
        assert_eq!(result["signal"], json!("useful"));
    }

    // H1: brain.create_profile creates a new inactive profile.
    #[tokio::test]
    async fn w4_h1_create_profile_creates_new_profile() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        let result = pack
            .dispatch(
                "brain.create_profile",
                json!({"name": "my-profile-v1", "consumer_kind": "search", "description": "Custom search profile"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(result["created"], json!(true));
        assert_eq!(result["profile_id"], json!("my-profile-v1"));
        assert_eq!(result["lifecycle"], json!("inactive"));
        assert_eq!(result["consumer_kind"], json!("search"));

        // Verify it appears in brain.profiles.
        let profiles = pack
            .dispatch("brain.profiles", json!({}), &registry, &token)
            .await
            .unwrap();
        let ids: Vec<&str> = profiles["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p["id"].as_str())
            .collect();
        assert!(
            ids.contains(&"my-profile-v1"),
            "new profile must appear in brain.profiles"
        );
    }

    // H1: duplicate name is rejected.
    #[tokio::test]
    async fn w4_h1_create_profile_duplicate_rejected() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        let err = pack
            .dispatch(
                "brain.create_profile",
                json!({"name": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "H1: duplicate profile name must return InvalidInput, got {err:?}"
        );
    }

    // H2: brain.bindings lists binding rows.
    #[tokio::test]
    async fn w4_h2_bindings_lists_rows() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Initially empty.
        let result = pack
            .dispatch("brain.bindings", json!({}), &registry, &token)
            .await
            .unwrap();
        assert_eq!(result["count"], json!(0u64));
        assert_eq!(result["bindings"], json!([]));

        // Add a binding.
        pack.dispatch(
            "brain.bind",
            json!({"profile_id": "balanced-recall-v1", "actor": "agent-a", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        let result2 = pack
            .dispatch("brain.bindings", json!({}), &registry, &token)
            .await
            .unwrap();
        assert_eq!(result2["count"], json!(1u64));
        let rows = result2["bindings"].as_array().unwrap();
        assert_eq!(rows[0]["actor"], json!("agent-a"));
        assert_eq!(rows[0]["profile_id"], json!("balanced-recall-v1"));
    }

    // H2: brain.bindings supports filtering.
    #[tokio::test]
    async fn w4_h2_bindings_filtered() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        pack.dispatch("brain.bind", json!({"profile_id": "balanced-recall-v1", "actor": "agent-1", "consumer_kind": "recall"}), &registry, &token).await.unwrap();
        pack.dispatch("brain.bind", json!({"profile_id": "balanced-recall-v1", "actor": "agent-2", "consumer_kind": "search"}), &registry, &token).await.unwrap();

        let result = pack
            .dispatch(
                "brain.bindings",
                json!({"actor": "agent-1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(result["count"], json!(1u64));
        assert_eq!(result["bindings"][0]["actor"], json!("agent-1"));
    }

    // H3: brain.resolve response includes both requested and matched consumer_kind.
    #[tokio::test]
    async fn w4_h3_resolve_returns_both_requested_and_matched_kind() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Install a wildcard binding (consumer_kind = "*").
        pack.dispatch(
            "brain.bind",
            json!({"profile_id": "balanced-recall-v1", "consumer_kind": "*", "priority": 1}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        // Query for a different consumer_kind — wildcard binding matches.
        let result = pack
            .dispatch(
                "brain.resolve",
                json!({"consumer_kind": "search"}),
                &registry,
                &token,
            )
            .await
            .unwrap();

        assert_eq!(
            result["requested_consumer_kind"],
            json!("search"),
            "H3: requested_consumer_kind must equal the query"
        );
        assert_eq!(
            result["matched_consumer_kind"],
            json!("*"),
            "H3: matched_consumer_kind must show the wildcard binding"
        );
        assert_eq!(result["resolved_profile_id"], json!("balanced-recall-v1"));
    }

    // H3: exact match returns matching kind in matched_consumer_kind.
    #[tokio::test]
    async fn w4_h3_resolve_exact_match_returns_exact_kind() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Default fallback (no binding) uses profile's consumer_kind.
        let result = pack
            .dispatch(
                "brain.resolve",
                json!({"consumer_kind": "recall"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(result["requested_consumer_kind"], json!("recall"));
        assert_eq!(result["matched_consumer_kind"], json!("recall"));
    }

    // Round-2 fix 3: archived high-priority binding + live lower-priority wildcard → live wins.
    #[tokio::test]
    async fn r2_archived_exact_binding_defers_to_live_wildcard() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Create a second live profile.
        pack.dispatch(
            "brain.create_profile",
            json!({"name": "search-v1", "consumer_kind": "search"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
        pack.dispatch(
            "brain.activate",
            json!({"profile_id": "search-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        // Insert a high-priority exact binding pointing at the default profile.
        {
            let mut state = pack.state.lock().unwrap();
            state.bindings.push(crate::state::ProfileBinding {
                actor: "*".into(),
                namespace: "*".into(),
                consumer_kind: "search".into(),
                profile_id: "balanced-recall-v1".into(),
                priority: 100,
                created_at: chrono::Utc::now(),
            });
            // Archive balanced-recall-v1 in-state to simulate it being retired.
            state
                .profiles
                .get_mut("balanced-recall-v1")
                .unwrap()
                .lifecycle = crate::state::ProfileLifecycle::Archived;
        }

        // Add a lower-priority wildcard binding pointing at the live profile.
        pack.dispatch(
            "brain.bind",
            json!({"profile_id": "search-v1", "consumer_kind": "*", "priority": 1}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        // Resolve must return the live lower-priority profile, NOT fall to default.
        let result = pack
            .dispatch(
                "brain.resolve",
                json!({"consumer_kind": "search"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(
            result["resolved_profile_id"],
            json!("search-v1"),
            "r2 fix 3: archived high-priority binding must not suppress the live wildcard binding"
        );
    }

    // Round-2 fix 4: brain.feedback rejects archived served_by_profile_id.
    #[tokio::test]
    async fn r2_feedback_rejects_archived_served_by_profile() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        let target = create_test_entity(&rt, &token).await;

        // Archive the only profile.
        pack.dispatch(
            "brain.deactivate",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
        pack.dispatch(
            "brain.archive",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        let err = pack
            .dispatch(
                "brain.feedback",
                json!({
                    "target_id": target,
                    "signal": "useful",
                    "served_by_profile_id": "balanced-recall-v1"
                }),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        if let RuntimeError::InvalidInput(msg) = &err {
            assert!(
                msg.contains("archived"),
                "r2 fix 4: feedback to archived profile must mention 'archived'; got: {msg}"
            );
        } else {
            panic!("r2 fix 4: feedback to archived served_by_profile_id must return InvalidInput, got {err:?}");
        }
    }

    // Round-2 fix 5: brain.create_profile rejects empty and wildcard consumer_kind.
    #[tokio::test]
    async fn r2_create_profile_rejects_empty_consumer_kind() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        let err = pack
            .dispatch(
                "brain.create_profile",
                json!({"name": "bad-profile", "consumer_kind": ""}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "r2 fix 5: empty consumer_kind must return InvalidInput, got {err:?}"
        );
    }

    #[tokio::test]
    async fn r2_create_profile_rejects_wildcard_consumer_kind() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        let err = pack
            .dispatch(
                "brain.create_profile",
                json!({"name": "wildcard-profile", "consumer_kind": "*"}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        if let RuntimeError::InvalidInput(msg) = &err {
            assert!(
                msg.contains("wildcard") || msg.contains("sentinel") || msg.contains("*"),
                "r2 fix 5: wildcard consumer_kind rejection must explain the issue; got: {msg}"
            );
        } else {
            panic!("r2 fix 5: wildcard consumer_kind must return InvalidInput, got {err:?}");
        }
    }

    #[tokio::test]
    async fn r2_create_profile_rejects_whitespace_consumer_kind() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        let err = pack
            .dispatch(
                "brain.create_profile",
                json!({"name": "ws-profile", "consumer_kind": "   "}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "r2 fix 5: whitespace consumer_kind must return InvalidInput, got {err:?}"
        );
    }

    // Round-2 fix 6: brain.bindings AND-semantics pinned with ≥3 bindings and combined filters.
    #[tokio::test]
    async fn r2_bindings_and_semantics_multi_filter() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Create two extra profiles for variety.
        for name in ["alpha-v1", "beta-v1"] {
            pack.dispatch(
                "brain.create_profile",
                json!({"name": name, "consumer_kind": "recall"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
            pack.dispatch(
                "brain.activate",
                json!({"profile_id": name}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        }

        // Three bindings with distinct (actor, namespace, consumer_kind) combos.
        pack.dispatch(
            "brain.bind",
            json!({"profile_id": "balanced-recall-v1", "actor": "agent-A", "namespace": "ns-1", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
        pack.dispatch(
            "brain.bind",
            json!({"profile_id": "alpha-v1", "actor": "agent-A", "namespace": "ns-2", "consumer_kind": "search"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
        pack.dispatch(
            "brain.bind",
            json!({"profile_id": "beta-v1", "actor": "agent-B", "namespace": "ns-1", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        // Filter by profile_id + namespace: should return exactly 1 row.
        let r1 = pack
            .dispatch(
                "brain.bindings",
                json!({"profile_id": "balanced-recall-v1", "namespace": "ns-1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(
            r1["count"],
            json!(1u64),
            "AND filter profile_id+namespace must return 1 row"
        );
        assert_eq!(r1["bindings"][0]["profile_id"], json!("balanced-recall-v1"));
        assert_eq!(r1["bindings"][0]["namespace"], json!("ns-1"));

        // Filter by actor + consumer_kind: agent-A+search → only alpha-v1 row.
        let r2 = pack
            .dispatch(
                "brain.bindings",
                json!({"actor": "agent-A", "consumer_kind": "search"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(
            r2["count"],
            json!(1u64),
            "AND filter actor+consumer_kind must return 1 row"
        );
        assert_eq!(r2["bindings"][0]["profile_id"], json!("alpha-v1"));

        // Zero-row combination: agent-B + consumer_kind=search (no such binding).
        let r3 = pack
            .dispatch(
                "brain.bindings",
                json!({"actor": "agent-B", "consumer_kind": "search"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(
            r3["count"],
            json!(0u64),
            "AND filter with no matches must return count=0"
        );
        assert_eq!(r3["bindings"], json!([]));
    }

    // Round-2 fix 2: user-created profile has real posterior state that reset mutates.
    // Round-3 strengthening: emit feedback first to move posteriors, then assert all three
    // posterior alpha/beta values return to their ADR-032 priors after reset.
    #[tokio::test]
    async fn r2_user_profile_reset_mutates_posteriors() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        let target = create_test_entity(&rt, &token).await;

        pack.dispatch(
            "brain.create_profile",
            json!({"name": "custom-v1", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
        pack.dispatch(
            "brain.activate",
            json!({"profile_id": "custom-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        // Emit feedback to custom-v1 so its salience posterior diverges from prior.
        // brain.feedback with signal="useful" → salience.update_success() (fold.rs:54).
        pack.dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "useful", "served_by_profile_id": "custom-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        // Confirm salience.alpha increased above the prior (2.0).
        let mutated = pack
            .dispatch(
                "brain.profile",
                json!({"profile_id": "custom-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        let salience_alpha_before = mutated["state_snapshot"]["salience"]["alpha"]
            .as_f64()
            .expect("state_snapshot.salience.alpha must be a number");
        assert!(
            salience_alpha_before > 2.0,
            "r3 fix 2: feedback must have moved salience alpha above prior 2.0; got {salience_alpha_before}"
        );
        let epoch_before = mutated["exploration_epoch"].as_u64().unwrap();

        // Reset custom profile.
        let reset_result = pack
            .dispatch(
                "brain.reset",
                json!({"profile_id": "custom-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(reset_result["reset"], json!(true));
        assert_eq!(reset_result["profile_id"], json!("custom-v1"));

        // Epoch must increment.
        let after = pack
            .dispatch(
                "brain.profile",
                json!({"profile_id": "custom-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        let epoch_after = after["exploration_epoch"].as_u64().unwrap();
        assert!(
            epoch_after > epoch_before,
            "r3 fix 2: reset must increment exploration_epoch on user-created profile; before={epoch_before} after={epoch_after}"
        );

        // All three posteriors must return exactly to ADR-032 priors:
        //   relevance  = Beta(7, 3)
        //   salience   = Beta(2, 8)
        //   temporal   = Beta(1, 9)
        let snap = &after["state_snapshot"];
        assert!(
            !snap.is_null(),
            "r3 fix 2: state_snapshot must be non-null after reset"
        );

        let rel_alpha = snap["relevance"]["alpha"]
            .as_f64()
            .expect("relevance.alpha");
        let rel_beta = snap["relevance"]["beta"].as_f64().expect("relevance.beta");
        assert!(
            (rel_alpha - 7.0).abs() < 1e-9 && (rel_beta - 3.0).abs() < 1e-9,
            "r3 fix 2: relevance must be Beta(7,3) after reset; got ({rel_alpha},{rel_beta})"
        );

        let sal_alpha = snap["salience"]["alpha"].as_f64().expect("salience.alpha");
        let sal_beta = snap["salience"]["beta"].as_f64().expect("salience.beta");
        assert!(
            (sal_alpha - 2.0).abs() < 1e-9 && (sal_beta - 8.0).abs() < 1e-9,
            "r3 fix 2: salience must be Beta(2,8) after reset; got ({sal_alpha},{sal_beta})"
        );

        let tmp_alpha = snap["temporal"]["alpha"].as_f64().expect("temporal.alpha");
        let tmp_beta = snap["temporal"]["beta"].as_f64().expect("temporal.beta");
        assert!(
            (tmp_alpha - 1.0).abs() < 1e-9 && (tmp_beta - 9.0).abs() < 1e-9,
            "r3 fix 2: temporal must be Beta(1,9) after reset; got ({tmp_alpha},{tmp_beta})"
        );
    }

    // Round-2 fix 2: feedback routes to the user-created profile's own state.
    #[tokio::test]
    async fn r2_user_profile_feedback_routes_to_profile_state() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        let target = create_test_entity(&rt, &token).await;

        pack.dispatch(
            "brain.create_profile",
            json!({"name": "custom-v1", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
        pack.dispatch(
            "brain.activate",
            json!({"profile_id": "custom-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        let before = pack
            .dispatch(
                "brain.profile",
                json!({"profile_id": "custom-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        let events_before = before["total_events"].as_u64().unwrap();

        pack.dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "useful", "served_by_profile_id": "custom-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        let after = pack
            .dispatch(
                "brain.profile",
                json!({"profile_id": "custom-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        let events_after = after["total_events"].as_u64().unwrap();
        assert!(
            events_after > events_before,
            "r2 fix 2: feedback routed to custom profile must increment its total_events; before={events_before} after={events_after}"
        );
    }

    // H4: brain.profile accepts profile_id (canonical) and id (alias).
    #[tokio::test]
    async fn w4_h4_profile_accepts_profile_id_and_id_alias() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Canonical arg.
        let r1 = pack
            .dispatch(
                "brain.profile",
                json!({"profile_id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(r1["id"], json!("balanced-recall-v1"));

        // Legacy alias.
        let r2 = pack
            .dispatch(
                "brain.profile",
                json!({"id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(r2["id"], json!("balanced-recall-v1"));
    }

    // ── Round-3 regression tests ──────────────────────────────────────────────

    // R3-1: archiving balanced-recall-v1 then calling brain.feedback without
    // served_by_profile_id must return InvalidInput and must NOT append an event.
    #[tokio::test]
    async fn r3_feedback_default_profile_archived_rejected() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        let target = create_test_entity(&rt, &token).await;

        // balanced-recall-v1 starts Active; must deactivate before archiving (lifecycle rule).
        pack.dispatch(
            "brain.deactivate",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        // Archive the default profile.
        pack.dispatch(
            "brain.archive",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

        // Baseline: total_events on default profile before the attempted feedback.
        // Also capture brain.events count so we catch any FeedbackExplicit row that
        // sneaks past the lifecycle check via a reordered append → fold sequence.
        let snap_before = pack
            .dispatch(
                "brain.profile",
                json!({"profile_id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        let events_before = snap_before["total_events"].as_u64().unwrap_or(0);

        let log_before = pack
            .dispatch("brain.events", json!({"limit": 1000}), &registry, &token)
            .await
            .unwrap();
        let log_count_before = log_before["events"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);

        // feedback without served_by_profile_id must be rejected.
        let err = pack
            .dispatch(
                "brain.feedback",
                json!({"target_id": target, "signal": "useful"}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        match &err {
            RuntimeError::InvalidInput(msg) => {
                assert!(
                    msg.contains("archived"),
                    "r3-1: error must mention 'archived'; got: {msg}"
                );
            }
            other => panic!("r3-1: expected InvalidInput(archived), got {other:?}"),
        }

        // No state mutation: total_events must be unchanged.
        let snap_after = pack
            .dispatch(
                "brain.profile",
                json!({"profile_id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        let events_after = snap_after["total_events"].as_u64().unwrap_or(0);
        assert_eq!(
            events_after, events_before,
            "r3-1: archived default profile must not have events appended; before={events_before} after={events_after}"
        );

        // Defense-in-depth: also verify nothing landed in the event log itself.
        // A future reorder that appends FeedbackExplicit before the lifecycle check
        // but skips the fold would leave total_events unchanged but still write to
        // the log. This assertion catches that class.
        let log_after = pack
            .dispatch("brain.events", json!({"limit": 1000}), &registry, &token)
            .await
            .unwrap();
        let log_count_after = log_after["events"].as_array().map(|a| a.len()).unwrap_or(0);
        assert_eq!(
            log_count_after, log_count_before,
            "r3-1: rejected feedback must not append a FeedbackExplicit event; before={log_count_before} after={log_count_after}"
        );
    }

    // R3-3: brain.create_profile profile-id grammar enforcement.
    #[tokio::test]
    async fn r3_create_profile_id_grammar_enforced() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local());

        // Whitespace-only name must be rejected.
        let err = pack
            .dispatch(
                "brain.create_profile",
                json!({"name": "   ", "consumer_kind": "recall"}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "r3-3: whitespace-only name must return InvalidInput; got {err:?}"
        );

        // Leading/trailing space: trimmed name "my-profile" must be accepted.
        pack.dispatch(
            "brain.create_profile",
            json!({"name": "  my-profile  ", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .expect("r3-3: name with leading/trailing spaces should be accepted after trim");

        // Dot in name must be rejected.
        let err = pack
            .dispatch(
                "brain.create_profile",
                json!({"name": "bad.profile", "consumer_kind": "recall"}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "r3-3: dot in name must return InvalidInput; got {err:?}"
        );

        // Underscore in name must be rejected.
        let err = pack
            .dispatch(
                "brain.create_profile",
                json!({"name": "bad_profile", "consumer_kind": "recall"}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "r3-3: underscore in name must return InvalidInput; got {err:?}"
        );

        // Asterisk in name must be rejected.
        let err = pack
            .dispatch(
                "brain.create_profile",
                json!({"name": "*", "consumer_kind": "recall"}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "r3-3: asterisk name must return InvalidInput; got {err:?}"
        );

        // Valid alphanumeric-hyphen name must succeed.
        pack.dispatch(
            "brain.create_profile",
            json!({"name": "valid-profile-123", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .expect("r3-3: valid alphanumeric-hyphen name must succeed");
    }
}

#[cfg(test)]
mod help_tests {
    use super::*;

    fn find_handler(name: &str) -> &'static HandlerDef {
        BRAIN_HANDLERS
            .iter()
            .find(|h| h.name == name)
            .unwrap_or_else(|| panic!("handler {name:?} not found in BRAIN_HANDLERS"))
    }

    #[test]
    fn brain_feedback_params_non_empty_and_has_target_and_signal() {
        let h = find_handler("brain.feedback");
        assert!(!h.params.is_empty(), "brain.feedback must have params");
        assert!(
            h.params.iter().any(|p| p.name == "target_id" && p.required),
            "brain.feedback must have required target_id param"
        );
        assert!(
            h.params.iter().any(|p| p.name == "signal" && p.required),
            "brain.feedback must have required signal param"
        );
        assert!(
            h.params.iter().any(|p| p.name == "served_by_profile_id"),
            "brain.feedback must document served_by_profile_id"
        );
    }

    #[test]
    fn brain_profile_params_has_required_profile_id() {
        let h = find_handler("brain.profile");
        assert!(!h.params.is_empty(), "brain.profile must have params");
        // H4: canonical param is now profile_id; id is accepted as a runtime alias.
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "profile_id" && p.required),
            "brain.profile must have required profile_id param (H4 fix)"
        );
    }

    #[test]
    fn brain_profiles_params_has_lifecycle_filter() {
        let h = find_handler("brain.profiles");
        assert!(!h.params.is_empty(), "brain.profiles must have params");
        assert!(
            h.params.iter().any(|p| p.name == "lifecycle"),
            "brain.profiles must document lifecycle filter param"
        );
    }

    #[test]
    fn brain_resolve_params_has_consumer_kind_required() {
        let h = find_handler("brain.resolve");
        assert!(!h.params.is_empty(), "brain.resolve must have params");
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "consumer_kind" && p.required),
            "brain.resolve must have required consumer_kind"
        );
        assert!(
            h.params.iter().any(|p| p.name == "actor"),
            "brain.resolve must document optional actor"
        );
        assert!(
            h.params.iter().any(|p| p.name == "namespace"),
            "brain.resolve must document optional namespace"
        );
    }

    #[test]
    fn brain_bind_params_has_required_profile_id_and_optionals() {
        let h = find_handler("brain.bind");
        assert!(!h.params.is_empty(), "brain.bind must have params");
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "profile_id" && p.required),
            "brain.bind must have required profile_id"
        );
        assert!(
            h.params.iter().any(|p| p.name == "actor"),
            "brain.bind must document actor"
        );
        assert!(
            h.params.iter().any(|p| p.name == "namespace"),
            "brain.bind must document namespace"
        );
        assert!(
            h.params.iter().any(|p| p.name == "consumer_kind"),
            "brain.bind must document consumer_kind"
        );
        assert!(
            h.params.iter().any(|p| p.name == "priority"),
            "brain.bind must document priority"
        );
    }

    #[test]
    fn brain_unbind_params_non_empty_all_optional() {
        let h = find_handler("brain.unbind");
        assert!(!h.params.is_empty(), "brain.unbind must have params");
        assert!(
            h.params.iter().all(|p| !p.required),
            "brain.unbind params must all be optional (filter semantics)"
        );
        assert!(
            h.params.iter().any(|p| p.name == "profile_id"),
            "brain.unbind must document profile_id filter"
        );
        assert!(
            h.params.iter().any(|p| p.name == "actor"),
            "brain.unbind must document actor filter"
        );
    }

    #[test]
    fn brain_activate_deactivate_archive_each_have_profile_id() {
        for verb in ["brain.activate", "brain.deactivate", "brain.archive"] {
            let h = find_handler(verb);
            assert!(!h.params.is_empty(), "{verb} must have params");
            assert!(
                h.params
                    .iter()
                    .any(|p| p.name == "profile_id" && p.required),
                "{verb} must have required profile_id param"
            );
        }
    }

    #[test]
    fn brain_reset_params_has_required_profile_id() {
        let h = find_handler("brain.reset");
        assert!(!h.params.is_empty(), "brain.reset must have params");
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "profile_id" && p.required),
            "brain.reset must have required profile_id param (C1 fix)"
        );
    }

    #[test]
    fn brain_config_params_has_parameter() {
        let h = find_handler("brain.config");
        assert!(
            !h.params.is_empty(),
            "brain.config must document the parameter arg"
        );
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "parameter" && !p.required),
            "brain.config parameter must be optional"
        );
    }

    #[test]
    fn brain_events_params_has_limit() {
        let h = find_handler("brain.events");
        assert!(
            !h.params.is_empty(),
            "brain.events must document the limit arg"
        );
        assert!(
            h.params.iter().any(|p| p.name == "limit" && !p.required),
            "brain.events limit must be optional"
        );
    }

    #[test]
    fn brain_emit_params_non_empty_with_target_and_signal() {
        let h = find_handler("brain.emit");
        assert!(
            !h.params.is_empty(),
            "brain.emit must have params (mirrors brain.feedback)"
        );
        assert!(
            h.params.iter().any(|p| p.name == "target_id" && p.required),
            "brain.emit must have required target_id"
        );
        assert!(
            h.params.iter().any(|p| p.name == "signal" && p.required),
            "brain.emit must have required signal"
        );
    }

    #[test]
    fn brain_bindings_params_all_optional() {
        let h = find_handler("brain.bindings");
        assert!(
            h.params.iter().all(|p| !p.required),
            "brain.bindings: all params must be optional filter args"
        );
        assert!(
            h.params.iter().any(|p| p.name == "profile_id"),
            "brain.bindings must document profile_id filter"
        );
        assert!(
            h.params.iter().any(|p| p.name == "consumer_kind"),
            "brain.bindings must document consumer_kind filter"
        );
    }

    #[test]
    fn brain_create_profile_params_has_required_name() {
        let h = find_handler("brain.create_profile");
        assert!(
            !h.params.is_empty(),
            "brain.create_profile must have params"
        );
        assert!(
            h.params.iter().any(|p| p.name == "name" && p.required),
            "brain.create_profile must have required name param"
        );
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "consumer_kind" && !p.required),
            "brain.create_profile consumer_kind must be optional"
        );
    }
}
