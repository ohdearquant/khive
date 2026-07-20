# khive-brain-core

Brain primitives: Beta-Binomial posteriors, the closed section-type taxonomy, profile
state, and deterministic/sampled weight derivation. Pure state and math — no storage,
no verb handlers; `khive-pack-brain` wires these primitives to the `brain.*` verbs
(`brain.feedback`, `brain.profile`, …) and recall-time ranking.

## Usage

```rust
use khive_brain_core::{BetaPosterior, EntityPosteriors};
use uuid::Uuid;

// A Beta-Binomial posterior over "is this useful" with a weak prior.
let mut relevance = BetaPosterior::new(7.0, 3.0);
relevance.update_success(); // alpha += 1.0 on a positive signal
relevance.update_failure(); // beta += 1.0 on a negative signal
let p_useful = relevance.mean();

// Bounded per-entity posteriors (LRU-evicted at `capacity`).
let mut entities = EntityPosteriors::new(10_000);
let id = Uuid::new_v4();
entities
    .get_or_insert(id, BetaPosterior::default)
    .update_success();
```

`BetaPosterior::try_new` rejects non-finite or non-positive `alpha`/`beta` (and so does
deserialization — invalid wire values fail closed rather than silently coercing).
`merge` combines two independent posteriors that share the same prior;
`apply_ess_cap` scales a posterior's effective sample size back toward its prior so a
single burst of feedback cannot dominate the estimate.

## Section types and weight derivation

```rust
use khive_brain_core::{derive_deterministic_weights, SectionPosteriorState, SectionType};

let state = SectionPosteriorState::new(); // seeded from SectionType::default_priors()
let weights = derive_deterministic_weights(&state); // HashMap<SectionType, f64>, posterior means

assert_eq!(SectionType::Overview.as_str(), "overview");
assert_eq!(SectionType::ALL.len(), 10);
```

`SectionType` is a closed 10-value taxonomy (`Overview`, `CoreModel`,
`BoundaryConditions`, `Formalism`, `OperationalGuidance`, `Examples`, `FailureModes`,
`ExpertLens`, `References`, `Other`) used to weight knowledge-atom sections during
composition. `SectionPosteriorState::weights` samples via Thompson sampling while
`exploration_epoch > 0` (early life, more exploration) and falls back to
`derive_deterministic_weights` (posterior means) once the epoch is exhausted, so
composition converges from exploratory to exploitative without a separate code path.

## Profiles and signals

`ProfileRecord` / `ProfileLifecycle` (`Defined` → `Registered` → `Active` / `Inactive`
→ `Archived`) model a brain profile's registry entry; `BalancedRecallState` is the live
Beta-posterior state for the built-in `balanced-recall-v1` profile (relevance,
salience, temporal scalars plus per-entity posteriors). `BrainSignal` is the decoded
signal vocabulary produced from raw events (`RecallHit`, `RecallMiss`, `Feedback`,
`SemanticFeedback`, `NoteAccessed`, …); `entity_signal` and `is_recall_positive` map a
`BrainSignal` to the posterior update it implies. `FeedbackSignal`
(`Useful`/`NotUseful`/`Wrong`) and `FeedbackEventKind`
(`ExplicitPositive`/`ExplicitNegative`/`ImplicitPositive`/`ImplicitNegative`/`Correction`)
are the two closed signal enums consumed by `brain.feedback` and semantic-fold updates
respectively; `FeedbackEventKind::update_weight` gives corrections 4x the posterior
weight of an implicit signal (2.0 vs 0.5).

`BrainState` aggregates all of the above (profile registry, per-profile
`BalancedRecallState`, per-profile `SectionPosteriorState`, bindings) with
`to_snapshot()` / `from_snapshot()` round-trips for persistence.

## Where this sits

`khive-brain-core` depends on `khive-runtime` for the `PackRuntime` trait and
`RuntimeError` — `PackTunable` (in `tunable.rs`) is an extension trait a pack
implements to expose a `ParameterSpace` of Beta-prior parameters to brain
auto-tuning, so this crate sits above the runtime rather than below it:

```text
types -> score -> storage -> db -> query -> runtime -> khive-brain-core -> khive-pack-brain
```

`khive-pack-brain` owns the `brain.*` verbs and storage; `khive-brain-core` is the
pure primitive layer it builds on. Design context: brain as profile-orchestration
over fold and objective
([ADR-032](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-032-brain-profile-orchestration.md)),
the section-type taxonomy
([ADR-048](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-048-knowledge-section-profiles.md)).

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
