# ADR-064: Brain Architecture — Event-Driven Auto-Tuning via Meta-Fold

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-058 (Fold Cognitive Primitives), ADR-059 (Bayesian Extensions),
ADR-062 (Recall Pipeline), ADR-063 (Dynamic Pack Loading)

## Context

khive has packs (KG, GTD, memory) with configurable pipelines (ADR-062 RecallConfig). Today,
tuning those pipelines is manual: change a weight, observe results, adjust. There is no
mechanism for the system to learn from its own usage.

The pieces for self-tuning already exist:

- **Beta-Binomial conjugacy**: shipped in the memory pack for importance scoring. `Beta(alpha,
  beta)` tracks success/failure counts with closed-form posterior updates.
- **Thompson sampling** (Thompson 1933): canonical Bayesian exploration — sample from the
  posterior to decide what to try next.
- **Active Inference** (Friston et al. 2015): agents minimize expected free energy by balancing
  epistemic value (learn) and pragmatic value (exploit). ADR-059 adds `epistemic_weight` to
  Selector for exactly this.
- **Predictive Coding** (Rao & Ballard 1999, Friston 2005): precision-weighted prediction
  errors. ADR-059 adds `precision` to Objective.

The missing piece: **an event fold that processes usage events and updates pipeline parameters**.
That fold IS the brain.

### What is a Brain in khive?

> A **meta-fold**: a `Fold<Event, BrainState>` whose derived state is a set of pipeline
> parameters. Other folds (recall, retrieval, selection) consume those parameters as their
> configuration. The brain observes the system's own behavior, updates its beliefs about what
> works, and adjusts the pipelines accordingly.

This pattern has precedent in cognitive architectures: ACT-R's subsymbolic layer maintains
base-level activation `B_i = ln(Σ t_j^{-d})` — a decay-weighted count of successful retrievals
that is structurally analogous to a Beta posterior accumulating recall successes. The brain
formalizes this pattern with exact Bayesian updates instead of ACT-R's heuristic activation
decay. Thompson sampling for pipeline configuration also has production precedent (Seldon Core
ships a Thompson sampling router for ML model selection using Beta-Bernoulli posteriors).

The distinctive contribution here is the event-sourced deterministic fold: same events → same
BrainState → same behavior. This enables replay verification, which neither ACT-R's activation
nor production Thompson sampling routers provide.

## Decision

### 1. Event substrate — no parallel event types

The brain consumes the **existing** Event observable (ADR-004) directly. There is no
`BrainEvent` enum. Events are immutable and append-only — they are never modified or
soft-deleted, unlike notes. This guarantees the replay invariant: the brain can reconstruct
its state from the event stream at any point.

The brain **interprets** raw events by pattern-matching on `event.verb`, `event.outcome`,
`event.target_id`, and `event.data`:

```rust
/// Interpreted brain signal extracted from a raw Event.
pub enum BrainSignal {
    RecallHit { target_id: Uuid, latency_us: i64 },
    RecallMiss,
    SearchCompleted { latency_us: i64 },
    Feedback { target_id: Uuid, signal: FeedbackSignal },
    NoteAccessed { target_id: Uuid },
    Irrelevant,
}

pub fn interpret(event: &khive_storage::Event) -> BrainSignal {
    match event.verb.as_str() {
        "recall" => /* Success + target_id → RecallHit, else RecallMiss */,
        "search" => BrainSignal::SearchCompleted { .. },
        "brain.emit" => /* parse data.signal → Feedback */,
        "get" | "remember" => /* target_id → NoteAccessed */,
        _ => BrainSignal::Irrelevant,
    }
}
```

This means **any pack that emits Events through the standard dispatch path automatically
feeds the brain**. No coupling to memory or any specific pack. To add a new signal source,
extend `interpret()` with another verb match arm.

Explicit feedback uses the `brain.emit` verb, which creates a standard Event with structured
`data: {"signal": "useful|not_useful|wrong"}` and a `target_id` pointing to the entity.

### 2. Pack-generic tuning interface

The brain tunes any pack, not just memory. Every pack that wants auto-tuning implements
`PackTunable`:

```rust
pub trait PackTunable: PackRuntime {
    fn parameter_space(&self) -> ParameterSpace;
    fn project_config(&self, state: &BrainState) -> Value;
    fn apply_config(&self, config: Value) -> Result<(), RuntimeError>;
}

pub struct ParameterSpace {
    pub parameters: Vec<ParameterDef>,
}

pub struct ParameterDef {
    pub name: String,
    pub prior_alpha: f64,
    pub prior_beta: f64,
    pub bounds: (f64, f64),
}
```

Note: there is no `EventSchema` type. Event-to-parameter routing lives in `interpret()` +
`is_recall_positive()` (and future per-pack signal extractors) — not in a static schema
declaration. This is simpler: the brain reads the event stream directly, and `interpret()`
is the single place that maps verbs to brain signals.

The brain discovers tunable packs via the `PackRegistry` (ADR-063) at startup. It merges all
parameter spaces into a unified `BrainState` keyed by `pack::parameter_name`.

### 3. BrainState: pack-generic learned parameters

`BrainState` is the runtime type (not directly serializable due to the LRU cache).
`BrainStateSnapshot` is the serde-compatible projection used for persistence and inspection.

```rust
pub struct BrainState {
    pub parameters: HashMap<String, BetaPosterior>,
    pub entity_posteriors: EntityPosteriors, // bounded LRU, 10K capacity
    pub total_events: u64,
    pub exploration_epoch: u64,
}

#[derive(Serialize, Deserialize)]
pub struct BrainStateSnapshot {
    pub parameters: HashMap<String, BetaPosterior>,
    pub entity_posteriors: HashMap<Uuid, BetaPosterior>,
    pub total_events: u64,
    pub exploration_epoch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BetaPosterior {
    pub alpha: f64,
    pub beta: f64,
}

impl BetaPosterior {
    pub fn new(prior_alpha: f64, prior_beta: f64) -> Self {
        Self { alpha: prior_alpha, beta: prior_beta }
    }
    pub fn mean(&self) -> f64 { self.alpha / (self.alpha + self.beta) }
    pub fn variance(&self) -> f64 {
        let n = self.alpha + self.beta;
        (self.alpha * self.beta) / (n * n * (n + 1.0))
    }
    pub fn effective_sample_size(&self) -> f64 { self.alpha + self.beta }
    pub fn update_success(&mut self) { self.alpha += 1.0; }
    pub fn update_failure(&mut self) { self.beta += 1.0; }
}
```

Key differences from earlier draft:

- **`parameters`** is a generic `HashMap<String, BetaPosterior>` keyed by `pack::param_name`,
  not hardcoded recall fields. Populated from `PackTunable::parameter_space()` at startup.
- **`entity_posteriors`** uses an LRU cache (bounded, e.g., 10K entries) instead of an
  unbounded `HashMap`. Old entries evict when the cache is full. This prevents memory growth
  proportional to total memories ever created.
- **`effective_sample_size()`** exposes how much evidence a posterior has accumulated. Priors
  like `Beta(7.0, 3.0)` have an effective sample size of 10 — meaning the brain starts as if
  it has already observed 10 events (7 successes, 3 failures). This is a design choice:
  informative priors warm-start the brain but take ~10 real events to override.

### 4. EventFold: the brain as a fold over the Event substrate

```rust
pub struct EventFold {
    entity_capacity: usize,
}

impl Fold<khive_storage::Event, BrainState> for EventFold {
    fn initial(&self, _context: &FoldContext) -> BrainState {
        BrainState::new(
            [("recall::relevance_weight", Beta(7.0, 3.0)),
             ("recall::importance_weight", Beta(2.0, 8.0)),
             ("recall::temporal_weight", Beta(1.0, 9.0))].into(),
            self.entity_capacity,
        )
    }

    fn step(&self, mut state: BrainState, event: &Event, _ctx: &FoldContext) -> BrainState {
        let signal = interpret(event);  // raw Event → BrainSignal
        state.total_events += 1;

        // Global parameter updates
        if let Some(positive) = is_recall_positive(&signal) {
            if let Some(posterior) = state.parameters.get_mut("recall::relevance_weight") {
                if positive { posterior.update_success(); }
                else { posterior.update_failure(); }
            }
        }

        // Per-entity updates
        if let Some((entity_id, positive)) = entity_signal(&signal) {
            let posterior = state.entity_posteriors
                .get_or_insert(entity_id, || BetaPosterior::new(1.0, 1.0));
            if positive { posterior.update_success(); }
            else { posterior.update_failure(); }
        }

        state
    }
}
```

The EventFold is deterministic: same events in the same order produce the same BrainState.
The bounded LRU cache (FIFO eviction) is deterministic given the same insertion order.

Key difference from earlier draft: the fold takes `khive_storage::Event` directly, not a
custom `BrainEvent`. The `interpret()` function is the mapping layer — it extracts brain
signals from the universal event stream without requiring producers to know about the brain.

### 5. Config projection and explore/exploit

```rust
impl BrainState {
    pub fn project_config(&self, pack: &dyn PackTunable, mode: TuningMode) -> Value {
        let space = pack.parameter_space();
        let mut config = serde_json::Map::new();
        for param in &space.parameters {
            let key = format!("{}::{}", pack.name(), param.name);
            let posterior = self.parameters.get(&key)
                .unwrap_or(&param.prior);
            let value = match mode {
                TuningMode::Exploit => posterior.mean(),
                TuningMode::Explore(ref rng) => posterior.thompson_sample(rng),
            };
            config.insert(param.name.to_string(), value.clamp(param.bounds.0, param.bounds.1).into());
        }
        Value::Object(config)
    }
}

pub enum TuningMode {
    Exploit,
    Explore(Box<dyn rand::RngCore>),
}
```

- **Exploit** (default): posterior means as weights. Stable.
- **Explore**: Thompson samples. Each pipeline invocation uses slightly different parameters;
  outcomes update posteriors. This IS Thompson sampling applied to pipeline configuration.

Switch to explore when posterior variance is high or success rate drops.

### 6. Brain as a pack

The brain registers as `khive-pack-brain` via the pack registry (ADR-063):

| Handler        | What it does                               |
| -------------- | ------------------------------------------ |
| `brain.state`  | Return current BrainState (for inspection) |
| `brain.config` | Return projected config for a named pack   |
| `brain.events` | List recent events (for debugging)         |
| `brain.reset`  | Reset to priors (start learning over)      |
| `brain.emit`   | Manually emit an event (for testing)       |

No top-level verbs. The brain is infrastructure. Events are emitted automatically by pipelines.

### 7. Hoare triple

| Component         | Brain instantiation                                                                                                                                  |
| ----------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Precondition**  | Event stream is append-only (Event substrate, ADR-004). All BetaPosterior priors have alpha > 0, beta > 0.                                           |
| **Program**       | EventFold processes events in order, interpreting each via `interpret()` and routing to the correct posterior. LRU eviction is deterministic (FIFO). |
| **Postcondition** | BrainState is deterministic (replay-verifiable). All posteriors maintain alpha > 0, beta > 0. Projected configs have values within declared bounds.  |

## Alternatives Considered

### A. Brain as a separate service

Pros: independent scaling. Cons: network latency on every pipeline call. The brain is pure
math — no IO, no model inference.

Rejected. Brain is a pack.

### B. Gradient-based optimization

Pros: can learn non-linear relationships. Cons: requires differentiable loss, training loop,
GPU. The parameter space is ~10-50 Beta posteriors. Conjugate updates are exact and O(1).

Rejected. Beta posteriors are the right tool.

### C. No exploration — always use posterior means

Pros: simpler. Cons: local optima. Thompson sampling escapes suboptimal priors with minimal
overhead.

Rejected. Exploration is essential.

### D. Hardcoded BrainState for recall only

Pros: simpler first version. Cons: every new pack requires brain code changes — the same
static-match problem that motivated ADR-063 (dynamic pack loading). Building generic from
the start costs ~50 LOC more and scales without code changes.

Rejected. Generic via PackTunable from the start.

## Future Extensions

These extensions are deferred until the base brain ships and demonstrates value. They are
documented here to inform the design (the base architecture must not preclude them).

### Hierarchical brains (per-namespace + global priors)

Global brain provides learned priors for new namespaces. Each namespace specializes its own
posteriors. New projects warm-start from cross-project wisdom. Implementation: two-level
`HierarchicalBrain { global: BrainState, namespaces: HashMap<String, BrainState> }` with a
mixing rate controlling how much each event influences the global vs namespace posterior.

### Predictive brain (prediction error as learning signal)

The brain predicts outcomes before pipeline execution, then learns from prediction error
weighted by precision. `learning_rate = |actual - predicted| * precision` — surprising events
from a confident brain cause larger updates. This connects ADR-059's precision field to a
self-calibrating feedback loop. Note: the learning rule is `|error| * precision` (a confident
wrong prediction learns more), not `|error| / precision`.

### Attention gating (salience-weighted events)

Not all events are equally informative. Attention weight = surprise from prediction error +
habituation factor (repeated identical events have diminishing returns). Two modes: background
(low-salience, slow updates) and active (high-salience, fast updates). The prediction error
from the predictive brain extension IS the attention signal.

### Brain transfer and merge

BrainState is serializable Beta posteriors. Export/import enables cross-project learning.
Merge formula for two Beta posteriors with shared prior `Beta(a0, b0)`:
`merged = Beta(a1 + a2 - a0, b1 + b2 - b0)`. The shared prior must be subtracted to avoid
double-counting. For non-uniform priors, the prior parameters must be tracked explicitly.

## Consequences

### Positive

- **Pack-generic**: any pack gets auto-tuning by implementing `PackTunable` + emitting events
- **Deterministic**: event-sourced fold enables replay verification
- **Grounded in theory**: Beta-Binomial conjugacy, Thompson sampling, Active Inference
- **Observable**: `brain.state` and `brain.events` handlers expose internals
- **Bounded memory**: LRU cache on entity posteriors prevents unbounded growth

### Negative

- **Cold-start**: informative priors (effective sample size 10) mitigate but don't eliminate
- **Event emission burden**: 3-5 lines per pipeline call
- **Posterior drift**: exponential forgetting (multiply alpha/beta by decay factor periodically)
  addresses this but adds a hyperparameter

## Open Questions

1. **Event retention**: append-only forever (replay-complete) or windowed? Windowed saves
   storage but breaks full replay. Checkpointing (ADR-058) allows periodic snapshots with
   events since last checkpoint, bounding replay cost without windowing.
2. **LRU cache size**: 10K entity posteriors is a placeholder. Should be configurable per
   namespace based on corpus size.
3. **Exploration schedule**: when to switch from exploit to explore? Posterior variance
   threshold? Success rate drop? Fixed schedule? Needs empirical calibration.
4. **Self-tuning prevention**: the brain could recursively tune itself (brain events update
   brain parameters). This must be explicitly prevented — the brain observes pack events only,
   never its own state transitions.

## References

- ADR-058: Fold Cognitive Primitives — `Fold<L, S>`, `Checkpoint<S>`
- ADR-059: Bayesian Fold Extensions — precision, epistemic_weight
- ADR-062: Recall Pipeline — `RecallConfig` (first tunable target)
- ADR-063: Dynamic Pack Loading — `PackRegistry`, `PackFactory`
- Anderson, J.R., "How Can the Human Mind Occur in the Physical Universe?" (2007) — ACT-R
- Thompson, W.R., "On the likelihood that one unknown probability exceeds another" (1933)
- Friston, K. et al., "Active inference and epistemic value" (2015)
- Rao, R. & Ballard, D., "Predictive coding in the visual cortex" (1999)
- Seldon Core Thompson Sampling Router — production precedent for Beta-Bernoulli config tuning
