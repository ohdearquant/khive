# ADR-032: Brain as Profile-Orchestration over Fold + Objective

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: Ocean, lambda:khive\
**Depends on**:

- ADR-006 (Deterministic Scoring)
- ADR-017 (Pack Standard)
- ADR-021 (Memory Pack)
- ADR-022 (Events Query Surface)
- ADR-024 (Fold Cognitive Primitives)
- ADR-025 (Verb Speech Acts)
- ADR-027 (Dynamic Pack Loading)
- ADR-031 (Multi-Engine Retrieval)
- ADR-033 (Recall Pipeline)

**Supersedes**: old ADR-064 (Brain Architecture — scalar-weight-only event fold; replaced by
the profile-orchestration direction defined here)

---

## Context

### What existed before this ADR

The predecessor design (legacy ADR-064) framed the brain as a `Fold<Event, BrainState>` whose
state was a flat map of `pack::parameter_name → BetaPosterior`. Three scalars drove memory
recall: `recall::relevance_weight`, `recall::salience_weight`, `recall::temporal_weight`.
Events updated these posteriors in-place via a single `EventFold::step` implementation. The
approach was correct in its Bayesian mechanics and in the replay-determinism invariant; it was
insufficient in two respects:

1. **Fixed state shape.** Calibrating a multi-engine retrieval layer (ADR-031) requires
   engine-weight matrices, per-context-bucket parameters, and eventually salience adjustments,
   RL Q-values, and conformal calibration sets. None of these fit a flat
   `HashMap<String, BetaPosterior>`. Every new signal source required a brain core change.

2. **No profile evolution.** There was no mechanism to ship an improved feedback definition
   without silently discarding history. Operators could not run a candidate profile against
   historical events to evaluate it before promoting it.

### The framing shift

The right framing is quantitative-finance backtesting. A profile is a strategy; the event log
is market data; live evolution is paper trading; backtest is historical performance evaluation
under a quality metric; promotion is the deploy decision made on backtest evidence.

khive already ships the abstractions for this in `khive-fold` (ADR-024):

- `Fold<L, S>` — deterministic state derivation from a stream of entries — is exactly the
  profile's evolution rule. Folding events into profile state collapses the event-state
  possibility space to one trajectory.
- `Objective<T>` — scoring and selection — is both the profile's ranker at retrieval time
  and the backtest quality metric.
- `Anchor` — causal graph traversal — handles cursor look-ahead and look-behind for evolution
  rules that depend on temporal context.
- `Selector` — budget-constrained subset selection — handles top-k under budget, diversity,
  and context constraints.

`khive-fold` also ships objective composition combinators (`objective/compose.rs`): weighted
sum, fallback, threshold gating, score modification. Multi-objective quality metrics for
backtests fall out of these for free.

This ADR does not introduce new primitive traits. It orchestrates the existing ones into a
brain that supports event-sourced strategy profiles with replay-based backtesting. The
Bayesian Beta-posterior shape from the predecessor design survives as the state type of
`BalancedRecallProfile` — today's three-scalar recall calibration, migrated as the v1 default
profile. It is now one profile among many, not the shape of the brain itself.

---

## Decision

### 1. Brain is a meta-Fold

Brain is a `Fold<Event, BrainState>` whose derived state is a set of pipeline parameters.
In shipped v1, `BrainState` stores both the profile registry and the live Bayesian recall
state. The built-in `balanced-recall-v1` profile keeps its live posteriors in
`BrainState.balanced_recall`; user-created Bayesian profiles keep live posteriors in
`BrainState.profile_states`.

Brain observes pack events only. It never processes its own state-transition events. This
boundary prevents recursive self-tuning loops.

### 2. Shipped v1 profile record

The shipped registry row is `ProfileRecord`, not the generic `Profile` composition from
the target architecture:

```rust
pub struct ProfileRecord {
    pub id: String,
    pub description: String,
    pub consumer_kind: String,
    pub state_class: String,
    pub lifecycle: ProfileLifecycle,
    pub created_at: DateTime<Utc>,
    pub state_snapshot: Option<serde_json::Value>,
    pub total_events: u64,
    pub exploration_epoch: u64,
}

pub enum ProfileLifecycle {
    Active,
    Inactive,
    Archived,
}

pub struct BrainState {
    pub profiles: HashMap<String, ProfileRecord>,
    pub balanced_recall: BalancedRecallState,
    pub profile_states: HashMap<String, BalancedRecallState>,
    pub bindings: Vec<ProfileBinding>,
    pub section_states: HashMap<String, SectionPosteriorState>,
}
```

`state_snapshot` is JSON (`serde_json::Value`) owned by the profile record. Namespace-level
persistence writes the full `BrainStateSnapshot` JSON blob into `brain_profile_snapshots`.
The generic `Profile` struct, `ProfileMetadata`, `ProfileStateClass`, `SnapshotAdapter`,
and inference-hook fields remain target architecture and are not shipped v1 API.

`event_filter` makes the ADR-022 §3a unification concrete: every profile declares its
input substrate slice via `EventFilter`. The brain pack's `PackEventConsumer::event_filter`
(ADR-017) returns the union of its active profiles' filters, so the runtime's storage
query pushes the predicate down at the SQL layer — a deployment carrying thousands of
cold profiles pays index lookup, not per-profile Rust evaluation. Profiles that need
the `Objective<Event>` typed-shape (e.g., for `FilterFold` composition) use
`filter.as_objective()`.

### 3. System-wide event log

All packs emit structured events to a shared, append-only, time-ordered, schema-versioned
log. Default-on for every pack. Brain reads this log. Today's `brain.events` table generalizes
into this store.

```rust
pub struct Event {
    pub id:                     Uuid,
    pub timestamp:              DateTime<Utc>,
    pub namespace:              String,
    pub actor:                  Option<String>,
    pub verb:                   String,
    pub kind:                   EventKind,
    pub payload:                serde_json::Value,
    pub payload_schema_version: u32,
    pub profile_state_version:  Option<u64>,
}

pub enum EventKind {
    RecallExecuted,
    RerankExecuted,
    LinkCreated,
    TaskTransitioned,
    FeedbackExplicit,
    ProfileResolutionRecommended,
    ProfileMerged,                  // §11 brain.merge_profiles + §5a BetaPosterior::merge
    EmbeddingModelChanged,          // ADR-043 — migration started
    EmbeddingMigrationCompleted,    // ADR-043 — swap committed
    EmbeddingMigrationFailed,       // ADR-043 — controller entered Failed
    EmbeddingDriftDetected,         // ADR-043 — advisory only
    ProposalCreated,                // ADR-046 — agent KG proposal created
    ProposalReviewed,               // ADR-046 — review decision recorded
    ProposalApplied,                // ADR-046 — approved proposal executed
    ProposalWithdrawn,              // ADR-046 — proposer rescinded
    // ... one variant per pack verb that produces observable outcomes
}
```

Replay must handle every historical payload schema. A per-kind migration registry upgrades
old payloads to the current shape before profile evolvers see them. The event log is the
system of record; profile states are derivable from it.

**Profile-served events carry `served_by_profile_id` in their payload.** Events whose
production was shaped by a brain-resolved profile — `RecallExecuted`, `RerankExecuted`
(ADR-042), `FeedbackExplicit` — record the resolved profile id in their
payload. Other event kinds (`LinkCreated`, `TaskTransitioned`, …) do not carry this
field — they were never "served" by a profile, just observed by brain.

```rust
// Minimum payload shape for profile-served events.
#[derive(Serialize, Deserialize)]
pub struct ServedEventPayload {
    pub served_by_profile_id: Option<String>,  // None ⇒ default/no-binding path
    #[serde(flatten)]
    pub kind_specific: serde_json::Value,
}
```

This field exists for backtest correctness (§8): when backtesting profile B over a
window where profile A was the resolved binding, every `RecallExecuted` event in the
window has `served_by_profile_id = Some("A")` in its payload — the backtest can tell
that B is being scored against A's candidate set and either acknowledge the
interleaved counterfactual or skip the event.

### 4. interpret() — the single event-to-signal mapping

```rust
pub enum BrainSignal {
    RecallHit     { target_id: Uuid, latency_us: i64 },
    RecallMiss,
    SearchCompleted { latency_us: i64 },
    Feedback      { target_id: Uuid, signal: FeedbackSignal },
    NoteAccessed  { target_id: Uuid },
    Irrelevant,
}

pub fn interpret(event: &Event) -> BrainSignal {
    match event.verb.as_str() {
        "recall"      => /* outcome field + target_id → RecallHit or RecallMiss */,
        "search"      => BrainSignal::SearchCompleted { latency_us: .. },
        "feedback"    => /* parse payload.signal → Feedback */,
        "get"
        | "remember"  => /* target_id → NoteAccessed */,
        _             => BrainSignal::Irrelevant,
    }
}
```

There is no `BrainEvent` enum parallel to `Event`. The `interpret()` function is the single
mapping layer. Any pack that emits events through the standard dispatch path automatically
feeds brain. To add a new signal source, add one match arm to `interpret()`.

### 5. Profile state typology — Bayesian is one class among many

Every profile's state is opaque bytes to brain. The state's _class_ — what's inside the
bytes, how the evolver updates it, how it integrates with the inference engine — is
declared in `ProfileMetadata.state_class`. The runtime treats classes uniformly for
fold/snapshot/binding, but classes differ in their update mechanics and downstream
consumers.

```rust
pub enum ProfileStateClass {
    /// Conjugate Bayesian update (closed form). No inference-engine integration.
    Bayesian,
    /// Low-rank adapter weights. State persisted as SafeTensors + sidecar JSON.
    /// Integrates with `lattice-inference::LoraHook` (see §5b).
    LoraAdapter,
    /// Per-head attention bias vectors. State persisted as SafeTensors.
    /// Integrates with lattice-inference attention forward hook.
    AttentionBias,
    /// Vector codebook (centroids) for embedding quantization.
    /// Integrates with the embedding pipeline (lattice-embed quantization stage).
    QuantizationCodebook,
    /// Cluster centroids + per-cluster Bayesian/LoRA state. Pre-inference routing.
    /// Indexing math may use ruvector-rabitq / ruvector-hyperbolic-hnsw / ruvector-diskann.
    ClusterConditional(Box<ProfileStateClass>),
    /// Bounded ring buffer of recent (op, embedding, reward) tuples for RL aggregation.
    /// No inference-engine integration; feeds other profile classes.
    Trajectory,
    /// SequentialFold composition over multiple class states.
    Composite(Vec<ProfileStateClass>),
}
```

Every pack-internal event aggregator implements `Fold<Event, State>` from ADR-024 — the
combinators (`FilterFold`, `MapFold`, `SequentialFold`) compose uniformly across all
classes. What differs class-to-class is the inner `reduce` math and the inference-side
consumer.

#### 5a. Bayesian — `BalancedRecallProfile` (v1 default)

The predecessor design's three-scalar Bayesian state survives as the state type of
`BalancedRecallProfile`. Conjugate Beta updates from `FeedbackExplicit` events; informative
priors warm-start cold deployments; entity posteriors in a bounded LRU. No inference-
engine state — the profile's `ranker: Objective<RetrievalCandidate>` weights pre-computed
candidate features.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BetaPosterior {
    pub alpha: f64,
    pub beta:  f64,
}

impl BetaPosterior {
    pub fn new(alpha: f64, beta: f64) -> Self { Self { alpha, beta } }
    pub fn mean(&self)  -> f64 { self.alpha / (self.alpha + self.beta) }
    pub fn variance(&self) -> f64 {
        let n = self.alpha + self.beta;
        (self.alpha * self.beta) / (n * n * (n + 1.0))
    }
    pub fn effective_sample_size(&self) -> f64 { self.alpha + self.beta }
    pub fn update_success(&mut self) { self.alpha += 1.0; }
    pub fn update_failure(&mut self) { self.beta += 1.0; }
    pub fn thompson_sample(&self, rng: &mut impl rand::RngCore) -> f64 {
        // sample from Beta(alpha, beta) using the standard method
    }

    /// Combine evidence from two independent observers that share the same prior.
    /// Each posterior is its prior plus its observed (successes, failures); the
    /// merged posterior is the prior plus the sum of observations across both:
    ///     Beta(a₁ + a₂ − a_prior, b₁ + b₂ − b_prior)
    /// Used by `brain.merge_profiles` when two profile states for the same actor
    /// (e.g., per-device or per-shard splits) need to be unified, or when a
    /// transfer learns from a sibling actor's posterior under the same prior.
    pub fn merge(&self, other: &BetaPosterior, prior: &BetaPosterior) -> BetaPosterior {
        BetaPosterior {
            alpha: self.alpha + other.alpha - prior.alpha,
            beta:  self.beta  + other.beta  - prior.beta,
        }
    }
}

pub struct BalancedRecallState {
    pub relevance:  BetaPosterior,   // prior Beta(7, 3)
    pub salience:   BetaPosterior,   // prior Beta(2, 8)
    pub temporal:   BetaPosterior,   // prior Beta(1, 9)
    pub entity_posteriors: EntityPosteriors, // bounded LRU, 10K default
    pub total_events: u64,
    pub exploration_epoch: u64,      // increments on reset to invalidate stale views
}
```

Informative priors (`Beta(7,3)` warm-starts with effective sample size 10) let the profile
produce reasonable rankings immediately, without requiring a cold-start dataset. Real events
override priors after approximately ten observations.

Entity posteriors use a bounded LRU cache (10K entries default, configurable per namespace).
Old entries evict on capacity. The eviction order is deterministic (insertion order) given
the same event sequence.

**Explore vs exploit**: Thompson sampling for exploration (sample the posterior); posterior
mean for exploitation. The profile switches to exploration when posterior variance exceeds a
configurable threshold or when recent success rate drops below a configurable floor.

#### 5b. LoRA-adapter — neural profile state plugged into Lattice

**v1 status**: this state class is **gated on ADR-042** (Composable Rerank Pipeline). Until ADR-042 ships, khive has no khive-side call site that
consumes `LoraHook`, so LoRA-class profiles are _registerable_ (the typology is
in place) but no built-in profile uses this class. `§5a Bayesian` is the only
v1-active state class. Sections 5b–5g describe the typology so the substrate is
ready when local-inference call sites land.

A LoRA-class profile's state is a set of rank-r adapter matrices `(A: rank×d_in,
B: d_out×rank)` per `(layer_idx, module_name)` pair. The state is consumed by
`lattice-inference` at forward-pass time via the
[`LoraHook` trait](../../../../lattice/crates/inference/src/lora_hook.rs) —
khive does not implement inference. Lattice owns the surface; brain composes:

| Surface                        | Lattice crate / feature                                                                              | What it provides                                                                                                                                                         |
| ------------------------------ | ---------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Forward-pass adapter injection | `lattice-inference::lora_hook::LoraHook` (lattice ADR-008)                                           | Per-layer per-module hook: `apply(layer_idx, module: &str, x, output)` adds `scale·B@(A@x)` in-place                                                                     |
| Adapter type                   | `lattice-tune::lora::LoraAdapter` + `LoraLayer { a: Vec<f32>, b: Vec<f32>, rank }` (lattice ADR-031) | `HashMap<(usize, String), LoraLayer>`; row-major f32 matrices                                                                                                            |
| `LoraAdapter: LoraHook` bridge | `lattice-tune` feature `inference-hook` (MANDATORY for khive)                                        | `impl LoraHook for LoraAdapter` available only when feature enabled                                                                                                      |
| Adapter SafeTensors I/O        | `lattice-tune::lora::safetensors`                                                                    | PEFT/MLX-format load; tensor-level save                                                                                                                                  |
| Base-model registry            | `lattice-tune::registry::ModelRegistry` (lattice ADR-029)                                            | `RegisteredModel` with semver + `ModelStatus` lifecycle (Pending→Production→Archived); brain's **base_model_id + target_model_id** (composed identity) fields point here |
| Training primitives            | `lattice-tune::train`                                                                                | Full-corpus batched training loop; v1 path for offline LoRA training, NOT online per-event step                                                                          |
| GPU acceleration               | `lattice-fann` (via wgpu), feature `gpu`                                                             | Available when both crates compiled with `features = ["gpu"]`; CPU fallback otherwise                                                                                    |
| Distillation                   | `lattice-tune::distill`                                                                              | Compress teacher model behavior into adapters (cf. lattice ADR-030)                                                                                                      |

**Required feature flags** in `khive-pack-brain/Cargo.toml`:

```toml
lattice-inference = "X"
lattice-tune = { version = "X", features = ["inference-hook"] }
# Optional: GPU adapter compute
# lattice-tune = { version = "X", features = ["inference-hook", "gpu"] }
```

Without `inference-hook`, the `LoraAdapter: LoraHook` impl is absent and LoRA-class
profiles cannot be passed to the rerank forward pass — boot-time check rejects
their registration. Without `gpu`, gradient and forward compute fall back to
pure-Rust SIMD (`lattice-fann` `Vec<f32>` paths).

The brain-side Profile composes Lattice primitives:

```rust
use lattice_tune::lora::{LoraAdapter, LoraLayer};
use lattice_inference::lora_hook::LoraHook;
use khive_types::ModuleName;       // see §6 — versioned enum, NOT &'static str

pub struct LoraProfileState {
    /// Indexed by (layer_idx, ModuleName) — versioned-enum keys (§6) ensure
    /// snapshot compatibility across lattice's projection-name evolution.
    /// Adapter values use lattice's owned types directly.
    pub adapters: HashMap<(usize, ModuleName), LoraLayer>,
    pub learning_rate: f32,
    pub scale: f32,
    pub training_steps: u64,
    /// Foundation model the LoRA adapter is layered onto.
    /// Resolved through ModelRegistry (lattice-tune::registry, ADR-029).
    pub base_model_id: ModelId,

    /// The fine-tuned model identity once the LoRA is composed with the base.
    /// Derived deterministically: `target_model_id = hash(base_model_id ++ adapter_id ++ version)`.
    /// Identifies a *derivable* model — reranker, query paraphraser, future synthesizer —
    /// NOT the embedding model. Brain LoRA-class profiles tune the harness layer's
    /// lattice-inference call sites; the embedding model is static and upgraded via
    /// re-indexing, not online adaptation (ADR-011).
    /// Used as the `served_by_profile_id` payload anchor for events served by this profile.
    /// The consuming call site (rerank in ADR-042, paraphrase in a future ADR)
    /// applies this hook only when the active model's id matches.
    pub target_model_id: ModelId,

    /// The actual LoRA adapter weights (sidecar reference).
    pub adapter_id: AdapterId,

    /// Adapter weight version (immutable once bound).
    pub version: ModuleVersion,
}

/// Build a lattice LoraAdapter view over the profile state. The conversion is
/// O(num_layers) — zero-copy on the Vec<f32> matrices via Cow, name-mapping on
/// the keys.
impl LoraProfileState {
    pub fn to_lora_adapter(&self) -> LoraAdapter {
        let map = self.adapters.iter()
            .map(|((layer_idx, module), layer)| {
                ((*layer_idx, module.as_str().to_owned()), layer.clone())
            })
            .collect();
        LoraAdapter::from_layers(map, self.scale)
    }
}

/// Brain returns this from Profile::inference_hook.as_ref().map(|p| p.as_hook()).
/// LoraAdapter implements LoraHook directly (feature inference-hook), so the
/// bridge is the lattice impl — no new wrapper type required.
pub trait LatticeAdapterProvider: Send + Sync {
    fn as_hook(&self) -> Box<dyn LoraHook>;
}

impl LatticeAdapterProvider for LoraProfileState {
    fn as_hook(&self) -> Box<dyn LoraHook> {
        Box::new(self.to_lora_adapter())
    }
}

impl Fold<Event, LoraProfileState> for LoraEvolver {
    fn reduce(&self, mut state: LoraProfileState, event: &Event, _ctx: &FoldContext) -> LoraProfileState {
        if let Some(signal) = extract_feedback_signal(event) {
            // v1 online-step path: khive-pack-brain implements a pure-Rust SGD
            // step over (layer.a, layer.b) in-place. Uses lattice's apply_lora
            // for the forward residual; gradient math lives in khive (lattice
            // does not currently ship adapt_step; lattice-tune::train is full-
            // corpus batched, wrong shape for per-event). See lattice issue
            // requesting an online adapt_step primitive.
            //
            // No async, no IO, fully deterministic given the same starting state
            // and signal — see ADR-024 v1 Fold invariants.
            khive_pack_brain::lora::sgd_step(&mut state, signal);
            state.training_steps += 1;
        }
        state
    }
}
```

**Persistence**: SafeTensors handles the A/B tensors only — `safetensors_io::save_tensors`
takes `HashMap<String, Vec<f32>>`. The scalar fields of `LoraProfileState`
(`learning_rate`, `scale`, `training_steps`, `base_model_id`, `target_model_id`,
the ModuleName keys) ride a sidecar JSON serde blob inside the same snapshot
container. `khive-pack-brain` owns this codec — lattice does not ship a
struct-shaped `LoraProfileState` round-trip. See §6.1 for the SQLite-primary
persistence story; SafeTensors is the export-only format.

**Hot-swap**: `lattice-tune::registry::ModelRegistry` (lattice ADR-029) tracks
`RegisteredModel` metadata (semver, status, lineage) — it does NOT manage adapter
hot-swap directly. Brain holds an `ArcSwap<Box<dyn LoraHook>>` per binding context;
swapping a profile means writing a new `Box::new(new_state.to_lora_adapter())` into
the ArcSwap. The rerank dispatcher (ADR-042) reads from the ArcSwap per call. Cost
of "no adapter": the ArcSwap holds a `Box::new(NoopLoraHook)`, whose `#[inline(always)]`
empty body is eliminated by the compiler (lattice ADR-008).

**What LoRA-class profiles tune** (and what they do NOT):

| Surface                                                        | Adapted? | Mechanism                          | Why                                                                                                                                                                                    |
| -------------------------------------------------------------- | -------- | ---------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Embedding model (`lattice-embed`)                              | NO       | Static / re-index on model upgrade | Embeddings define the corpus's vector geometry; LoRA-adapting them would silently misalign stored vectors. Re-embed + re-index is the right path for embedding-model changes (ADR-011) |
| Reranker (small LLM via `lattice-inference`)                   | **YES**  | LoRA hook at forward pass          | Per-deployment quality signal; tight feedback loop via `RecallSelected` events. v1 entry point — ADR-042                                                                               |
| Query paraphraser (small LLM via `lattice-inference`)          | **YES**  | LoRA hook at forward pass          | Adapts to deployment-specific phrasing. Future ADR (mirrors ADR-042 shape)                                                                                                             |
| Synthesizer / consolidator (small LLM via `lattice-inference`) | **YES**  | LoRA hook at forward pass          | Trains "good summary" shape per deployment. Future ADR                                                                                                                                 |

The recall pipeline's embedding stage stays adapter-free. The adapter enters at
rerank (ADR-042) and analogous downstream lattice-inference call sites.

**Resolution-chain integration**: when a derivable-model call site (rerank in ADR-042,
paraphrase / synthesize in future ADRs) resolves a LoRA-class profile via
`brain.resolve(caller_ctx)`, brain reads the per-context ArcSwap. If
`state.target_model_id` matches the active call site's lattice model id,
`state.as_hook()` is passed to that forward pass. Otherwise the hook is dropped
(`NoopLoraHook` substitutes). LoRA weights are tied to one model's weight space —
the rerank model's hook does not work on the paraphrase model.

#### 5c. Attention-bias — selective head reweighting

State: per-head bias vectors `bias[head_idx]: Vec<f32>`. Stored as SafeTensors. Consumed
by a `lattice-inference` attention forward hook (parallel to `LoraHook`, scoped to
attention modules). Update rule: SGD step against a target rerank distribution. Useful
when full LoRA is overkill but specific heads should be amplified or suppressed for a
binding context.

#### 5d. Quantization-codebook — embedding-pipeline state

State: codebook centroids over the embedding distribution observed during operation.
Persisted via SafeTensors. Consumed by the embedding pipeline (`lattice-embed`
quantization stage). Update rule: residual-based codebook refinement. Indexing math
may compose ruvector primitives — `ruvector-rabitq` for quantized centroid storage,
`ruvector-hyperbolic-hnsw` for hierarchical centroid lookup, `ruvector-diskann` for
disk-resident centroids at billion-scale. Lattice owns the embedding forward path;
ruvector contributes the math kernels brain may delegate to.

#### 5e. Cluster-conditional — pre-inference routing

State: a centroid index over past queries plus a per-cluster sub-state (which is itself
a `ProfileStateClass` — `Bayesian` or `LoraAdapter` typically). When a recall arrives,
brain quantizes the query embedding, finds the nearest cluster (via ruvector-rabitq +
ruvector-hyperbolic-hnsw or ruvector-diskann depending on cluster cardinality), and
serves the corresponding sub-state. This pattern enables "many small profiles indexed
by query semantics" without explicit binding for every cluster.

#### 5f. Trajectory — RL signal aggregation

State: a bounded ring buffer of recent `(operator, embedding, reward, baseline)` tuples
analogous in shape to ruvector's `WasmTrajectoryBuffer` (but native, no WASM). Brain
uses this for offline importance sampling and replay-based backtest; it does not enter
the inference forward pass. Trajectory profile state can feed gradient computations
for LoRA-class profiles that share the same binding context.

#### 5g. Composite — multi-class profiles

A profile may declare `Composite([Bayesian, LoraAdapter, Trajectory])`. The Profile's
evolver is a `SequentialFold` over the class evolvers; the state is a struct of
per-class blobs; the snapshot adapter stitches per-class SafeTensors/serde blobs.
Resolution returns the full composite to the recall pipeline, which routes each
sub-state to its appropriate consumer (Bayesian → ranker weights, LoRA → LoraHook,
Trajectory → backtest pipeline).

### 6. Data flow

Brain registers as a `PackEventConsumer` (ADR-017). The runtime delivers events; brain
fans each delivered event to the matching profiles, applies their `Fold::reduce`, and
persists `(state, cursor)` atomically per profile. The runtime does NOT execute Folds
or persist profile state — those are pack territory.

```
EVENT LOG (substrate, ADR-022 §3b ordering: created_at ASC, event_id ASC for replay)
   |
   |-- live delivery:
   |     runtime queries events matching brain.event_filter()
   |       (union of active-profile filters, pushed down as SQL WHERE)
   |     for each event in canonical order:
   |       view = runtime.load_event_view(event)         ← ADR-041 §5 (event + observations)
   |       ctx  = RuntimeEventContext { namespace, event_cursor: EventCursor::from(&view.event) }
   |       brain.on_event(view, &ctx):
   |         for each active profile P where P.event_filter.matches(&view.event):
   |           tx = pack.storage.begin()
   |           state = tx.load_state(P.id)
   |           // LoraEvolver is Fold<Event, S> — pass &view.event (ADR-041 §5).
   |           state = P.evolver.reduce(state, &view.event, fold_ctx)
   |           tx.save_state(P.id, &state)
   |           tx.save_cursor(P.id, EventCursor::from(&view.event))
   |           tx.commit()                          ← atomic state + cursor (ADR-017)
   |           if snapshot_due(P): emit snapshot (lattice-tune::registry for LoRA,
   |                                              serde blob via SnapshotAdapter otherwise)
   |
   |-- recall (consumer-driven, not event-driven):
   |     P            = brain.resolve(actor, namespace, consumer_kind)   ← §10
   |     query_emb    = lattice_embed.embed_one(query, embed_model)
   |                       (no hook here in v1 — embed call site does not accept
   |                        a LoraHook today; tracked in lattice issue. See ADR-011.)
   |     candidates   = vector_search(query_emb, …)
   |     // Rerank pass (ADR-042) — the v1 LoraHook consumer. Brain hands the
   |     // resolved profile's hook (None for non-LoRA classes) to the rerank
   |     // forward; lattice-inference applies adapter deltas per layer.
   |     adapter_hook = P.inference_hook.as_ref().map(|h| h.as_hook())   ← §5b for LoRA
   |     ranked       = lattice_rerank::forward(candidates, adapter_hook.as_ref(),
   |                                            …) (ADR-042)
   |     top_k        = P.selector.select(ranked, k, weights)
   |     emit RecallExecuted event with payload.served_by_profile_id = Some(P.id)
   |       (closes the feedback loop via §4 interpret())
   |
   |-- catch-up (on registration / restart, ADR-017 PackEventConsumer):
   |     for each active profile P:
   |       cursor = tx.load_cursor(P.id) or EventCursor::zero()
   |       events = event_store.query(filter=P.event_filter, after=cursor,
   |                                  order=AscReplay)
   |       fold each through on_event (atomic per-event commit)
   |     switch to live delivery
   |
   |-- backtest (read-only, no live state mutation):
   |     state = restore_state(profile, starting_snapshot)
   |     for event in event_store.range(from, to, order=AscReplay):
   |       if event.kind == RecallExecuted:
   |         counterfactual = profile.ranker.select_top(candidates, k, obj_ctx)
   |         score          = quality.score(build_outcome(counterfactual, comparison))
   |         curve.push((EventCursor::from(event), score))
   |       state = profile.evolver.reduce(state, event, fold_ctx)
   |
   `-- snapshot:
         (live state already in pack SQLite — see §6.1; snapshot is a read-only
          export, not a primary persistence path)
         profile.snapshot_adapter.serialize(state) →
           SafeTensors (LoRA tensors) + sidecar JSON (scalars) — for portability,
           training import, audit (LoRA / AttentionBias / QuantizationCodebook)
           ruvector-snapshot + ruvector-delta-core blob (generic / Bayesian)
```

**Key invariants** (all derived from ADR-017 + ADR-022 §3b):

1. The runtime queries events using the §3b cursor-aware ascending replay query —
   `(created_at, event_id)` tiebreak guarantees no skipped events at clock-tie boundaries.
2. State and cursor are persisted in the same transaction in the **same backend** —
   the pack's primary SQLite store. See §6.1 below for how this works for LoRA-class
   profiles whose A/B matrices look like they want to live as files.
3. Cold profiles never run Rust-side filtering — their `EventFilter` rolls into the
   storage query's WHERE clause via the §3a closed-struct lowering (ADR-022).
4. `Fold::reduce` is pure (no async, no IO, no clock — ADR-024 v1 invariants). The
   LoRA evolver's online SGD step (`khive_pack_brain::lora::sgd_step`) is deterministic
   given the same starting weights and signal. Lattice's online `adapt_step` is a
   future primitive (filed as a lattice issue) — until it lands the gradient math
   lives in khive-pack-brain.

#### 6.1 LoRA two-resource atomicity — single-backend primary, SafeTensors as export

ADR-017 mandates state + cursor in the same transaction in the same backend. LoRA-class
profile state looks like it wants to span two resources (SQLite for cursor + scalars,
SafeTensors files for the A/B matrices) — that would be a two-phase commit problem.
Resolution: **SafeTensors is not the primary persistence layer**, it is an import/export
format. The primary store is the pack's SQLite, full stop.

Shipped v1 persistence is JSON snapshot based (V20 migration, `V20_BRAIN_PROFILE_PERSISTENCE`):

```sql
CREATE TABLE IF NOT EXISTS brain_profile_snapshots (
    profile_id    TEXT NOT NULL,
    namespace     TEXT NOT NULL DEFAULT 'default',
    snapshot_json TEXT NOT NULL,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (profile_id, namespace)
);

CREATE TABLE IF NOT EXISTS brain_event_log (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    profile_id TEXT NOT NULL,
    namespace  TEXT NOT NULL DEFAULT 'default',
    event_kind TEXT NOT NULL,
    payload    TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
```

The `ProfileRecord.state_snapshot` field stores the latest profile-local JSON snapshot.
`brain_profile_snapshots.snapshot_json` stores the namespace-level `BrainStateSnapshot`
used for reload. The LoRA tables (`profile_state_lora_layer`, `profile_state_scalars`,
`profile_cursor`) and SafeTensors import/export verbs are deferred until a native
lattice/LoRA rerank call site ships.

This is the right separation: the live state stays in one transactional backend; the
portable format is for sharing/training-import/audit, where the latency of "flush to
file" is acceptable and atomicity with the cursor is not required.

#### 6.1 Versioned `ModuleName` enum

LoRA adapter keys use module-name strings (`"q_proj"`, `"k_proj"`, …) per lattice
ADR-008. Using `&'static str` for these in khive snapshots makes the snapshot
compatibility hostage to the running binary's string table — if a future lattice
version renames a projection (or khive's binary is rebuilt with different string
interning), old snapshots either fail to deserialize or silently match the wrong
projection.

khive defines a closed `ModuleName` enum in `khive-types` and uses it as the snapshot
key. Serde tags identify each variant by stable name (not enum discriminant integer)
so adding variants is non-breaking; removing or renaming variants requires a
migration step.

```rust
// crates/khive-types/src/lora.rs
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleName {
    QProj,      // attention query projection
    KProj,      // attention key projection
    VProj,      // attention value projection
    OProj,      // attention output projection
    GateProj,   // FFN gate (SwiGLU)
    UpProj,     // FFN up
    DownProj,   // FFN down
    InProjQkv,  // GatedDeltaNet fused QKV
    InProjZ,    // GatedDeltaNet Z
    InProjB,    // GatedDeltaNet B
    InProjA,    // GatedDeltaNet A
    OutProj,    // GatedDeltaNet out
    // Add variants as lattice introduces new module-name conventions; never
    // rename or remove without a migration.
}

impl ModuleName {
    /// Wire string for the lattice LoraHook::apply call. Must match lattice's
    /// expected strings (lattice ADR-008 / ADR-031).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QProj    => "q_proj",
            Self::KProj    => "k_proj",
            // …
        }
    }

    pub fn from_lattice_str(s: &str) -> Option<Self> { /* exhaustive match */ }
}
```

Snapshot serde uses the enum tag (`"q_proj"`); the hot-path conversion to the wire
string is `&'static str` so the cost across the LoraHook boundary is zero. If lattice
adds a new projection, khive adds a new variant — old snapshots still deserialize the
variants they knew about; new bindings get the new variant. Renaming on the lattice
side without coordinating with khive is the failure mode lattice ADR-008's "Risks"
section calls out; this enum is khive's defence against it.

### 7. Snapshot and delta substrate

Profile state persists via `ruvector-snapshot::SnapshotManager` with delta encoding for
intermediate snapshots:

```rust
pub struct ProfileSnapshot {
    pub profile_id:        String,
    pub at_event_id:       Uuid,
    pub at_timestamp:      DateTime<Utc>,
    pub state_blob:        Vec<u8>,
    pub is_delta:          bool,
    pub base_snapshot_id:  Option<Uuid>,
}
```

Storage policy: full snapshot every N events; delta-encoded snapshots between full snapshots.
N is configurable per profile. State reconstruction:

1. Find the latest full snapshot at or before the target event id.
2. Apply intervening delta snapshots in order.
3. Apply remaining events via `evolver.reduce`.

This is `Fold::derive` starting from a non-initial state. No new code path.

### 8. Backtest = derive + score

```rust
pub struct BacktestRequest {
    pub profile_id:          String,
    pub from_event_id:       Uuid,
    pub to_event_id:         Option<Uuid>,
    pub starting_snapshot_id: Option<Uuid>,
    pub quality:             Box<dyn Objective<TrajectoryOutcome>>,
    pub comparison:          ComparisonTarget,
    /// Default false — events served by a different profile (per the payload's
    /// `served_by_profile_id`, §3) are skipped and reported in
    /// `BacktestResult::skipped_interleaved`. Set true to score every event
    /// regardless of who served it (interleaved counterfactual).
    pub allow_interleaved:   bool,
}

pub enum ComparisonTarget {
    AnotherProfile(String),
    ActualHistory,
    SyntheticGroundTruth(Vec<RankedResult>),
}

pub struct BacktestResult {
    pub profile_id:            String,
    pub events_replayed:       u64,
    pub recall_events_scored:  u64,
    pub aggregate_score:       f64,
    pub performance_curve:     Vec<(Uuid, f64)>,
    pub divergences:           Vec<Divergence>,
    /// RecallExecuted events skipped because they were served by a different
    /// profile and `allow_interleaved` was false. Empty when there were no
    /// binding changes in the window or `allow_interleaved` was true.
    pub skipped_interleaved:   Vec<Uuid>,
}
```

Implementation:

```rust
async fn backtest(req: BacktestRequest) -> Result<BacktestResult, RuntimeError> {
    let profile = registry.get(&req.profile_id)?;
    let mut state = restore_state(&profile, req.starting_snapshot_id).await?;
    let events = event_log.range(req.from_event_id, req.to_event_id).await?;
    let mut curve = Vec::with_capacity(events.len());
    let mut interleaved = Vec::new();    // events served by other profiles
    for event in &events {
        if let EventKind::RecallExecuted = event.kind {
            let served = decode_served_by(&event.payload);            // Option<String>
            // §8 interleaving rule: if another profile served this event,
            // the candidate set was shaped by that profile's filter. Scoring
            // req.profile_id against it is a counterfactual, not a replay.
            let is_interleaved = matches!(&served, Some(p) if p != &req.profile_id);
            if is_interleaved && !req.allow_interleaved {
                interleaved.push(event.id);
                state = profile.evolver.reduce(state, event, &fold_ctx);
                continue;
            }
            let candidates     = decode_candidates(&event.payload)?;
            let counterfactual = profile.ranker.select_top(&candidates, k, &ctx);
            let outcome        = build_outcome(counterfactual, &req.comparison, event);
            let score          = req.quality.score(&outcome, &obj_ctx);
            curve.push((event.id, score));
        }
        state = profile.evolver.reduce(state, event, &fold_ctx);
    }
    Ok(BacktestResult {
        performance_curve: curve,
        aggregate_score:   aggregate(&curve),
        skipped_interleaved: interleaved,
        ..
    })
}
```

**Interleaved-counterfactual rule.** When the backtest window contains events served
by a different profile than `req.profile_id`, the candidate set those events carry
was shaped by that other profile's filter and pre-rank. Scoring `req.profile_id`'s
ranker over those candidates measures "given someone else's candidates, what would
my ranker have picked" — a partial counterfactual, not a true replay.

`BacktestRequest` adds an `allow_interleaved: bool` field. Default `false` — interleaved
events are skipped and listed in `skipped_interleaved`. When `true`, the backtest
scores every `RecallExecuted` event regardless of which profile served it. Operators
get to see the interleaving honestly and decide which mode answers their question.

For `ComparisonTarget::AnotherProfile(other_id)` over a window where bindings changed
multiple times, the backtest evaluates both profiles under the same interleaving
rule. True end-to-end replay — where `req.profile_id`'s candidate-generation runs
from the embedding stage forward — is out of scope for v1 (would require rerunning
the engines as well as the ranker). Tracked for future when an operator has a
concrete need.

Quality is another `Objective`. Multi-metric quality is composition via
`khive-fold::objective::compose::WeightedObjective` — no new combinators needed.

### 9. Determinism boundary

khive uses `DeterministicScore` (i64 fixed-point, ADR-006) as the canonical cross-platform
score type. Some implementation components — notably HNSW inner loops and SIMD fusion — operate
in f32 and achieve replay determinism only on the same machine (via `to_bits()` hashing and
sorted iteration). This asymmetry is explicit and bounded:

**Rule**: at every boundary where a score enters the brain event log or profile state, convert
to `DeterministicScore`. Inside an adapter's inner loop (HNSW walk, SIMD fusion, MMR scoring)
f32 is permitted. The score that is _stored_ or _compared across profiles_ is fixed-point.

What this guarantees:

- All scores in events are bit-identical across platforms.
- All profile state evolution is bit-identical (state updates use fixed-point).
- All `Objective::score()` outputs that enter comparison are bit-identical.
- Backtest aggregate scores are bit-identical across platforms.

What this does not guarantee:

- HNSW walk order can vary across SIMD platforms. Top-k membership may differ by one or two
  items in rare cases. This is inherent to approximate nearest-neighbor search; we accept it.
- Profile promotion decisions are not affected — they compare backtest aggregate scores, which
  are deterministic.

### 9a. Stage-scoped feedback (per ADR-033/042 split)

Posterior updates are stage-scoped. A `FeedbackExplicit` event or an implicit signal
from a `RecallExecuted` / `RerankExecuted` event updates only the profile that served
that stage — not all profiles simultaneously.

**Stage-scoped feedback**:

- `consumer_kind="recall"`: feedback updates the recall profile's posteriors (ranking
  signals from recall-stage outcomes — which recall results were acted upon).
- `consumer_kind="rerank"`: feedback updates the rerank profile's posteriors (ranking
  signals from rerank-stage outcomes — whether rerank improved over the fused order).

Cross-stage propagation is NOT automatic. Each stage's posterior reflects only its
own outcomes. A caller who wants to update both stages must emit feedback events for
each stage independently. The `served_by_profile_id` field on profile-served events
(§3) identifies which profile to credit for each stage.

### 10. Profile lifecycle and resolution

Profile cardinality is unbounded at the ADR level, but shipped v1 includes one built-in
active fallback profile: `balanced-recall-v1`.

```
active  <->  inactive  ->  archived
```

- **Active**: the profile can be resolved and updated by feedback.
- **Inactive**: state is retained and the profile can be inspected, but live updates are stopped.
- **Archived**: terminal/read-only/audit-retained. No transition out of `Archived` is legal;
  `brain.activate(profile_id)` rejects archived profiles.

#### Resolution chain

When a consumer needs a profile, shipped v1 resolves from in-memory `BrainState.bindings`
using longest-match semantics. Bindings are persisted as part of the namespace
`BrainStateSnapshot`, not as a standalone SQLite `profile_bindings` table.

```
resolve(caller_ctx) -> profile_id

caller_ctx ::= {
    explicit_profile_id: Option<String>,
    actor:               Option<String>,
    namespace:           Option<String>,
    consumer_kind:       String,
}

Match order:
  1. explicit_profile_id
  2. (actor, namespace, consumer_kind)
  3. (actor, *, consumer_kind)
  4. (*, namespace, consumer_kind)
  5. (*, *, consumer_kind)
  6. (*, *, *)
  7. built-in fallback: balanced-recall-v1
```

Archived profiles are never returned. A binding that points at an archived profile is
ignored for live resolution. If no non-archived binding matches, `balanced-recall-v1`
is returned.

#### Binding workflow

A typical "train and serve per subagent per project" sequence:

```
1. Define profile shape  (Fold + Objective + Selector + state type)
2. brain.backtest(profile_id=candidate, event_filter=...)
     → derives state by replaying events through the Fold
     → persists snapshot
3. brain.activate(profile_id=candidate)
     → live update loop begins
4. brain.bind(profile_id=candidate,
              actor="implementer-α",
              namespace="lambda:khive",
              consumer_kind="recall")
     → next call by that actor in that namespace uses this profile
5. (Optional) brain.unbind(...)
     → resolution falls through to the next specificity tier
```

No global displacement. Multiple bindings coexist. A previously-bound profile keeps
running until explicitly unbound or archived; it is simply no longer resolved for that
binding tuple.

#### No auto-promotion

Brain MAY emit `ProfileResolutionRecommended` events when a registered profile
outperforms the currently-bound one by a configured margin on a configured backtest
window. Acting on the recommendation requires explicit `brain.bind`. Auto-binding is
out of scope — runaway feedback risk requires its own ADR.

### 11. Brain verb surface

> **Amendment (current shipped v1)**: verbs `brain.create_profile` and `brain.bindings`
> are public. `brain.profile` accepts canonical `profile_id` and legacy alias `id`.
> `brain.feedback` takes `(target_id, signal, served_by_profile_id?)`. `brain.reset`
> accepts optional `profile_id` and defaults to `balanced-recall-v1`.

| Verb                                                                     | Speech act (ADR-025) | Visibility | Purpose                                                                                                                                                                |
| ------------------------------------------------------------------------ | -------------------- | ---------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `brain.profiles`                                                         | assertive            | Verb       | List profiles, optionally filtered by lifecycle.                                                                                                                       |
| `brain.profile(profile_id)`                                              | assertive            | Verb       | Metadata, latest snapshot, current state summary. `profile_id` is canonical; `id` is accepted as a legacy alias.                                                       |
| `brain.resolve(actor?, namespace?, consumer_kind)`                       | assertive            | Verb       | Show which non-archived profile would serve this caller context, falling back to `balanced-recall-v1` when no binding matches.                                         |
| `brain.bindings(profile_id?, actor?, namespace?, consumer_kind?)`        | assertive            | Verb       | List `BrainState.bindings` rows, optionally filtered.                                                                                                                  |
| `brain.activate(profile_id)`                                             | commissive           | Verb       | Move an inactive profile to Active. Archived profiles reject because archive is terminal.                                                                              |
| `brain.deactivate(profile_id)`                                           | commissive           | Verb       | Move to Inactive.                                                                                                                                                      |
| `brain.archive(profile_id)`                                              | commissive           | Verb       | Move to Archived after deactivation; archived is terminal/read-only.                                                                                                   |
| `brain.reset(profile_id?)`                                               | declaration          | Verb       | Reset posteriors to priors, increment `exploration_epoch`, and sync `ProfileRecord.state_snapshot`. Defaults to `balanced-recall-v1`.                                  |
| `brain.create_profile(name, description?, consumer_kind?, seed_priors?)` | declaration          | Verb       | Create an inactive Bayesian `ProfileRecord` plus live `BalancedRecallState`. `seed_priors` seeds section posteriors, not the three recall Beta priors.                 |
| `brain.bind(profile_id, actor?, namespace?, consumer_kind?, priority?)`  | declaration          | Verb       | Write a binding row into `BrainState.bindings`. Archived profiles are rejected.                                                                                        |
| `brain.unbind(profile_id?, actor?, namespace?, consumer_kind?)`          | declaration          | Verb       | Remove binding rows. At least one filter is required.                                                                                                                  |
| `brain.feedback(target_id, signal, served_by_profile_id?)`               | commissive           | Verb       | Emit a `FeedbackExplicit` event and fold it into the selected profile's `BalancedRecallState`; defaults to `balanced-recall-v1` when `served_by_profile_id` is absent. |
| `brain.events`                                                           | assertive            | Subhandler | Debug event listing.                                                                                                                                                   |
| `brain.emit`                                                             | assertive            | Subhandler | Manual event emit/debug path; prefer `brain.feedback`.                                                                                                                 |
| `brain.config`                                                           | assertive            | Subhandler | Projected pack config for inspection.                                                                                                                                  |
| `brain.state`                                                            | assertive            | Subhandler | Return current `BrainState` snapshot for inspection.                                                                                                                   |

`brain.backtest`, `brain.compare`, `brain.snapshot`, `brain.merge_profiles`, LoRA adapter
import/export, and generic `Profile` composition are deferred target architecture, not
shipped v1 handlers.

Per [ADR-023](ADR-023-declarative-pack-format.md) §4 only kg owns bare verbs; everything
above carries the `brain.` prefix on the wire.

#### 11a. Per-profile Bayesian state

Every profile with `state_class = "Bayesian"` owns a live `BalancedRecallState` instance
from the moment it is created. The built-in `balanced-recall-v1` profile uses the
`BrainState.balanced_recall` field directly. User-created Bayesian profiles use
`BrainState.profile_states: HashMap<profile_id, BalancedRecallState>`.

- `brain.create_profile` allocates a fresh `BalancedRecallState` (default priors: Beta(7,3),
  Beta(2,8), Beta(1,9)) and stores a snapshot in `ProfileRecord.state_snapshot`.
- `brain.reset(profile_id)` calls `reset_posteriors()` on the profile's own state, increments
  `exploration_epoch`, and syncs the snapshot back to `ProfileRecord.state_snapshot`.
- `brain.feedback(served_by_profile_id=X)` folds the event into profile X's `BalancedRecallState`
  and syncs `total_events` + `state_snapshot` to the record.
- If `served_by_profile_id` is absent, feedback defaults to `balanced-recall-v1`.
- Archived profiles are rejected on the feedback write path (not just the lifecycle-transition
  path) — `served_by_profile_id` pointing at an archived profile returns `InvalidInput`.

### 12. Brain registers as a pack

Brain registers as `khive-pack-brain` via the pack registry (ADR-027). It observes pack
events emitted by the runtime; it never processes events emitted by its own state
transitions. This self-tuning prevention boundary is enforced by filtering on `EventKind`
before passing events to profile evolvers — brain-internal kinds (`ProfileResolutionRecommended`,
etc.) are excluded from the live update loop.

---

## Rationale

### Why profile = composition of existing primitives instead of a new Profile trait

`khive-fold` already ships exactly the required surface. Adding a `Profile` trait with
`apply`/`update`/`snapshot` methods would re-export what `Fold`, `Objective`, and
`ruvector-snapshot` provide, under different names. Abstractions multiplied beyond necessity.
The composition approach also unifies brain with the rest of khive's cognitive surface:
anyone who learned `Fold` and `Objective` for curation, retrieval, or recall already
understands how brain works.

### Why StateBlob is opaque to brain

Because state schema evolution must not require brain core changes. Today: scalar weights.
Tomorrow: per-note salience matrix. Next quarter: neural rerank weights. Each is a new `Fold`
impl with its own state type. Brain serializes via the snapshot adapter and never inspects
the bytes. Schema evolution is a contained change to one profile's code.

### Why backtest instead of live A/B by default

Backtest is deterministic, cheap, risk-free, and produces results in seconds to minutes. Live
A/B requires traffic splitting, user-visible behavior changes, and statistically significant
sample accumulation on real traffic. Backtest covers most calibration cases at a fraction of
the cost and risk. Reserve live A/B for signals backtest cannot capture.

### Why explicit operator promotion

Self-tuning systems can game their own quality metrics. Operator-in-the-loop is the safety
hatch. Auto-promotion is a future ADR with its own risk analysis.

### Why the Beta-posterior shape survives as BalancedRecallProfile

The predecessor design's Bayesian mechanics were correct. `BetaPosterior`, Thompson sampling,
and informative priors are well-grounded. The problem was not the update rule but the
fixed-shape state. Migrating the three-scalar design as a named profile preserves working
behavior, provides the v1 default, and demonstrates the profile pattern without requiring a
big-bang migration.

### Why system-wide event log spans all packs

Backtest fidelity requires full history. Recall outcomes depend on what entities were created,
what links were made, what tasks were transitioned. Whole-system event log enables
whole-system replay fidelity. Per-pack event silos would make cross-pack causal analysis
impossible.

### Why per-profile state stores instead of a shared store

Profiles disagree about success and failure for the same event. State is strategy-specific;
sharing it across profiles defeats the purpose. Per-profile storage cost is bounded by
parameters times profiles, both of which are small in practice.

### Why the informative prior is Beta(7,3) for relevance

Effective sample size of 10 means the profile starts with a 70% prior belief in relevance
success. This is a reasonable warm-start for a recall system: most recall invocations return
useful results. The prior can be overridden after approximately ten real events. Alternative:
flat Beta(1,1) prior — maximum uncertainty, but produces unstable rankings during cold-start.
The informative prior is a design choice made explicit so operators can override it per
deployment.

---

## Alternatives Considered

| Alternative                                              | Pros                              | Cons                                                                                     | Decision                                                                         |
| -------------------------------------------------------- | --------------------------------- | ---------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| Introduce a new Profile trait with apply/update/snapshot | Self-contained brain abstraction  | Re-exports what Fold + Objective already provide; unnecessary abstraction multiplication | Rejected                                                                         |
| Single canonical update rule, no profiles                | Simplest                          | Cannot evolve feedback definition; no A/B                                                | Does not address the real problem                                                |
| Live A/B for all profile comparisons                     | Real-traffic signal               | Slow, expensive, risk-bearing                                                            | Backtest covers most cases; rejected as default                                  |
| In-place posterior updates, no snapshots                 | Cheaper storage                   | No replay, no backtest, no audit                                                         | Forecloses everything this ADR enables; rejected                                 |
| Brain owns state schema; profiles fill in values         | Brain enforces structure          | Every state shape change is a brain breaking change                                      | Wrong locus of evolution; rejected                                               |
| Gradient-based optimization                              | Can learn nonlinear relationships | Requires differentiable loss, training loop, GPU                                         | Parameter space is ~10-50 posteriors; conjugate updates are O(1) exact; rejected |
| Brain as a separate service                              | Independent scaling               | Network latency on every pipeline call                                                   | Brain is pure math; no IO; rejected                                              |

---

## Consequences

### Positive

- **Architectural surface stays minimal.** Brain orchestrates existing `khive-fold` primitives
  rather than introducing new traits.
- **Calibration model is versionable.** Each (Fold, Objective) pair is a registered profile.
  New state shapes are new registered profiles. Old profiles keep working.
- **Backtest is trivial to implement.** Fold::derive plus Objective::select_top plus
  composition combinator. Approximately twelve lines of Rust.
- **Multi-objective quality.** Falls out of `compose.rs` combinators with no new code.
- **Audit and reproducibility.** Pin the snapshot id and event range; replay is deterministic.
- **Existing cognitive-primitives knowledge transfers.** Anyone who learned Fold and Objective
  understands brain immediately.
- **Cold-start mitigated.** Informative priors with effective sample size 10 produce reasonable
  rankings before sufficient events accumulate.
- **Bounded memory.** LRU cache on entity posteriors prevents unbounded growth.

### Negative

- **Existing scalar-weight Bayesian posteriors must migrate to a Fold.** Small one-time work;
  the algorithm does not change.
- **Snapshot storage cost.** Bounded by delta encoding and tunable snapshot interval.
- **More moving parts than the predecessor design.** Each piece is bounded and reuses existing
  primitives; the complexity is compositional, not accidental.

### Neutral

- Documentation explains the framework with worked examples. The new abstractions are
  pre-existing `khive-fold` types that contributors already encounter in memory and retrieval
  contexts.

---

## Implementation Phases

### Phase 0 — System-wide event log (foundation)

1. Move event emission from `khive-pack-brain` to `khive-runtime`. Every pack handler emits
   via the runtime; runtime persists to a shared `events` store.
2. Generalize event payload schema; add `payload_schema_version` and `EventKind` enum.
3. Make the log queryable by `(time_range, kind, namespace, actor, target_event_id)`.
4. Implement per-kind payload migration registry (old versions upgraded before evolvers
   see them).

### Phase 1 — Profile orchestration and snapshot integration

1. Define `Profile` struct in `khive-pack-brain` as the composition of `Fold` + `Objective` +
   optional `Anchor` + `Selector` + `SnapshotAdapter` + metadata.
2. Define `SnapshotAdapter` trait — single type-erased serialization method for the Fold's
   state type.
3. Wire `ruvector-snapshot::SnapshotManager` for `ProfileSnapshot` persistence.
4. Migrate today's three-scalar Bayesian state into `BalancedRecallProfile` composed of:
   - `Fold` that updates the three Beta posteriors from events via `interpret()`
   - `Objective` that ranks candidates by weighted combination of relevance, salience,
     temporal decay
   - `Selector` that picks top-k under budget
   - `SnapshotAdapter` for the three Beta pairs plus entity LRU cache
5. Live update loop: brain's `PackEventConsumer::on_event` (ADR-017) dispatches each
   matching event to active profiles' `evolver.reduce` with atomic state+cursor commit.

### Phase 2 — Backtest execution

1. Implement `brain.backtest` as the `derive + score` flow.
2. Register built-in quality objectives:
   - `cosine_alignment` (via `ruvector-coherence::quality::cosine_similarity`)
   - `subsequent_action_alignment` (cursor look-ahead via `Anchor`)
   - `explicit_feedback_alignment` (consumes `FeedbackExplicit` events)
   - `latency_penalty`
3. Implement `brain.compare` for head-to-head profile evaluation on a shared window.

### Phase 3 — Delta snapshots

1. Integrate `ruvector-delta-core` for delta-encoded intermediate snapshots.
2. GC old deltas after the next full snapshot supersedes them.
3. Per-profile snapshot interval configuration.

### Phase 4 — Rich-state reference profile

1. Build `PerNoteSalienceProfile` — Fold state is a per-note salience adjustment vector
   stored via `ruvector-temporal-tensor` (Hot/Warm/Cold tiering by access frequency).
2. Backtest against canonical `BalancedRecallProfile` to demonstrate non-trivial state shape.
3. Document the pattern as the reference for future profile authors.

### Phase 5 — Binding and recommendation workflow

1. Implement `brain.activate`, `brain.deactivate`, `brain.archive`.
2. Implement `brain.bind` / `brain.unbind` / `brain.resolve` over the
   `profile_bindings` table (§10).
3. Emit `ProfileResolutionRecommended` events when a registered profile outperforms
   the currently-bound profile for the same `(actor, namespace, consumer_kind)`
   binding tuple by the configured margin on the configured backtest window. Acting
   on the recommendation requires explicit `brain.bind` — no auto-binding (§10).
4. Operator CLI hook for review-and-bind flow.

---

## Open Questions

1. **Event log retention vs replay fidelity.** Unbounded log cost against reduced replay
   fidelity from compaction. Tentative: time-tiered retention (full events for N months,
   compacted summaries afterward; summaries sufficient for most but not all backtests).

2. **Profile state schema versioning across binary upgrades.** Schema change means new profile
   id, old snapshots keep working under old profile id, new profile starts fresh or migrates
   explicitly via a one-time migration path.

3. **Anchor cursor look-ahead bound.** Tentative K = 24 hours of event-log time. Unbounded
   look-ahead makes replay expensive and couples reconstruction cost to future event density.

4. **Multi-tenant profile definitions.** Each tenant has its own profile state. Profile
   definitions (the Fold and Objective code) are shared across tenants. Backtests are
   tenant-scoped. This is the expected cloud deployment shape; it is not enforced by this ADR.

5. **LRU cache size.** 10K entity posteriors is a conservative default. Should be configurable
   per namespace based on corpus size. Too small: frequent eviction resets learned entity
   signal. Too large: unbounded memory on large corpuses.

6. **Exploration schedule.** When to switch from exploit to explore? Posterior variance
   threshold? Success rate drop over a rolling window? Fixed epoch schedule? Needs empirical
   calibration on real usage data. `BalancedRecallProfile` ships with variance-threshold
   default; operators can override.

---

## References

- ADR-006 — Deterministic Scoring (`DeterministicScore`, i64 fixed-point, canonical ordering)
- ADR-017 — Pack Standard (brain registers as `khive-pack-brain`)
- ADR-021 — Memory Pack (provides the `recall` verb that brain tunes)
- ADR-022 — Events Query Surface (the substrate event log brain reads and all packs emit to)
- ADR-024 — Fold Cognitive Primitives (`Fold`, `Anchor`, `Objective`, `Selector`,
  composition combinators — the building blocks brain composes into profiles)
- ADR-025 — Verb Speech Acts (brain verbs classified as assertive, commissive, declaration)
- ADR-027 — Dynamic Pack Loading (brain pack self-registers via the pack registry)
- ADR-031 — Multi-Engine Retrieval (brain learns engine and strategy weights)
- ADR-033 — Recall Pipeline (recall calibration is brain's first tunable target)
- Legacy ADR-064 — Brain Architecture (event-driven scalar Beta posteriors; the historical
  predecessor to this ADR. The Bayesian mechanics survive as `BalancedRecallProfile`; the
  fixed-shape `BrainState` design is superseded by the profile-orchestration direction here)
- kkernel crate — the runtime that composes packs, hosts the event log, and runs the live
  update loop
- `ruvector-snapshot` — profile state persistence
- `ruvector-delta-core` — delta-encoded snapshots
- `ruvector-temporal-tensor` — time-evolving tiered state for rich-state profiles
- `ruvector-coherence::quality` — built-in cosine / L2 quality metrics
- Thompson, W.R., "On the likelihood that one unknown probability exceeds another" (1933)
- Friston, K. et al., "Active inference and epistemic value" (2015)
- Rao, R. & Ballard, D., "Predictive coding in the visual cortex" (1999)
- Anderson, J.R., "How Can the Human Mind Occur in the Physical Universe?" (2007) — ACT-R
  base-level activation as a structural analogue to Beta-posterior accumulation
