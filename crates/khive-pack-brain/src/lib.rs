pub mod event;
pub mod fold;
pub mod state;
pub mod tunable;

use std::sync::Mutex;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use khive_fold::{Fold, FoldContext};
use khive_runtime::pack::PackRuntime;
use khive_runtime::{DispatchHook, KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_storage::event::{Event, EventFilter};
use khive_storage::types::PageRequest;
use khive_types::{Pack, VerbDef};

use crate::fold::EventFold;
use crate::state::BrainState;

const ENTITY_CACHE_CAPACITY: usize = 10_000;

pub struct BrainPack {
    runtime: KhiveRuntime,
    state: Mutex<BrainState>,
    fold: EventFold,
}

impl Pack for BrainPack {
    const NAME: &'static str = "brain";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const VERBS: &'static [VerbDef] = &BRAIN_VERBS;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

static BRAIN_VERBS: [VerbDef; 5] = [
    VerbDef {
        name: "brain.state",
        description: "Return current BrainState snapshot for inspection",
    },
    VerbDef {
        name: "brain.config",
        description: "Return projected config for a named pack parameter",
    },
    VerbDef {
        name: "brain.events",
        description: "List recent brain-relevant events for debugging",
    },
    VerbDef {
        name: "brain.reset",
        description: "Reset posteriors to priors (preserves event history)",
    },
    VerbDef {
        name: "brain.emit",
        description: "Manually emit a feedback event for a specific entity",
    },
];

impl BrainPack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        let fold = EventFold::new(ENTITY_CACHE_CAPACITY);
        let ctx = FoldContext::new();
        let state = fold.initial(&ctx);
        Self {
            runtime,
            state: Mutex::new(state),
            fold,
        }
    }

    async fn handle_state(&self, _params: Value) -> Result<Value, RuntimeError> {
        let state = self.state.lock().unwrap();
        let snapshot = state.to_snapshot();
        serde_json::to_value(&snapshot).map_err(|e| RuntimeError::InvalidInput(e.to_string()))
    }

    /// Public snapshot of the current `BrainState`.
    ///
    /// Equivalent to dispatching the `brain.state` verb but callable directly
    /// when you hold an `Arc<BrainPack>` (e.g. a test that registered the pack
    /// as a `DispatchHook` and wants to verify posteriors updated).
    pub fn snapshot(&self) -> crate::state::BrainStateSnapshot {
        self.state.lock().unwrap().to_snapshot()
    }

    async fn handle_config(&self, params: Value) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        struct ConfigParams {
            parameter: Option<String>,
        }
        let p: ConfigParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let state = self.state.lock().unwrap();
        match p.parameter {
            Some(key) => {
                let posterior = state
                    .parameters
                    .get(&key)
                    .ok_or_else(|| RuntimeError::NotFound(format!("parameter {key:?}")))?;
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
                let configs: serde_json::Map<String, Value> = state
                    .parameters
                    .iter()
                    .map(|(k, p)| {
                        (
                            k.clone(),
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
                "brain.emit".into(),
                "get".into(),
                "remember".into(),
            ],
            namespaces: vec![ns],
            ..EventFilter::default()
        };
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
                })
            })
            .collect();

        Ok(json!({
            "count": events.len(),
            "events": events,
        }))
    }

    async fn handle_reset(&self, _params: Value) -> Result<Value, RuntimeError> {
        let mut state = self.state.lock().unwrap();
        state.reset_posteriors();
        Ok(json!({
            "reset": true,
            "exploration_epoch": state.exploration_epoch,
        }))
    }

    async fn handle_emit(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        struct EmitParams {
            target_id: String,
            signal: String,
        }
        let p: EmitParams = serde_json::from_value(params)
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

        let event = khive_storage::event::Event::new(
            token.namespace().as_str().to_string(),
            "brain.emit",
            khive_types::SubstrateKind::Event,
            "brain",
        )
        .with_target(target)
        .with_data(json!({"signal": signal}));

        let store = self.runtime.events(token)?;
        store
            .append_event(event.clone())
            .await
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        // Update brain state from this event
        let ctx = FoldContext::new();
        let mut state = self.state.lock().unwrap();
        let current = std::mem::replace(
            &mut *state,
            BrainState::new(std::collections::HashMap::new(), 0),
        );
        *state = self.fold.step(current, &event, &ctx);

        Ok(json!({
            "emitted": true,
            "event_id": event.id.to_string(),
            "signal": signal,
            "target_id": target.to_string(),
        }))
    }
}

// ── ADR-063: inventory self-registration ─────────────────────────────────────

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

    fn verbs(&self) -> &'static [VerbDef] {
        &BRAIN_VERBS
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
            "brain.state" => self.handle_state(params).await,
            "brain.config" => self.handle_config(params).await,
            "brain.events" => self.handle_events(token, params).await,
            "brain.reset" => self.handle_reset(params).await,
            "brain.emit" => self.handle_emit(token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "brain pack does not handle verb {verb:?}"
            ))),
        }
    }
}

/// `BrainPack` as a post-dispatch hook (Issue #158).
///
/// When registered via `VerbRegistryBuilder::with_dispatch_hook`, every
/// successful verb dispatch calls `on_dispatch` with a synthesized `Event`.
/// The event is fed into `EventFold::step`, updating the brain's posteriors
/// in real time — no polling required.
///
/// This is opt-in: the hook must be explicitly registered. Registries that do
/// not load the brain pack are unaffected.
#[async_trait]
impl DispatchHook for BrainPack {
    async fn on_dispatch(&self, event: &Event) {
        let ctx = FoldContext::new();
        let mut state = self.state.lock().unwrap();
        // Replace state with fold result. BrainState is not Clone, so we
        // use mem::replace with a sentinel and immediately overwrite.
        let current = std::mem::replace(
            &mut *state,
            BrainState::new(std::collections::HashMap::new(), 0),
        );
        *state = self.fold.step(current, event, &ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::VerbRegistryBuilder;
    use serde_json::json;

    fn make_pack() -> BrainPack {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        BrainPack::new(rt)
    }

    fn empty_registry() -> VerbRegistry {
        VerbRegistryBuilder::new()
            .build()
            .expect("empty registry builds successfully")
    }

    #[tokio::test]
    async fn dispatch_unknown_verb_returns_invalid_input() {
        let pack = make_pack();
        let registry = empty_registry();
        let err = pack
            .dispatch(
                "brain.unknown",
                json!({}),
                &registry,
                &NamespaceToken::local(),
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
        let pack = make_pack();
        let registry = empty_registry();
        let result = pack
            .dispatch(
                "brain.reset",
                json!({}),
                &registry,
                &NamespaceToken::local(),
            )
            .await
            .unwrap();
        assert_eq!(result["reset"], json!(true));
        assert_eq!(result["exploration_epoch"], json!(1u64));
    }

    #[tokio::test]
    async fn dispatch_emit_invalid_signal_returns_invalid_input() {
        let pack = make_pack();
        let registry = empty_registry();
        let target = "00000000-0000-0000-0000-000000000001";
        let err = pack
            .dispatch(
                "brain.emit",
                json!({"target_id": target, "signal": "bad_signal"}),
                &registry,
                &NamespaceToken::local(),
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
        let pack = make_pack();
        let registry = empty_registry();
        let result = pack
            .dispatch(
                "brain.state",
                json!({}),
                &registry,
                &NamespaceToken::local(),
            )
            .await
            .unwrap();
        assert!(result.get("total_events").is_some(), "missing total_events");
        assert!(
            result.get("exploration_epoch").is_some(),
            "missing exploration_epoch"
        );
        assert!(result.get("parameters").is_some(), "missing parameters");
    }
}
