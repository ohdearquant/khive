# ADR-058: Brain Posterior Read Path — Wiring Profile Posteriors into Recall Ranking

**Status**: Proposed\
**Date**: 2026-06-15\
**Authors**: lambda:khive (architect draft)\
**Depends on**:

- [ADR-021](ADR-021-memory-pack.md) (Memory Pack — recall scoring is a research surface, weights are starting values)
- [ADR-032](ADR-032-brain-profile-orchestration.md) (Brain as profile-orchestration; three-scalar Beta posteriors)
- [ADR-033](ADR-033-recall-pipeline.md) (Recall pipeline)
- [ADR-035](ADR-035-cli-config-and-auto-embed.md) (Profile resolution order; `brain.resolve` / binding table)
- [ADR-017](ADR-017-pack-standard.md) (Pack standard; cross-pack dispatch via `VerbRegistry`)

**Relates**: closes the design gap in #85; makes #62's posterior-serving verification passable.

---

## Seed-readiness notice

Until this ADR is implemented, **"profile-tuned recall" and "self-improving recall" must not be
claimed in any pitch deck, investor README, or product copy.** The posteriors are written and
persisted correctly; the read path that would make them steer ranking does not exist. This ADR
is the plan to make the claim true — it is not yet true.

---

## Context

The brain pack maintains per-profile Bayesian posteriors over three recall signals — relevance,
salience, temporal — that are meant to tune recall ranking per profile
([ADR-032](ADR-032-brain-profile-orchestration.md) §three-scalar Beta posteriors). The **write
path works**: `brain.feedback` events update the posteriors, persisted to the
`brain_profile_snapshots` table (`crates/khive-pack-brain/src/persist.rs:264`,
`crates/khive-pack-brain/src/persist.rs:286`). The **read path does not exist**: recall ranking
never consumes the resolved profile's posteriors.

### Verified current state (source-cited, double-confirmed)

These findings were independently confirmed by an architect TBV pass and a gemini
adversarial-mirror REFUTE-stance review. Both returned CONFIRMED on all findings below.

Two posterior-consumption mechanisms are present in code; both are dead at ranking time.

**1. `PackTunable::project_config` / `apply_config` — zero production callers.**

- The trait is defined at `crates/khive-brain-core/src/tunable.rs:15-19`:
  `parameter_space()`, `project_config(&self, state: &BalancedRecallState) -> Value`,
  `apply_config(&self, config: Value) -> Result<(), RuntimeError>`.
- The **only** implementation in the workspace is `impl PackTunable for MemoryPack`
  (`crates/khive-pack-memory/src/tunable.rs:22`; confirmed via `grep "impl PackTunable"` over
  `crates/` returning exactly one hit). `project_config` reads the three posterior means into a
  `RecallConfig` (`crates/khive-pack-memory/src/tunable.rs:56-71`); `apply_config` deserializes,
  validates, and stores it into `self.config` (`crates/khive-pack-memory/src/tunable.rs:78-84`).
- **No production code calls `project_config`, `apply_config`, or `parameter_space`.** A grep over
  `crates/` excluding `tests/`, the trait definition, the impl file, and re-exports returns only
  doc comments. The only callers are unit tests in `crates/khive-pack-memory/src/tunable.rs`
  (tests module, lines 87-202) and the integration regression at
  `crates/khive-pack-memory/tests/integration.rs:963` (the `#[tokio::test]` function
  `test_pack_tunable_apply_config_affects_recall_score`, a `#[test]`, not production).
- The recall handler picks up `apply_config` results only via `MemoryPack::active_config()`
  (`crates/khive-pack-memory/src/pack.rs:46-48`), which clones `self.config`. That field is
  initialized to `RecallConfig::default()` (`crates/khive-pack-memory/src/pack.rs:55`) and is
  mutated **only** by `apply_config`. With no production caller, the active config is permanently
  the static default (relevance 0.70, salience 0.20, temporal 0.10 —
  `crates/khive-pack-memory/src/config.rs:362-388`).

**2. `BrainProfileHint` in `RecallConfig` — dead config, never read at ranking time.**

- The field `pub brain_profile: Option<BrainProfileHint>` is defined at
  `crates/khive-pack-memory/src/config.rs:83` (the hint struct at lines 90-102), serde-defaulted
  to `None` (`crates/khive-pack-memory/src/config.rs:385`).
- The recall handler (`crates/khive-pack-memory/src/handlers/recall.rs`) **never reads
  `cfg.brain_profile`**. The full ranking path — fusion, scoring, rerank, MMR, supersedes
  suppression, sort (`recall.rs:196-464`) — contains no reference to the hint. Confirmed by grep:
  `brain_profile` appears in `recall.rs` zero times.

**Conclusion:** Profile-tuned recall is currently a false product claim. Posteriors are written
and persisted but never injected into ranking.

**3. `exploration_epoch` does NOT advance on `brain.feedback` — critical finding.**

- `BalancedRecallState.exploration_epoch` is defined at
  `crates/khive-brain-core/src/profile.rs:76`. It is incremented in exactly one location in the
  brain pack handlers: the reset handler (`crates/khive-pack-brain/src/handlers.rs:781`), which
  writes `record.exploration_epoch += 1` on the non-Bayesian branch, and
  `reset_posteriors()` (`crates/khive-brain-core/src/profile.rs:96`) for the balanced-recall
  and user-created-profile branches.
- The feedback handler (`crates/khive-pack-brain/src/handlers.rs:800-964`) applies
  `apply_signal` to the profile state (`handlers.rs:927` / `handlers.rs:934`) but **never
  increments** `exploration_epoch`. A grep over the feedback handler body returns zero hits for
  `exploration_epoch`.
- **Consequence:** The draft ADR's Option b as originally written — keying the cache on
  `exploration_epoch` — would cache stale weights after every `brain.feedback` call and would
  only invalidate on a full `brain.reset`. This makes the original Option b broken for any
  production use where posteriors evolve through feedback rather than resets.

### Where the read path must inject

The single, narrow injection point is `crates/khive-pack-memory/src/handlers/recall.rs:73`:

```rust
let mut cfg = p.effective_config(self.active_config());
```

`active_config()` is where the per-call `RecallConfig` originates. `effective_config`
(`crates/khive-pack-memory/src/handlers/common.rs:180-189`) only overlays per-call `min_score` /
`min_salience` and an optional explicit `config`. To make ranking profile-aware, the three weights
(`relevance_weight`, `salience_weight`, `temporal_weight`) in `cfg` must reflect the **resolved
profile's** posterior means before `cfg.validate()` at `recall.rs:89`. `project_config` already
performs exactly this projection (`crates/khive-pack-memory/src/tunable.rs:56-71`) — what is
missing is the call site that resolves the profile, fetches its `BalancedRecallState`, and applies
the projection.

### Where the posteriors live, and how a profile is resolved

- `BalancedRecallState` (`crates/khive-brain-core/src/profile.rs:70-77`) carries the three
  `BetaPosterior` fields plus `entity_posteriors`, `total_events`, and `exploration_epoch`.
- Live profile state lives in the brain pack's `BrainState`
  (`crates/khive-brain-core/src/brain_state.rs:23-28`): `balanced_recall` (the default profile),
  `profile_states` (other profiles), `bindings`, `section_states`. The brain pack holds it behind
  a `Mutex` and loads it from `brain_profile_snapshots` on demand, replaying events since the
  snapshot.
- Profile resolution is `brain.resolve` (handler at `crates/khive-pack-brain/src/handlers.rs`,
  resolution logic `brain_state.rs` `resolve_with_match`). Order per
  [ADR-035](ADR-035-cli-config-and-auto-embed.md): explicit binding (`matched_binding=true`) →
  system default → active-profile scan. Bindings key on `(actor, namespace, consumer_kind)` with
  wildcards (`crates/khive-brain-core/src/profile.rs:57-65`). The recall consumer_kind is
  `"recall"` (`crates/khive-pack-memory/src/handlers/feedback.rs:48-49`).
- **Cross-pack access today is via `VerbRegistry` dispatch only** — there is no in-process API to
  fetch another pack's state. The proven precedent is `memory.feedback` resolving and routing
  through the registry: `resolve_namespace_profile` dispatches `brain.resolve`
  (`crates/khive-pack-memory/src/handlers/feedback.rs:96-122`) and treats the result as a hit only
  when `matched_binding == true` (lines 107-118); `route_to_brain` dispatches `brain.feedback`
  (lines 69-86). The read path follows the same dispatch pattern in reverse: resolve, then
  fetch the resolved profile's state.

Note: the knowledge pack also routes feedback through `brain.resolve(consumer_kind="recall")` for
the write path (`crates/khive-pack-knowledge/src/handlers.rs:362-403`) but likewise does not read
posteriors to rank `compose` output. The read path is dead for both consumers; this ADR scopes the
memory-pack recall path, which is the canonical case. The knowledge pack's equivalent gap is
flagged as an open question for Ocean (see Open Questions, Q4).

---

## Decision

**Adopt Option b-amended: an on-resolution cache, keyed on a monotonic `change_counter`
incremented on every `brain.feedback` application.**

The original Option b keyed the cache on `exploration_epoch`, which is confirmed ONLY incremented
on `brain.reset` (not on `brain.feedback`). Option b as originally drafted would therefore serve
stale posterior weights after every feedback event, invalidating only on reset. The amendment
introduces a separate `change_counter` (a `u64` field on the brain profile record) that the
feedback handler increments on every applied signal, providing exact cache invalidation without
the reset requirement.

The memory pack gains a small cached projection of the active recall profile's three weights,
keyed by `(namespace, profile_id, change_counter)`. On each recall:

1. Resolve the caller's recall profile using the same `brain.resolve(consumer_kind="recall")`
   dispatch the feedback write path already uses, including the `matched_binding` discipline
   (`crates/khive-pack-memory/src/handlers/feedback.rs:96-122`).
2. If the cache holds a projection for `(namespace, profile_id)` at the current
   `change_counter`, apply it to `cfg` (overwrite the three weights) and proceed. This is
   an O(1) lookup; no lock on the brain `Mutex` is required.
3. On cache miss or `change_counter` advance, fetch the resolved profile's
   `BalancedRecallState` and current `change_counter` via a new brain read verb (see Open
   Questions, Q2). Project the state via the existing `project_config` logic
   (`crates/khive-pack-memory/src/tunable.rs:56-71`). Store the projection in the cache keyed
   by `(namespace, profile_id, change_counter)`. Apply it.
4. When no profile resolves (no binding, system-default-only), apply nothing — `cfg` keeps the
   pack defaults. This mirrors the feedback write path's tier-3 fallback semantics exactly
   (`crates/khive-pack-memory/src/handlers/feedback.rs:55-60`).

### Where the `change_counter` lives and where it increments

- **Lives on**: the brain profile record (`crates/khive-pack-brain/src/handlers.rs` — the same
  `BrainState`-backed record object accessed throughout the feedback handler). A new `u64` field
  `change_counter` is added alongside `exploration_epoch` on the in-memory profile state and on
  the `brain_profile_snapshots` persisted row.
- **Increments at**: the feedback apply site in the feedback handler
  (`crates/khive-pack-brain/src/handlers.rs:926-944`), immediately after `apply_signal` is called
  on the profile state. This is the only mutation path for posteriors; incrementing here provides
  exact invalidation semantics — the counter advances if and only if the posteriors actually
  changed.
- **Read by**: the new brain read verb (step 3 above), which returns
  `{weights, change_counter, matched_binding}` in a single dispatch, halving registry round-trips
  relative to a separate resolve + state-fetch sequence.

### Cache invalidation semantics

The cache entry `(namespace, profile_id, change_counter)` is valid as long as the brain's
`change_counter` for the resolved profile matches the cached counter. After a feedback event, the
brain's counter is one greater; the next recall for that `(namespace, profile_id)` detects the
mismatch, refreshes from the brain, and updates the cache. The cost of a refresh is one registry
dispatch (the new brain read verb). After a `brain.reset`, `exploration_epoch` also increments,
which can be included in the cache key as an additional guard; but `change_counter` alone is
sufficient for exact invalidation.

The staleness window is: at most one recall call after the `change_counter` advances (i.e., after
a `brain.feedback` completes). For the human/agent feedback timescale, this is operationally
immediate.

### Rationale

Recall is on the hot path. ANN production work targets sub-millisecond-class query latency
([ADR-052](ADR-052-ann-production-lifecycle.md)), and the recall handler is already
stage-profiled per-call (`crates/khive-pack-memory/src/handlers/common.rs:38-54`). Adding a
cross-pack `brain.resolve` plus a state-fetch dispatch — two registry round-trips, each re-locking
the brain `Mutex` and potentially touching persistence — to **every** recall would be a
per-call tax on the path we are spending effort to keep fast. Per-recall freshness (Option a)
buys correctness the workload does not need: posteriors move on the slow human/agent-feedback
timescale, not within a recall burst.

The amended Option b closes the staleness hole at minimal added cost: one `u64` increment in the
already-mutating feedback path, one O(1) cache lookup per recall, and one registry dispatch only
on counter advance. The projection math already exists and is tested.

### Trade-off table

| Dimension                             | (a) per-recall                                             | (b-amended) counter-keyed cache (chosen)                                                            | (c) TTL cache                                                                |
| ------------------------------------- | ---------------------------------------------------------- | --------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------- |
| Recall latency                        | 2 registry dispatches + brain `Mutex` lock, **every call** | dispatch on cache miss / counter advance only; O(1) lookup otherwise                                | same as (b) plus a clock check per call                                      |
| Staleness window                      | none (always fresh)                                        | ≤ one recall after `change_counter` advances                                                        | ≤ min(TTL, one recall after counter advance)                                 |
| Complexity                            | lowest (no cache state)                                    | one cache map keyed on `(namespace, profile_id, change_counter)` + `u64` increment in feedback path | (b-amended) + TTL bookkeeping and a tuning knob                              |
| Correctness under concurrent feedback | exact                                                      | exact — counter advances on every feedback apply                                                    | bounded by TTL; may serve stale for up to the TTL window even after feedback |
| New brain surface                     | a verb to fetch projected weights / state                  | same                                                                                                | same                                                                         |

### Option c (TTL) — rejected

Option c adds a time-to-live in addition to the counter. It is rejected as strictly worse than
Option b-amended: the TTL serves stale weights for up to the TTL window after feedback has already
changed the posteriors, and it adds a redundant recompute when nothing has changed. Option
b-amended achieves exact invalidation with only an O(1) counter comparison. There is no gap that
a TTL closes that the counter does not. Option c is not retained as a fallback; the `change_counter`
fully subsumes it.

### Folding in #62 — what this read path enables

[#62](https://github.com/ohdearquant/khive/issues/62) raised three feedback-loop gaps. The read
path is the precondition for fixing two of them:

- **Posterior-serving verification (#62 gap 3).** Today the question "is the resolved profile's
  state consumed by ranking, and with what effect size?" has the answer "no". Once recall applies
  the projected weights, a controlled before/after becomes measurable: set the temporal posterior
  mean low (the observed `temporal α=1/β=9 ≈ 0.10` signal) and confirm recency contribution is
  suppressed in ranking. This is the test #85 says the design must make passable, and it directly
  exercises `temporal_weight` flowing from posterior to `compute_score`'s temporal contribution
  (`crates/khive-pack-memory/src/handlers/common.rs:228-251`).
- **Top-1-only positive crediting and the missing negative path (#62 gaps 1-2).** The current
  recall handler credits only the first result with an implicit-positive signal
  (`crates/khive-pack-memory/src/handlers/recall.rs:518-533`, `top_id` →
  `on_recall_hit`). That is self-confirming: it mostly re-encodes "the top hit had high salience",
  which ranking already guaranteed. A live read path makes the negative signal **actionable** —
  once posteriors demonstrably steer ranking, a `not_useful` / counterfactual-miss signal has a
  measurable downstream effect, so it is worth emitting. Without a read path, any negative-signal
  work is unobservable and therefore unjustified. This ADR does not specify the negative-signal
  mechanism (probe-based vs explicit agent discipline — #62's open ask); it removes the blocker
  that made designing it premature.

The mapping of posteriors to tunables is the three-to-three identity already encoded in
`parameter_space()` and `project_config()` (`crates/khive-pack-memory/src/tunable.rs:23-71`):
`memory::relevance_weight` ← `relevance.mean()`, `memory::salience_weight` ← `salience.mean()`,
`memory::temporal_weight` ← `temporal.mean()`. This ADR adopts that mapping unchanged; the broader
22-variable calibration inventory referenced in #85 is out of scope here and remains a tuning
surface ([ADR-021](ADR-021-memory-pack.md) §Scope: weights are starting values, not invariants).

---

## Consequences

**Positive**

- Profile-tuned recall becomes a true claim, testable by the #62 gap-3 experiment.
- The dead `project_config` projection logic gains its first production caller; no new scoring
  math is introduced.
- Recall latency is unchanged on the common path (cache hit); the resolution + projection cost is
  paid once per `(profile, change_counter)`.
- The negative-signal and crediting fixes (#62) become justified follow-ups rather than
  unobservable speculation.
- Exact cache invalidation: no TTL knob, no periodic churn, no stale-weight window after feedback.

**Negative / cost**

- A new brain read verb (weights/state fetch) must be added and validated — a small surface
  expansion on the brain pack, governed by [ADR-032](ADR-032-brain-profile-orchestration.md).
- A `change_counter` field must be added to the brain profile record and incremented in the
  feedback apply path. This requires a DB migration for the `brain_profile_snapshots` table
  (a new `change_counter INTEGER NOT NULL DEFAULT 0` column).
- The memory pack gains mutable cache state (`(namespace, profile_id, change_counter) → projected
  weights`), with the usual lock and eviction considerations. It must be bounded (e.g. LRU with
  a cap on tracked `(namespace, profile_id)` pairs).
- A one-recall staleness lag after a posterior update is accepted by design. Surfaces that require
  exact-fresh posteriors per call are not served by this design (none identified today).
- `BrainProfileHint` (`crates/khive-pack-memory/src/config.rs:90-102`) remains dead under this
  decision. It is a separate, orthogonal post-recall multiplicative-boost mechanism, not the
  weight-projection read path. This ADR neither wires nor removes it; that is a follow-up cleanup
  (recommend: delete it as dead config unless a boost use case is specified, per the repo's
  no-backwards-compat-shims standard).

**Neutral**

- No change to the `brain_profile_snapshots` schema beyond adding the `change_counter` column
  (additive migration; existing rows default to 0, which correctly signals "cache always stale"
  until the first feedback after the migration — safe, just triggers one cache miss per profile
  on restart).
- The cache is in-memory and rebuilt on restart, matching the existing `recall_state` posture
  (`crates/khive-pack-memory/src/pack.rs:32` — "Persistence is deferred — state is rebuilt
  from actions on restart").

---

## Alternatives considered

**(a) Per-recall resolution and projection.** Resolve + fetch + project on every recall. Rejected
as the default for the latency reason above. It remains the correct choice for any consumer that
later proves it needs exact-fresh posteriors per call; the chosen design can degrade to it by
disabling the cache.

**(b) Original Option b — epoch-keyed cache.** Cache keyed on `exploration_epoch`. Rejected:
`exploration_epoch` is confirmed to advance only on `brain.reset`
(`crates/khive-pack-brain/src/handlers.rs:779-782`) and `reset_posteriors()`
(`crates/khive-brain-core/src/profile.rs:96`). It does **not** advance on `brain.feedback`. An
epoch-keyed cache would therefore serve permanently stale weights in any production flow that
accumulates feedback without resetting. The amendment (adding `change_counter`) corrects this by
providing a counter that advances on every feedback application.

**(c) Cached projection with TTL invalidation.** Cache plus a time-to-live. Rejected as strictly
worse than the chosen design: it serves stale weights for up to the TTL window after feedback has
changed the posteriors AND adds a redundant recompute when nothing has changed. The `change_counter`
achieves exact invalidation with a lower per-call cost (one O(1) comparison vs a clock read).
There is no correctness or performance gap that a TTL addresses.

**(d) Brain pushes config into the memory pack on activation/feedback** (inverted control:
`brain.activate` / `brain.feedback` calls `MemoryPack::apply_config`). This is what the original
`apply_config` design implies ("future recall calls pick it up via `active_config()`",
`crates/khive-pack-memory/src/tunable.rs:20-21`). Rejected: it inverts the pack dependency
(`memory` already depends on `kg`, `crates/khive-pack-memory/src/pack.rs:74`; making `brain` reach
into `memory` adds a reverse coupling), it cannot serve multiple namespaces/profiles into a single
shared `self.config` slot (the active config is a single `Mutex<RecallConfig>`, not keyed by
profile or namespace — `crates/khive-pack-memory/src/pack.rs:22`), and it makes recall ranking
depend on activation ordering rather than on the caller's resolved binding. The chosen pull-based
design keys the projection by `(namespace, profile_id)` and resolves per caller, which the
push-into-one-slot model structurally cannot do.

---

## Open questions

### Q1 — RESOLVED: `exploration_epoch` does not advance on feedback

**Confirmed closed.** Source evidence:

- `exploration_epoch` advances in `reset_posteriors()` only
  (`crates/khive-brain-core/src/profile.rs:96`) and the reset handler's non-Bayesian branch
  (`crates/khive-pack-brain/src/handlers.rs:781`).
- The feedback handler (`handlers.rs:800-964`) calls `apply_signal` on the profile state but
  contains zero references to `exploration_epoch`.
- This is precisely why Option b-amended introduces `change_counter`: a field that advances on
  every feedback application and is independent of the reset-only epoch.

### Q2 — Open (Ocean / brain VP): new brain read verb contract

**What is the new brain read verb's exact contract?** Options:

- **(i)** The existing `brain.profile` verb already returns a snapshot/state summary. If it
  exposes the three posterior means and the current `change_counter` in its response, no new verb
  is needed — the memory pack can call `brain.profile` (resolve first via `brain.resolve`, then
  fetch). This is two dispatches per cache miss.
- **(ii)** A purpose-built verb — e.g. `brain.serve_weights(consumer_kind, namespace)` — that
  performs resolve + project inside the brain pack and returns
  `{weights: {relevance, salience, temporal}, change_counter, matched_binding}` in one dispatch.
  This halves the round-trips and matches the shape the cache directly needs.

**Recommendation**: Option (ii), for one-dispatch resolution. The choice touches the brain pack's
public surface ([ADR-032](ADR-032-brain-profile-orchestration.md)) and is the brain VP's call.

### Q3 — Open (Ocean / architect): binding semantics for the read path

**Should the projection apply only on a real binding (`matched_binding=true`), exactly like
the feedback write path tier-2, or also on a system-default active profile?** Feedback falls
through to pack-local state when only a system default matches
(`crates/khive-pack-memory/src/handlers/feedback.rs:88-122`). Symmetry argues recall should too
— apply tuned weights only on an explicit binding; otherwise use pack defaults. Confirm this is
the intended semantics so the read path and write path stay consistent.

### Q4 — Open (Ocean): knowledge pack read-path scope

**The gemini mirror flagged this independently.** The `knowledge.compose` verb also lacks read-path
wiring: it routes feedback correctly through `brain.resolve(consumer_kind="recall")`
(`crates/khive-pack-knowledge/src/handlers.rs:362-403`) but does not consume posteriors when
ranking compose output. This ADR scopes the memory-pack path. Should the knowledge pack be brought
to the same spec in a follow-up ADR, or in this one?

### Q5 — Open: `BrainProfileHint` disposition

**Delete or wire?** The `BrainProfileHint` struct (`crates/khive-pack-memory/src/config.rs:90-102`)
specifies a post-recall score-boost mechanism distinct from the weight-projection read path. It is
currently dead (`brain_profile` is never read in `recall.rs`). Options: delete it as dead config
per the no-backwards-compat-shims standard, or specify the boost use case and wire it separately.
Recommend deletion unless a boost use case is articulated.
