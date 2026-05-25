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
            description: "Specific parameter to query: \"recall::relevance_weight\" | \"recall::importance_weight\" | \"recall::temporal_weight\". Omit to return all.",
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
            name: "id",
            param_type: "string",
            required: true,
            description: "Profile ID string (e.g. \"balanced-recall-v1\"). NOT a UUID — use the string identifier.",
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
        params: &[],
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
        description: "Remove rows from the profile resolution table",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "profile_id",
                param_type: "string",
                required: false,
                description: "Remove bindings for this profile ID. All filters use AND semantics.",
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
            ("recall::importance_weight", &br.importance),
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
        let filter_lc: Option<ProfileLifecycle> = p
            .lifecycle
            .as_deref()
            .map(|s| serde_json::from_value(Value::String(s.to_owned())))
            .transpose()
            .map_err(|e| RuntimeError::InvalidInput(format!("invalid lifecycle: {e}")))?;

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
        #[derive(Deserialize)]
        struct ProfileParams {
            id: String,
        }
        let p: ProfileParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let state = self.state.lock().unwrap();
        let record = state
            .profiles
            .get(&p.id)
            .ok_or_else(|| RuntimeError::NotFound(format!("profile {:?}", p.id)))?;

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
        match state.resolve(p.actor.as_deref(), p.namespace.as_deref(), &p.consumer_kind) {
            Some(record) => Ok(json!({
                "resolved_profile_id": record.id,
                "lifecycle": record.lifecycle,
                "consumer_kind": record.consumer_kind,
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

        record.lifecycle = lifecycle.clone();
        Ok(json!({
            "profile_id": p.profile_id,
            "lifecycle": lifecycle,
        }))
    }

    // ── brain.reset ───────────────────────────────────────────────────────

    async fn handle_reset(&self, _params: Value) -> Result<Value, RuntimeError> {
        let mut state = self.state.lock().unwrap();
        state.reset_posteriors();
        // Fix #295: sync profile record after reset so brain.profile reflects
        // the restored domain-informed priors, not stale pre-reset values.
        sync_balanced_recall_record(&mut state);
        Ok(json!({
            "reset": true,
            "exploration_epoch": state.balanced_recall.exploration_epoch,
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

        // Update balanced-recall profile state from this event
        let ctx = FoldContext::new();
        let mut state = self.state.lock().unwrap();
        let current_recall = std::mem::replace(
            &mut state.balanced_recall,
            crate::state::BalancedRecallState::new(0),
        );
        let updated = self.fold.reduce(current_recall, &event, &ctx);
        state.balanced_recall = updated;

        // Fix #356 (MAJ-003): sync profile record metadata via shared helper.
        sync_balanced_recall_record(&mut state);

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

        // Verify the profile exists
        if !state.profiles.contains_key(&p.profile_id) {
            return Err(RuntimeError::NotFound(format!(
                "profile {:?}",
                p.profile_id
            )));
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
            // Commissive
            "brain.activate" => self.handle_activate(params).await,
            "brain.deactivate" => self.handle_deactivate(params).await,
            "brain.archive" => self.handle_archive(params).await,
            "brain.reset" => self.handle_reset(params).await,
            "brain.feedback" => self.handle_feedback(token, params).await,
            // Declaration
            "brain.bind" => self.handle_bind(params).await,
            "brain.unbind" => self.handle_unbind(params).await,
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
                json!({}),
                &registry,
                &rt.authorize(Namespace::local()),
            )
            .await
            .unwrap();
        assert_eq!(result["reset"], json!(true));
        assert_eq!(result["exploration_epoch"], json!(1u64));
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
        let result = pack
            .dispatch(
                "brain.archive",
                json!({"profile_id": "balanced-recall-v1"}),
                &registry,
                &rt.authorize(Namespace::local()),
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
        assert!(obj.contains_key("recall::importance_weight"));
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
        let target = "00000000-0000-0000-0000-000000000001";

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

        // Build a registry WITH the hook so we can trigger the double-count path.
        let mut builder = VerbRegistryBuilder::new();
        let hook: std::sync::Arc<dyn DispatchHook> = brain.clone();
        builder.with_dispatch_hook(hook);
        let registry = builder.build().expect("registry builds");

        let target = "00000000-0000-0000-0000-000000000002";

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

        // Step 2: also call handle_feedback directly to move importance away from prior.
        let target = "00000000-0000-0000-0000-000000000003";
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
            before.balanced_recall.importance.alpha > 2.0,
            "importance.alpha must have grown past prior after useful feedback"
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
            .dispatch("brain.reset", json!({}), &registry, &token)
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

        // importance prior = Beta(2,8)
        assert!(
            (after.balanced_recall.importance.alpha - 2.0).abs() < 1e-12,
            "#295: importance.alpha must be 2.0 after reset, got {}",
            after.balanced_recall.importance.alpha
        );
        assert!(
            (after.balanced_recall.importance.beta - 8.0).abs() < 1e-12,
            "#295: importance.beta must be 8.0 after reset, got {}",
            after.balanced_recall.importance.beta
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

        // state_snapshot: importance.alpha must be the prior value.
        let snap = &record["state_snapshot"];
        let imp_alpha = snap["importance"]["alpha"].as_f64().unwrap();
        assert!(
            (imp_alpha - 2.0).abs() < 1e-12,
            "#295: brain.profile state_snapshot importance.alpha must be 2.0 after reset, \
             got {imp_alpha}"
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
    fn brain_profile_params_has_required_id() {
        let h = find_handler("brain.profile");
        assert!(!h.params.is_empty(), "brain.profile must have params");
        assert!(
            h.params.iter().any(|p| p.name == "id" && p.required),
            "brain.profile must have required id param (not name)"
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
    fn brain_reset_params_empty() {
        let h = find_handler("brain.reset");
        assert!(
            h.params.is_empty(),
            "brain.reset takes no params — params slice must be empty"
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
}
