# ADR-104: Profile Posteriors in the Recall Read Path

**Status**: Accepted (signed 2026-07-08, with riders R1 and R2 below)
**Date**: 2026-07-08
**Depends on**: ADR-021 (memory pack), ADR-081 (serve ledger + profile stamping), ADR-033
(recall scoring), issue #159 (brain-tunable parameter surface)

## Context

The brain pack accumulates feedback into per-profile Beta posteriors, and the memory pack
was built to be tuned by them. Today the loop is half-open:

- **The write side works.** Every `memory.recall` updates `BalancedRecallState` — the
  `relevance` and `temporal` posteriors, plus a per-entity posterior keyed by the top
  result's UUID (`recall_feedback.rs`). Explicit feedback updates the `salience` posterior
  and the target's per-entity posterior. ADR-081 stamps `served_by_profile_id` on every
  recall response and writes a serve-ledger row.
- **The projection machinery exists but has no caller.** `MemoryPack` implements
  `PackTunable` (issue #159): `project_config` maps the three posterior means into
  `RecallConfig { relevance_weight, salience_weight, temporal_weight }`, and
  `apply_config` validates and swaps the active config. A workspace-wide search finds no
  production caller of either method. `brain.activate`'s verb description says it starts a
  "live update loop"; the handler flips `ProfileLifecycle::Active` and nothing more. No
  loop exists.
- **Per-entity posteriors never reach scoring at all.** `calculate_score` reads salience,
  decay, relevance, and the caller's `entity_names`; the per-entity Beta posteriors —
  the signal that would let one piece of explicit feedback visibly lift one specific
  memory — are not an input, and were not an input even in the #159 projection design,
  which only tunes the three global weights.

The consequence: feedback accumulates, profiles resolve and stamp correctly, and none of
it changes what a recall returns. Two profiles with arbitrarily different posterior state
serve identical rankings. There is no way to demonstrate — or measure — that the posterior
machinery earns its keep.

A second, related gap surfaced in the same evaluation cycle: the `entity_names` boost
(the `EntityMatch` ×1.3 adjustment) now auto-derives candidates from capitalized query
tokens (#738), but all-lowercase queries — the common shape for agent-generated recall —
and unsegmented CJK queries extract nothing. Free-text extraction cannot go further
without degenerating into a lexical-overlap reward (the measured failure that rejected
the first cut of #738). Anchoring candidates against records that actually exist in the
knowledge graph is the precision-safe extension.

## Decision

Posteriors enter the read path **per-request, at serve time**. No global config mutation,
no background loop. Five components:

### 1. Serve-time projection of profile weights

`memory.recall` already resolves the serving profile (ADR-081, `resolve_serving_profile`).
After resolution, the handler loads that profile's `BalancedRecallState` and derives this
request's scoring weights through the existing `PackTunable::project_config` path — used
as a pure function from posterior state to weights, not as a mutation of the pack's
active config. Requests with no resolvable profile score with the configured defaults,
exactly as today.

Properties this buys:

- **Deterministic and testable.** Same store, same query, same profile state → same
  ranking. No cross-request interference, no ordering dependence on when a loop last ran.
- **Profile-differentiated serving.** Two profiles with different posterior state now
  produce different rankings for the same query — observable, comparable, explainable.
- **No new lifecycle machinery.** `apply_config` and any notion of a live update loop
  stay unused. The `brain.activate` verb description is corrected to describe what the
  handler does (lifecycle transition); serving reads state per-request, which makes a
  push loop unnecessary by construction.

### 2. Bounded per-entity posterior term

After the existing `rank_score` computation, one additional multiplicative term:

```text
rank_score *= clamp(1 + w_ent * (entity_posterior_mean - 0.5), 0.85, 1.15)
```

- `entity_posterior_mean` is the mean of the serving profile's per-entity Beta posterior
  for the candidate memory's UUID.
- The term is neutral (exactly 1.0) when the profile holds no posterior for the candidate
  beyond the uninformative prior — memories nobody has given feedback on are unaffected.
- `w_ent` defaults to 0.3, making the clamp bounds reachable at posterior means of 0.0
  and 1.0; the clamp guarantees the term can never move a score by more than ±15%,
  preserving relevance dominance by design.

This is the component that closes the visible loop: one `useful` signal on a recalled
memory measurably lifts that memory's rank on the next equivalent query, under that
profile, and nowhere else.

### 3. Score breakdown exposure

`include_breakdown=true` responses gain two fields per hit:

- `profile_component`: the multiplicative contribution of serve-time projection relative
  to default weights (1.0 when no profile served the request).
- `entity_posterior_mean`: the posterior mean used by component 2 (absent when no
  posterior exists for the hit).

Read models and viewers render these directly; this is how the posterior effect becomes
inspectable rather than inferred.

### 4. `profile_id` override on `memory.recall`

An optional `profile_id` request parameter short-circuits binding resolution: the named
profile's state serves the request (and is stamped as `served_by_profile_id`). Evaluation
tooling can run the same query under different profiles and diff the orderings. The
override participates in the serve ledger identically to resolved profiles; it is a
resolution override, not a ledger bypass.

### 5. Entity-anchored candidate extraction

Extends #738's capitalized-token extraction with a second, precision-safe source: query
tokens (and adjacent-token bigrams) that case-insensitively equal the name of an existing
KG entity become entity candidates regardless of capitalization. Matching against real
entity records is what makes lowercase coverage safe — a token can only earn the boost by
naming something the graph actually knows — and it serves unsegmented CJK queries through
substring lookup against entity names rather than whitespace tokenization.

Implementation constraint: one indexed lookup per recall (batched over tokens), reusing
the entity store's existing name index. Explicit caller-supplied `entity_names` continues
to win over all extraction, and `entity_names: []` remains a full opt-out (#738
semantics unchanged).

## What this deliberately does not do

- **No global weight mutation.** `apply_config` remains callerless in production; if a
  future consumer wants persistent tuned defaults, that is a separate decision with its
  own rollback story.
- **No background loop.** Per-request projection makes freshness structural. The
  `brain.activate` description is corrected rather than implemented-toward.
- **No cross-profile blending.** A request is served by exactly one profile's state
  (resolved or overridden), or by defaults.
- **No new storage.** Every input already exists: posterior state (brain snapshots),
  profile resolution (ADR-081), entity names (kg store).

## Staged landing

| Stage | Contents                                                                     | Gate                                                                                        |
| ----- | ---------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| A     | Serve-time projection (1) + `profile_id` override (4) + breakdown fields (3) | Unit tests proving profile-differentiated ranking; eval-set A/B run demonstrating the diff  |
| B     | Per-entity posterior term (2)                                                | Feedback-lift test: one explicit `useful` signal changes next-query rank under that profile |
| C     | Entity-anchored extraction (5)                                               | Eval rows for lowercase and CJK queries move as predicted; no regression on #738 tests      |

Stages land as separate PRs in order; A carries the request/response shape changes, so
B and C are additive behind already-shipped surface.

Two riders bind the stage gates (sign-off conditions, 2026-07-08):

- **R1 (Stage C)**: the Stage C PR must document the lowercase/CJK lookup strategy and an
  explicit per-recall cost bound for entity anchoring — one batched indexed lookup, no
  unbounded per-recall scans of the entity table.
- **R2 (Stage A)**: Stage A ships a **measured** per-recall overhead number (recall with
  vs without the profile-state read) recorded in the PR; Stage B does not land until that
  number is in the record.

## Consequences

### Positive

- The feedback flywheel closes: signals change serving, visibly and per-profile, and the
  effect is inspectable per hit via the breakdown fields.
- Evaluation becomes profile-aware: the same eval set runs under any profile via the
  override, turning posterior state into something measurable rather than latent.
- Bounded influence by construction: projection only reweights existing factors, and the
  per-entity term is clamped to ±15%, so relevance remains the dominant signal and a
  poisoned or skewed posterior cannot invert a ranking.
- The `brain.activate` description finally matches its behavior.

### Negative

- Per-request projection adds a profile-state read to every recall that resolves a
  profile. The state is small and already cached for stamping; measured overhead is
  expected to be negligible, but Stage A must confirm this before B lands on top.
- Per-entity posteriors are keyed to memory UUIDs; superseded or merged memories carry
  their posterior history with them only if curation preserves the UUID. Merge rewires
  edges but does not merge posterior state — accepted for now, recorded below.

## Open questions

1. **Posterior state merge on entity merge.** When `merge` deduplicates two entities (or
   a future curation pass merges memories), per-entity posterior state for the removed
   UUID is orphaned rather than folded into the kept record. Acceptable at current
   volumes; revisit if feedback density grows.
2. **ESS floor for the per-entity term.** Resolved at sign-off (2026-07-08): **no ESS
   floor at Stage B.** Component 2 treats any posterior beyond the uninformative prior as
   informative; single-signal responsiveness is the point of the feedback-lift gate. If
   eval noise later proves visible, a minimum-evidence floor is a one-line adjustment —
   revisit only with eval evidence.
3. **Bigram window for entity anchoring.** Component 5 starts with unigrams and
   adjacent-token bigrams. Longer entity names (3+ tokens) fall back to capitalized
   extraction or explicit `entity_names`; extending the window is a measured decision.

## Amendment 1 (2026-07-12): Prior-preserving evidence decay for per-entity posteriors

**Status**: Proposed (amendment to the accepted ADR; Stage D in the staged landing)

### Problem

The per-entity Beta posteriors that feed component 2 accumulate evidence forever. A
memory judged `useful` three months ago carries the same weight as one judged `useful`
this morning, and a memory judged `wrong` once can sit at the 0.85 clamp floor
indefinitely with no path back to neutral unless someone happens to re-serve and
re-judge it. As feedback density grows, stale evidence dominates fresh evidence, and
the term drifts from "what this profile currently finds useful" toward "what this
profile ever found useful."

A second latent defect shares the fix: the stored `(alpha, beta)` pair mixes the
uninformative prior into the evidence counts. Any future operation that re-applies a
prior to stored state (a reset that re-seeds, a merge that adds two posteriors, a
projection that adds priors before computing a mean) double-counts it. Production
evidence from recommender-system deployments converges on both points: decay old
evidence rather than accumulate forever, and keep the prior decomposed from observed
evidence so it is applied exactly once, at read time.

### Decision

Per-entity posterior state decomposes into **evidence-only counts plus a fixed prior
applied at read time**, with **exponential time decay of the evidence**:

```text
stored:   (alpha_ev, beta_ev, last_event_at)        # evidence mass only, both start at 0
decayed:  g = 2^(-dt_days / H)                       # dt in days, see clock rules below
          alpha' = g * alpha_ev,  beta' = g * beta_ev
mean:     (1 + alpha') / (2 + alpha' + beta')        # Beta(1,1) prior enters here, once
```

- **Evidence is judgment-bearing feedback only.** Per-entity evidence accrues from
  feedback signals that carry a caller's judgment (explicit positive/negative,
  implicit positive/negative, correction, and the useful/not_useful/wrong ladder),
  each at its configured signal weight. Automatic exposure events (`RecallHit`,
  `NoteAccessed`) are excluded from per-entity evidence: they record that an item was
  served or opened, not that anyone judged it useful, and routing them into the term
  that reorders the next serve creates a positional feedback loop. At one auto-hit per
  day with the default half-life, replenished positive evidence reaches a steady state
  near `1 / (1 - 2^(-1/30)) = 43.8`, a decayed mean near 0.978, and a persistent
  multiplier near 1.143, enough to durably order candidates on exposure alone.
  Exposure events continue to feed the global posteriors exactly as shipped; only the
  per-entity accrual rule changes. This amends the accepted ADR's Stage B accrual
  behavior as part of Stage D.
- **Evidence counts are mass, not event counts.** Each signal adds its weight
  (correction 2.0, explicit 1.5, implicit 0.1 under the existing fold-gate clamp on
  cumulative implicit mass), so `B` throughout this amendment denotes accrued evidence
  mass. A single correction is `B = 2.0`, not `B = 1`.
- **Write path**: decay the stored counts by the interval elapsed since
  `last_event_at`, then add the new observation's weight, then stamp `last_event_at`.
  The prior is never part of what decays or what is written.
- **Event-time clock, not wall clock, on the write path.** The accepted ADR's replay
  contract (ADR-032) requires that replaying the same event history yields identical
  state regardless of when the replay runs. Every write-side interval and every
  `last_event_at` stamp therefore uses the event's own persisted occurrence time
  (`Event.created_at`), carried into the profile reducer alongside the signal. The
  reducer never reads a wall clock. Backward-clock policy: `dt = max(event_time -
  last_event_at, 0)` days, so an out-of-order or regressed timestamp decays nothing
  and never grows evidence; it is not an error.
- **Read path** (component 2's `entity_posterior_mean`): compute the decayed mean as a
  pure function of stored state and an injected current time. No writes on read:
  serve-time projection stays deterministic and side-effect free, exactly as the
  accepted ADR requires.
- **Half-life `H`**: runtime configuration `entity_evidence_half_life_days`, default
  30 days, one value per running instance (the same configuration layer, and the same
  validation posture, as the existing recall half-life: finite and strictly positive,
  rejected at the configuration boundary otherwise). It is not per-profile or
  per-request state, so identical stored posteriors always project identically within
  a deployment.
- **Decay is the recovery mechanism, on a magnitude-dependent clock.** Because the
  decayed mean converges to the prior mean 0.5, the component-2 multiplier converges to
  exactly 1.0 (neutral). For a purely negative posterior with accumulated evidence mass
  `B`, the decayed mean is `1 / (2 + g * B)` with `g = 2^(-dt / H)`, so the time to
  return within a tolerance of neutral grows with `log2(B)`: unit evidence mass
  (`B = 1`, and anything below about 1.23) is within 0.02 of neutral (in multiplier
  terms) after two half-lives, while a single explicit judgment (`B = 1.5`), a single
  correction (`B = 2.0`), and any larger accumulated mass are not, and reach the 0.02
  band by `H * (log2(B) + 2.2)` uniformly in `B` (at that elapsed time
  `g * B = 2^(-2.2)` regardless of `B`). This is
  intended behavior: heavily-confirmed judgments decay on a proportionally longer
  clock, and no memory is permanently suppressed, but recovery time is conditional on
  evidence magnitude and signal weight, never a flat two-half-life guarantee.

### Snapshot migration

`entity_posteriors_version` advances to 2: entries become
`(uuid, alpha_ev, beta_ev, last_event_at)`. Version-1 snapshots load by subtracting
the uninformative prior from the stored pair (`alpha_ev = max(alpha - 1, 0)`,
`beta_ev = max(beta - 1, 0)`) and stamping `last_event_at` at load time, so
grandfathered evidence starts its decay clock at migration rather than being
retroactively expired. Version-1 snapshots are never written again after a version-2
load.

### What this amendment deliberately rejects

- **Pessimistic cold-start priors** (e.g. Beta(1, 99), shipped by feed recommenders):
  correct where candidates compete for exposure and an unproven item must earn its
  slot, wrong here. Component 2 is neutral at no evidence by design; a pessimistic
  prior would penalize every unjudged memory by up to 15% and invert the accepted
  ADR's relevance-dominance property. Rejected with intent, not overlooked.
- **A separate coverage/exploration route** (serving low-evidence items via an explicit
  known-propensity arm to prevent cold-start starvation): rejected, conditional on the
  exposure-event exclusion above. With `RecallHit` and `NoteAccessed` excluded from
  per-entity evidence, evidence enters only when a caller judges a result, so a
  repeatedly served item gains no automatic advantage and the rich-get-richer loop
  through the posterior term cannot close; ranking stays relevance-dominant, the term
  is clamped to plus or minus 15%, and decay-to-neutral restores any suppressed item
  without requiring it to be re-served first. Two conditions void this rejection and
  force the decision to be retaken: reintroducing any automatic exposure signal into
  per-entity evidence, or making the posterior term exposure-controlling (a larger
  `w_ent`, or posterior-driven candidate selection).
- **Decay of the three global posteriors** (`relevance`, `salience`, `temporal`): out
  of scope. They receive signal on effectively every recall, so staleness self-corrects
  at event rate; the per-entity posteriors are the sparse, staleness-prone state.

### Stage D gate

| Stage | Contents                                              | Gate                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| ----- | ----------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| D     | Evidence/prior decomposition + lazy exponential decay | Unit: decayed mean converges to 0.5 over elapsed time; update-after-gap decays before adding; v1 snapshot migration round-trips; read path provably write-free; a `RecallHit` and a `NoteAccessed` leave per-entity evidence unchanged; configuration rejects zero, negative, and non-finite `H`; a write whose event time precedes `last_event_at` neither decays nor grows evidence. Replay determinism: a snapshot-plus-event-history replay executed at two different wall-clock times yields byte-identical stored per-entity state (event-time clock, ADR-032). Behavior (clock-injected, magnitudes pinned in evidence-mass units): a `B = 1` negative posterior's component-2 term returns within 0.02 of neutral after two configured half-lives; a single-correction `B = 2.0` posterior is provably NOT within 0.02 at two half-lives; a `B >= 8` posterior is NOT within 0.02 at two half-lives but is within 0.02 by `H * (log2(B) + 2.2)`; the mean-to-multiplier mapping `clamp(1 + w_ent * (mean - 0.5), 0.85, 1.15)` is computed explicitly in the assertions rather than approximated. |

Stage D is additive behind the shipped Stage A/B surface; the breakdown field
`entity_posterior_mean` keeps its shape and now reports the decayed mean.
