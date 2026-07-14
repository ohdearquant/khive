# ADR-058: Brain Posterior Read Path — Wiring Profile Posteriors into Recall Ranking

**Status**: Accepted\
**Date**: 2026-06-15 (updated 2026-06-23)\
**Authors**: khive maintainers
**Depends on**:

- [ADR-021](ADR-021-memory-pack.md) (Memory Pack — recall scoring is a research surface, weights are starting values)
- [ADR-032](ADR-032-brain-profile-orchestration.md) (Brain as profile-orchestration; three-scalar Beta posteriors)
- [ADR-033](ADR-033-recall-pipeline.md) (Recall pipeline)
- [ADR-035](ADR-035-cli-config-and-auto-embed.md) (Profile resolution order; `brain.resolve` / binding table)
- [ADR-017](ADR-017-pack-standard.md) (Pack standard; cross-pack dispatch via `VerbRegistry`)
- [ADR-015](ADR-015-schema-migrations.md) (Migration system; V5 migration specified here)

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

These findings were independently confirmed by an architecture pass and an internal
adversarial review. Both returned CONFIRMED on all findings below.

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
- **Consequence:** Any cache keyed on `exploration_epoch` alone would serve permanently stale
  weights after every `brain.feedback` call, invalidating only on a full `brain.reset`. This
  makes the original Option b broken for any production use where posteriors evolve through
  feedback rather than resets. The amendment below introduces `change_counter` to fix this.

### Where the read path must inject

The single, narrow injection point is `crates/khive-pack-memory/src/handlers/recall.rs:73`:

```rust
let mut cfg = p.effective_config(self.active_config());
```

`active_config()` is where the per-call `RecallConfig` originates. `effective_config`
(`crates/khive-pack-memory/src/handlers/common.rs:194-204`) only overlays per-call `min_score` /
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
flagged as an open question for maintainers (see Open Questions, Q4).

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
3. On cache miss or `change_counter` advance, call the new `brain.serve_weights` verb (see
   §New brain verb below). Store the returned projection in the cache keyed by
   `(namespace, profile_id, change_counter)`. Apply it.
4. When no profile resolves (no binding, system-default-only), apply nothing — `cfg` keeps the
   pack defaults. This mirrors the feedback write path's tier-3 fallback semantics exactly
   (`crates/khive-pack-memory/src/handlers/feedback.rs:55-60`).

### Binding semantics for the read path (Q3 resolved)

The projection applies **only when `matched_binding == true`**, exactly mirroring the feedback
write path tier-2 discipline (`crates/khive-pack-memory/src/handlers/feedback.rs:107-118`).
When `brain.serve_weights` returns `matched_binding: false` (system-default fallback —
`balanced-recall-v1` active with no explicit binding), the recall path applies nothing and keeps
pack defaults. This keeps the read and write paths consistent: both require an explicit binding
before brain posteriors influence behavior.

Rationale: symmetric behavior is easier to reason about and audit. The system-default
`balanced-recall-v1` posteriors are already reflected in the static default config weights
(Beta(7,3)=0.70 relevance, Beta(2,8)=0.20 salience, Beta(1,9)=0.10 temporal, confirmed at
`crates/khive-pack-memory/src/config.rs:362-388`), so applying them on a matched binding
adds incremental value over the hard-coded defaults. On a system-default non-binding, the
hard-coded defaults are already correct at prior values; divergence comes only after real events
update the posteriors, at which point an explicit binding is the right mechanism.

### New brain verb: `brain.serve_weights`

**Q2 resolved: adopt Option (ii), a purpose-built one-dispatch verb.**

Option (i) — extend `brain.profile` to expose `change_counter` plus calling `brain.resolve` first
— is two registry dispatches per cache miss: one for resolve, one for profile. Option (ii) is one
dispatch. At warm-up where every namespace triggers a cold cache lookup, halving the dispatch count
per namespace per restart is worth a modest surface addition.

Verb contract:

```
brain.serve_weights(namespace, consumer_kind) -> {
    matched_binding:  bool,
    profile_id:       String,
    change_counter:   u64,
    weights: {
        relevance:  f64,   // posterior mean of BalancedRecallState.relevance
        salience:   f64,   // posterior mean of BalancedRecallState.salience
        temporal:   f64,   // posterior mean of BalancedRecallState.temporal
    }
}
```

Implementation inside the brain pack handler:

1. Run `resolve_with_match(namespace, consumer_kind)` from `brain_state.rs` (the same function
   `brain.resolve` calls) to obtain `(profile_id, matched_binding)`.
2. Load the resolved profile's `BalancedRecallState` from `BrainState` (in-memory, under the
   existing `Mutex`).
3. Project via `BalancedRecallState::{relevance, salience, temporal}.mean()`.
4. Return the flat `{matched_binding, profile_id, change_counter, weights}` shape.

The verb is `Visibility::Subhandler` — it is an internal efficiency surface for pack-to-pack
coordination, not a user-facing verb. It does not appear in `brain.verbs()` output at `Verb`
visibility. This is consistent with the `brain.config`, `brain.state`, `brain.events`,
`brain.emit` subhandlers already present in ADR-032 §11.

### Where the `change_counter` lives and where it increments

- **Lives on**: a new `u64` field `change_counter` on `ProfileRecord`
  (`crates/khive-pack-brain/src/handlers.rs` — the same `BrainState`-backed record accessed
  throughout the feedback handler). Added alongside `exploration_epoch` on the in-memory profile
  state and on the `brain_profile_snapshots` persisted row via V5 migration.
- **Increments at**: the feedback apply site in the feedback handler
  (`crates/khive-pack-brain/src/handlers.rs:926-944`), immediately after `apply_signal` is called
  on the profile state. This is the only mutation path for posteriors; incrementing here provides
  exact invalidation semantics — the counter advances if and only if the posteriors actually
  changed.
- **Read by**: `brain.serve_weights`, which returns the current `change_counter` together with
  the projected weights in a single dispatch, letting the caller cache and invalidate precisely.

### Cache invalidation semantics

The cache entry `(namespace, profile_id, change_counter)` is valid as long as the brain's
`change_counter` for the resolved profile matches the cached counter. After a feedback event, the
brain's counter is one greater; the next recall for that `(namespace, profile_id)` detects the
mismatch, refreshes from the brain, and updates the cache. The cost of a refresh is one registry
dispatch (`brain.serve_weights`). After a `brain.reset`, `exploration_epoch` also increments,
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
| Staleness window                      | none (always fresh)                                        | at most one recall after `change_counter` advances                                                  | at most min(TTL, one recall after counter advance)                           |
| Complexity                            | lowest (no cache state)                                    | one cache map keyed on `(namespace, profile_id, change_counter)` + `u64` increment in feedback path | (b-amended) + TTL bookkeeping and a tuning knob                              |
| Correctness under concurrent feedback | exact                                                      | exact — counter advances on every feedback apply                                                    | bounded by TTL; may serve stale for up to the TTL window even after feedback |
| New brain surface                     | `brain.serve_weights` (one-dispatch resolve+project)       | same                                                                                                | same                                                                         |

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

### Amendment (2026-07-03): typed consumer-kind vocabulary

Issue #542 (closed-issue audit consolidation pass) surfaced that `consumer_kind` is a bare,
unvalidated `String` at every call site — `ProfileRecord`/`BindingRecord`
(`crates/khive-brain-core/src/profile.rs:29,61`) type it as `String`, and every caller hand-types
the literal, including this ADR's own §Exact change list #3 sample (`"consumer_kind": "recall"`
above). Three values exist across the ADR corpus today, at three different states of wiring:

| Value                 | Declared in                                                                                                          | Status                                                                                                                  |
| --------------------- | -------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `"recall"`            | this ADR / [ADR-035](ADR-035-cli-config-and-auto-embed.md)                                                           | Live — the only consumer_kind actually wired to a resolver call site                                                    |
| `"knowledge_compose"` | [ADR-048](ADR-048-knowledge-section-profiles.md) §"New brain verb" (line 396)                                        | **Gap** — declared but never wired; the knowledge pack's feedback handler still resolves against `"recall"` (see below) |
| `"rerank"`            | [ADR-032](ADR-032-brain-profile-orchestration.md) §934, [ADR-042](ADR-042-local-rerank-via-lattice-inference.md) §71 | Deferred — no resolver call site exists yet                                                                             |

The knowledge pack's mis-binding is a real bug, not just a naming gap:
`crates/khive-pack-knowledge/src/handlers.rs:395-400` and `:459` call `brain.resolve` with the
literal `"recall"`, meaning knowledge-pack feedback today updates the SAME posterior bucket as
memory-pack recall feedback instead of the independently-tunable bucket ADR-048 specified. The
inline comment at handlers.rs:397 documents this as if deliberate ("brain contract registers
recall bindings under consumer_kind=\"recall\"") — that comment is an artifact of the gap, not
evidence it was intentional at the ADR-048 level.

**Decision**: introduce a closed `ConsumerKind` enum in `crates/khive-brain-core/src/profile.rs`
(the crate both `khive-pack-memory` and `khive-pack-knowledge` already depend on directly, and
which already depends on `khive-runtime` — no new dependency edge), mirroring the closed-vocabulary
discipline this codebase already applies to entity kinds and edge relations:

```rust
/// Closed vocabulary of brain profile consumers. Adding a new consumer requires
/// adding a variant here — never a bare string literal at a new call site.
pub enum ConsumerKind {
    Recall,           // "recall" — memory pack recall ranking (live)
    KnowledgeCompose, // "knowledge_compose" — knowledge pack compose ranking (ADR-048, not yet wired)
    Rerank,           // "rerank" — lattice-inference rerank profile (ADR-032/042, deferred)
}
```

with `as_str()` (and `FromStr` where a caller needs to parse the wire-level string back) — the MCP
`consumer_kind` param stays a JSON string at the wire boundary; the enum is the Rust-side internal
vocabulary, not a wire schema change. The #542 implementation PR MUST: (a) replace every existing
bare `"recall"` literal — this ADR's own §3 sample included — with `ConsumerKind::Recall.as_str()`,
and (b) correct `khive-pack-knowledge`'s handlers.rs:400/459 call sites from `"recall"` to
`ConsumerKind::KnowledgeCompose.as_str()`, closing the ADR-048 gap.

This does **not** expand Q4's scope below. Q4 (open, deferred to a follow-up ADR) asks whether
`knowledge.compose` ranking should CONSUME posteriors at read time. This amendment only fixes which
bucket the knowledge pack's EXISTING feedback write path resolves against — a correctness fix
available now, independent of whether Q4's read-path wiring ever lands.

### Amendment (2026-07-03): feedback-path resolver consolidation

`crates/khive-pack-memory/src/handlers/feedback.rs:43-` and
`crates/khive-pack-knowledge/src/handlers.rs:375-` each implement their own tier-1/tier-2/tier-3
profile resolution ladder (explicit config profile → namespace-bound `brain.resolve` lookup gated
on `matched_binding` → pack-local fallback), and each pack additionally hand-rolls its own private
helper to perform tier 2 — `resolve_namespace_profile` (`feedback.rs:113-`) and
`knowledge_resolve_namespace_profile` (`handlers.rs:523-`). The two implementations are
structurally identical (same `brain.resolve` dispatch, same `matched_binding` gate, same
`Option<String>` return), maintained as independent copies with no shared source.

**Decision**: the #542 implementation PR MUST extract one shared resolver into
`khive-brain-core` (same crate as the new `ConsumerKind` enum, same dependency justification):

```rust
pub async fn resolve_consumer_profile(
    registry: &khive_runtime::VerbRegistry,
    namespace: &str,
    consumer_kind: ConsumerKind,
) -> Option<String>
```

and both packs' tier-2 call sites MUST call this shared function instead of maintaining private
copies. This is a MUST, not a nice-to-have: two independently-maintained copies of the same
matched_binding-gated resolution logic is exactly the kind of silent-divergence risk this ADR's
read path exists to eliminate at the ranking layer — the same discipline applies to the write-path
resolver underneath it.

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

- `brain.serve_weights` (new `Visibility::Subhandler` verb) must be added to the brain pack and
  validated — a small, internal-only surface expansion on the brain pack, governed by
  [ADR-032](ADR-032-brain-profile-orchestration.md).
- A `change_counter` field must be added to the brain profile record and incremented in the
  feedback apply path. This requires V5 DB migration for the `brain_profile_snapshots` table
  (a new `change_counter INTEGER NOT NULL DEFAULT 0` column, additive — see §Change list).
- The memory pack gains mutable cache state (`(namespace, profile_id, change_counter) → projected
  weights`), with the usual lock and eviction considerations. It must be bounded (e.g. LRU with
  a cap on tracked `(namespace, profile_id)` pairs; 64 entries is a safe ceiling for v1 — no
  deployment today has more than a handful of concurrently-bound profiles).
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
- The weights cache is in-memory and rebuilt on restart, matching the existing `recall_state`
  posture (`crates/khive-pack-memory/src/pack.rs:32` — "Persistence is deferred — state is rebuilt
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

**(e) Extend `brain.profile` + separate `brain.resolve` call (Option i for Q2).** Two dispatches
per cache miss: `brain.resolve(namespace, consumer_kind)` then `brain.profile(profile_id)`. Rejected
in favor of `brain.serve_weights` for the one-dispatch advantage. Cold-start (every namespace
triggers a cache miss on the first recall after restart) means the round-trip count is O(active
namespaces), making the per-cache-miss cost matter at warm-up. The one-dispatch verb also
co-locates resolve and projection inside the brain pack where the `BrainState` Mutex is already
held, avoiding two separate lock acquisitions.

---

## Exact change list

### 1. `crates/khive-pack-brain` — feedback handler and new verb

**File: `crates/khive-pack-brain/src/handlers.rs`**

- In `ProfileRecord` (or the equivalent `BrainState` profile record struct): add `change_counter:
  u64` alongside `exploration_epoch`.
- In `handle_feedback` at the feedback apply site (~line 927): immediately after
  `state.balanced_recall.apply_signal(&signal)` (and the equivalent branch for user-created
  profiles at ~line 934), increment the profile's `change_counter` by 1.
  ```
  // After apply_signal — exact invalidation for the weights cache
  if serving_profile == "balanced-recall-v1" {
      state.balanced_recall.change_counter += 1;
  } else if let Some(ps) = state.profile_states.get_mut(serving_profile) {
      ps.change_counter += 1;
  }
  ```
- Add `handle_serve_weights` method: resolve + project weights + return
  `{matched_binding, profile_id, change_counter, weights: {relevance, salience, temporal}}`.
- Register `brain.serve_weights` in the `PackRuntime::dispatch` match arm and in `BRAIN_HANDLERS`
  with `Visibility::Subhandler`.

**File: `crates/khive-brain-core/src/profile.rs`**

- Add `change_counter: u64` to `BalancedRecallState` (alongside `exploration_epoch`).
- Initialize to `0` in `BalancedRecallState::new`.
- Include in snapshot serialization (it is a `u64`, serde-compatible, additive field).

**File: `crates/khive-pack-brain/src/persist.rs`**

- `BrainStateSnapshot` (the serde struct serialized to `snapshot_json`): add
  `change_counter: u64` with `#[serde(default)]`. Existing snapshots deserialize with
  `change_counter = 0` (correct — signals stale cache on first recall after migration).

### 2. `crates/khive-db` — V5 migration

**New file: `crates/khive-db/sql/005-brain-change-counter.sql`**

```sql
-- V5: Add change_counter column to brain_profile_snapshots for recall-weights cache
-- invalidation. DEFAULT 0 ensures existing rows start as "always stale on next recall",
-- which is safe — triggers one weights-cache miss per profile, then caches normally.
ALTER TABLE brain_profile_snapshots ADD COLUMN change_counter INTEGER NOT NULL DEFAULT 0;
```

**File: `crates/khive-db/src/migrations.rs`**

- Add `const V5_UP: &str = include_str!("../sql/005-brain-change-counter.sql");`
- Add `VersionedMigration { version: 5, name: "brain_change_counter", up: V5_UP }` to the
  `MIGRATIONS` constant after the existing V4 entry.

### 3. `crates/khive-pack-memory` — weights cache and recall injection

**File: `crates/khive-pack-memory/src/pack.rs`**

- Add `weights_cache: Mutex<WeightsCacheMap>` field to `MemoryPack`, where:
  ```rust
  /// Cached brain profile weights. Key: (namespace, profile_id, change_counter).
  /// Value: projected (relevance, salience, temporal) weights.
  /// Bounded LRU: evicts oldest (namespace, profile_id) entry when capacity (64) is reached.
  type WeightsCacheMap = lru::LruCache<(String, String), (u64, f64, f64, f64)>;
  //                                   ^namespace ^profile_id  ^cc  ^rel ^sal ^tem
  ```
  This requires adding `lru` to `crates/khive-pack-memory/Cargo.toml` (already a transitive
  dep of `khive-runtime` via `khive-db`; verify with `cargo tree -p khive-pack-memory | grep lru`
  before adding a new dep).
- Initialize in `MemoryPack::new`: `weights_cache: Mutex::new(LruCache::new(NonZeroUsize::new(64).unwrap()))`.

**File: `crates/khive-pack-memory/src/handlers/recall.rs`**

- Before `cfg.validate()` at ~line 89, insert the profile-weights injection:
  ```rust
  // Inject brain profile weights if a binding is resolved for this caller.
  // Fails silently (brain pack absent or binding not found) — recall continues
  // with pack defaults. See ADR-058.
  if let Ok(weights) = self.resolve_brain_weights(token, registry).await {
      if let Some((rel, sal, tem)) = weights {
          cfg.relevance_weight = rel;
          cfg.salience_weight = sal;
          cfg.temporal_weight = tem;
      }
  }
  ```

**New method in `MemoryPack` (e.g., `crates/khive-pack-memory/src/handlers/recall.rs` or
extracted to a new `brain_weights.rs` module if the file approaches its 700-LOC limit):**

```rust
/// Resolve and cache brain profile weights for the current caller.
///
/// Returns Ok(Some((rel, sal, tem))) when a binding is found and weights are loaded.
/// Returns Ok(None) when brain is absent, no binding matches (matched_binding=false),
/// or the brain pack returns an error.
/// Never propagates brain errors — recall must not fail if brain is unavailable.
async fn resolve_brain_weights(
    &self,
    token: &NamespaceToken,
    registry: &VerbRegistry,
) -> Result<Option<(f64, f64, f64)>, RuntimeError> {
    let ns = token.namespace().as_str().to_string();

    // Cache lookup: check if we have a valid (namespace, profile_id) entry
    // whose change_counter matches what brain will return.
    // We don't know the profile_id yet, so we must dispatch first on cold miss.
    // On a warm hit, the entry is keyed by (ns, profile_id) from the prior call.
    // Strategy: peek for any entry for this namespace; if found and we need to
    // verify counter, the dispatch will confirm. For v1 simplicity, always dispatch
    // brain.serve_weights (one call) and use the returned change_counter to decide
    // whether to return the cached projection or update it.
    //
    // The dispatch is cheap (in-process registry, no network) and is only one call
    // regardless of cache state — it returns matched_binding + counter + weights
    // in one round-trip.

    let serve_params = serde_json::json!({
        "namespace": &ns,
        "consumer_kind": khive_brain_core::ConsumerKind::Recall.as_str(),
    });

    let result = registry
        .dispatch("brain.serve_weights", serve_params)
        .await;

    let v = match result {
        Err(_) => return Ok(None), // brain pack absent or error — use defaults
        Ok(v) => v,
    };

    let matched = v.get("matched_binding").and_then(|b| b.as_bool()).unwrap_or(false);
    if !matched {
        return Ok(None); // system-default only — use pack defaults (symmetric with write path)
    }

    let profile_id = match v.get("profile_id").and_then(|s| s.as_str()) {
        Some(id) => id.to_owned(),
        None => return Ok(None),
    };
    let change_counter = v.get("change_counter").and_then(|n| n.as_u64()).unwrap_or(0);

    // Check cache: if (ns, profile_id) is present and counter matches, use cached weights.
    {
        let mut cache = self.weights_cache.lock().unwrap();
        if let Some(&(cached_cc, rel, sal, tem)) = cache.get(&(ns.clone(), profile_id.clone())) {
            if cached_cc == change_counter {
                return Ok(Some((rel, sal, tem)));
            }
        }
    }

    // Cache miss or counter mismatch — extract weights from the serve_weights response.
    let weights = v.get("weights");
    let rel = weights.and_then(|w| w.get("relevance")).and_then(|v| v.as_f64()).unwrap_or(0.70);
    let sal = weights.and_then(|w| w.get("salience")).and_then(|v| v.as_f64()).unwrap_or(0.20);
    let tem = weights.and_then(|w| w.get("temporal")).and_then(|v| v.as_f64()).unwrap_or(0.10);

    // Update cache.
    {
        let mut cache = self.weights_cache.lock().unwrap();
        cache.put((ns, profile_id), (change_counter, rel, sal, tem));
    }

    Ok(Some((rel, sal, tem)))
}
```

Note: `brain.serve_weights` is called on **every** recall (not only on cold miss), but it is an
in-process registry dispatch with no I/O — the brain handler reads from its in-memory `BrainState`
under a brief `Mutex` lock. If profiling shows this is a measurable hot-path cost, the optimization
path is to store the profile_id from the prior call in the cache and check the counter without
dispatching; but at v1 scale, the dispatch cost is negligible compared to ANN search.

### 4. Pack decoupling invariant

`crates/khive-pack-memory/Cargo.toml` must **not** gain a direct dependency on
`khive-pack-brain`. The weights injection uses only `VerbRegistry::dispatch("brain.serve_weights",
...)` — the same indirection already used for `brain.resolve` and `brain.feedback` in the feedback
handler. If the brain pack is not loaded, `dispatch` returns an error, which `resolve_brain_weights`
converts to `Ok(None)`. Pack coupling via the registry is O(1) and already the established pattern.

### 5. ADR-032 amendment note

[ADR-032](ADR-032-brain-profile-orchestration.md) §11 lists the brain verb surface. An implementer
must add `brain.serve_weights` to that table with:

- Verb: `brain.serve_weights`
- Speech act: assertive
- Visibility: Subhandler
- Purpose: resolve the caller's recall profile and return projected weights with `change_counter`
  for the memory pack's weights cache.

### 6. Consumer-kind vocabulary and resolver consolidation (amendment, 2026-07-03)

**File: `crates/khive-brain-core/src/profile.rs`**

- Add the `ConsumerKind` enum (see amendment above) with `Recall` / `KnowledgeCompose` / `Rerank`
  variants and `as_str(&self) -> &'static str`.
- Add `resolve_consumer_profile(registry, namespace, consumer_kind) -> Option<String>` as the
  single shared implementation of the tier-2 namespace-bound-profile lookup, replacing the bodies
  of `resolve_namespace_profile` (`khive-pack-memory/src/handlers/feedback.rs:113-`) and
  `knowledge_resolve_namespace_profile` (`khive-pack-knowledge/src/handlers.rs:523-`).

**File: `crates/khive-pack-memory/src/handlers/feedback.rs`**

- Line 52: call `khive_brain_core::resolve_consumer_profile(registry, &ns, ConsumerKind::Recall)`
  in place of the private `resolve_namespace_profile` helper; delete the now-dead private fn.
- §Exact change list #3's `resolve_brain_weights` sample (line 529 above): use
  `ConsumerKind::Recall.as_str()` in place of the bare `"recall"` literal.

**File: `crates/khive-pack-knowledge/src/handlers.rs`**

- Lines 400 and 459: call the shared resolver with `ConsumerKind::KnowledgeCompose`, not
  `ConsumerKind::Recall` — this is the ADR-048 gap fix, not a rename. Delete the private
  `knowledge_resolve_namespace_profile` (line 523-).

This item is the implementer checklist for the (separate) #542 implementation PR — no `.rs` files
change in this spec-delta PR itself.

---

## Determinism and test plan

### Scoring determinism preserved

The three weights (`relevance_weight`, `salience_weight`, `temporal_weight`) flow into
`compute_score` (`crates/khive-pack-memory/src/handlers/common.rs:227-251`) as `f64` values.
`compute_score` returns a tuple that passes through `khive_score::DeterministicScore` conversion
before ranking. The weights themselves are Beta posterior means (rational f64 arithmetic on the
same input data), which are platform-deterministic given the same event sequence. No new
non-determinism is introduced: the injection point at `recall.rs:73` overwrites three f64 fields
in `cfg` before any ANN or scoring call.

### Test cases required for implementation

1. **Read-path wired test.** With brain pack loaded, a profile bound for `consumer_kind="recall"`,
   and a known posterior state (set via `brain.feedback` calls), verify that `memory.recall`
   ranking reflects the projected weights — e.g. after setting temporal to a low posterior mean,
   confirm that a recently-created memory scores lower relative to an older memory with equal
   relevance than it would under the static default temporal weight.

2. **Fallback test: brain absent.** With only `kg` + `memory` packs loaded (no brain), verify
   that `memory.recall` succeeds and uses pack defaults. This confirms `resolve_brain_weights`
   errors silently rather than blocking recall.

3. **Fallback test: no binding (`matched_binding=false`).** With brain loaded but no explicit
   binding created, verify recall uses pack defaults. This confirms the `matched_binding` gate
   in `resolve_brain_weights` is respected.

4. **Cache hit test.** After a recall that primes the weights cache, emit a `brain.feedback`
   that does NOT advance `change_counter` for the resolved profile (e.g. a no-op or a feedback
   to a different profile), then recall again — verify `brain.serve_weights` is called once
   total (the cache_miss path on first recall) and the weights match the prior call.
   (Implementation note: this test requires a test hook or observable counter on the cache.)

5. **Cache invalidation test.** After a recall that primes the cache, emit `brain.feedback` to the
   bound profile (which increments `change_counter`), then recall again — verify the new weights
   are fetched and cached under the incremented counter.

6. **Same query + same profile = same ranking (determinism).** With a fixed corpus, fixed profile
   state, and fixed `change_counter`, running `memory.recall(query=X)` twice must return
   identical results in identical order. This is already guaranteed by the deterministic scoring
   invariant (ADR-006 / ADR-032 §9), but the test pins it explicitly as a regression guard for
   this change.

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

### Q2 — RESOLVED: new brain read verb contract

**Resolved: adopt `brain.serve_weights` (Option ii), a purpose-built one-dispatch verb** that
performs resolve + project inside the brain pack and returns
`{matched_binding, profile_id, change_counter, weights: {relevance, salience, temporal}}`.
Contract and implementation details are in §New brain verb above.

Rationale: one dispatch per cache miss vs two (resolve + profile fetch). At cold-start, the
difference is O(active namespaces) dispatches; at warm steady-state it is zero (O(1) cache hit
with no dispatch).

### Q3 — RESOLVED: binding semantics for the read path

**Resolved: projection applies only on `matched_binding=true`**, mirroring the feedback write
path tier-2 discipline. System-default fallbacks (`matched_binding=false`) use pack defaults.
See §Binding semantics above for rationale.

### Q4 — Open: knowledge pack read-path scope

**This gap was flagged independently during review.** The `knowledge.compose` verb also lacks read-path
wiring: it routes feedback through `brain.resolve(consumer_kind="recall")`
(`crates/khive-pack-knowledge/src/handlers.rs:362-403`) — **note: as of the 2026-07-03 amendment
above, this is a known gap, not a correct baseline; it should resolve against
`consumer_kind="knowledge_compose"` per ADR-048, and the #542 implementation PR fixes it** — but
does not consume posteriors when ranking compose output, which remains this question's actual
scope. This ADR scopes the memory-pack path. Should the knowledge pack's read-path ranking be
brought to the same spec in a follow-up ADR, or in this one?

**Recommendation**: follow-up ADR. The knowledge pack's recall path is structurally different
(compose ranking over atoms vs. note retrieval); a separate ADR can reference ADR-058 as the
canonical pattern and adapt as needed.

### Q5 — Open: `BrainProfileHint` disposition

**Delete or wire?** The `BrainProfileHint` struct (`crates/khive-pack-memory/src/config.rs:90-102`)
specifies a post-recall score-boost mechanism distinct from the weight-projection read path. It is
currently dead (`brain_profile` is never read in `recall.rs`). Options: delete it as dead config
per the no-backwards-compat-shims standard, or specify the boost use case and wire it separately.
**Recommendation**: delete in the implementation PR unless a boost use case is articulated. The
weight-projection read path (this ADR) subsumes the motivating intent; a per-entity boost on
top requires a separate evidence base.
