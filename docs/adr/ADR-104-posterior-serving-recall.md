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
tokens (and adjacent-token bigrams) that equal the name of an existing KG entity under the
bounded case contract become entity candidates regardless of ASCII capitalization. Matching
against real entity records is what makes lowercase coverage safe: a token can only earn the boost by
naming something the graph actually knows — and it serves unsegmented CJK queries through
substring lookup against entity names rather than whitespace tokenization.

Entity-name matching is case-insensitive for ASCII, exact-form for cased non-ASCII scripts,
and caseless scripts such as CJK are unaffected.

Implementation constraint: one indexed lookup per recall, driven by a distinct relation of at
most 64 token candidates. Each candidate performs one `LIMIT 1` seek against a live-row partial
index on `(namespace, LOWER(name))`; work is bounded by candidates rather than duplicate or
tombstoned entity rows. Explicit caller-supplied `entity_names` continues to win over all
extraction, and `entity_names: []` remains a full opt-out (#738 semantics unchanged).

## What this deliberately does not do

- **No global weight mutation.** `apply_config` remains callerless in production; if a
  future consumer wants persistent tuned defaults, that is a separate decision with its
  own rollback story.
- **No background loop.** Per-request projection makes freshness structural. The
  `brain.activate` description is corrected rather than implemented-toward.
- **No cross-profile blending.** A request is served by exactly one profile's state
  (resolved or overridden), or by defaults.
- **No new storage.** Every input already exists: posterior state (brain snapshots),
  profile resolution (ADR-081), entity names (kg store). Superseded in scope by
  Amendment 2 for Stage D: no new table, column, or external store is added, but the
  persisted snapshot payload gains versioned fields (per-entry `last_event_at` and a
  decay-policy record), with the migrations Amendment 2 specifies.

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
  `RecallHit` retains its shipped global-posterior updates (relevance and temporal);
  those global updates are applied to the profile stamped by the serve-time
  `serve_attribution="profile"` contract (ADR-032 Amendment 2), not unconditionally to
  `balanced-recall-v1`. An `unattributed` hit updates no profile, while an
  `unspecified`/legacy hit retains the built-in fallback. `NoteAccessed` has no
  global-posterior effect in the shipped reducer and loses only
  its per-entity effect here, so after Stage D a `NoteAccessed` leaves every posterior
  unchanged. Only the per-entity accrual rule changes. This amends the accepted ADR's
  Stage B accrual behavior as part of Stage D.
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
  reducer never reads a wall clock. The write rule is a complete three-way branch on
  `event_time` versus the entry's `last_event_at`:
  - `event_time > last_event_at`: decay by `dt = event_time - last_event_at`, add the
    observation's weight, stamp `last_event_at = event_time`.
  - `event_time == last_event_at`: add the observation's weight with no decay
    (`g = 1`); the stamp is unchanged.
  - `event_time < last_event_at`: the per-entity entry is returned unchanged. The
    regressed event contributes no per-entity evidence and does not move the stamp;
    it is not an error. Events arrive from an ordered log, so a regression is a clock
    anomaly, and deterministic replay requires the same drop decision at every replay.
    The event's global-posterior effects, if any, are unaffected by this rule.
- **Read path** (component 2's `entity_posterior_mean`): compute the decayed mean as a
  pure function of stored state and an injected current time, with
  `dt = max(now - last_event_at, 0)` days. A read clock earlier than `last_event_at`
  therefore projects with `g = 1` (no additional decay), never with `g > 1`: a
  regressed clock must not amplify evidence. No writes on read: serve-time projection
  stays deterministic and side-effect free, exactly as the accepted ADR requires.
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
`beta_ev = max(beta - 1, 0)`) and stamping `last_event_at` from the snapshot's own
persisted `updated_at`, so grandfathered evidence starts its decay clock at the moment
the snapshot state was last true rather than being retroactively expired. The anchor
is deliberately a persisted value, never the load-time wall clock: conversion is a
pure function of stored bytes, so migrating the same v1 snapshot at any two wall-clock
times yields identical v2 state, and post-snapshot events replay under the ordinary
three-way write branch (their `event_time` is at or after the snapshot's `updated_at`
in a well-formed history, so no evidence is dropped by the regression branch). A v1
snapshot with no persisted timestamp is malformed and is rejected at load, not
defaulted. Version-0 snapshots (the still-supported legacy format with prior-inclusive
pairs and empty cache-order metadata) convert by the same rule: subtract the prior from
each pair, anchor `last_event_at` to the snapshot's persisted `updated_at`, and emit an
explicit version-2 order that preserves the documented deterministic ascending-UUID
compatibility order the v0 loader applies today. Neither legacy version is ever written
again after a version-2 load.

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

| Stage | Contents                                              | Gate                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| ----- | ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| D     | Evidence/prior decomposition + lazy exponential decay | Unit: decayed mean converges to 0.5 over elapsed time; update-after-gap decays before adding; v0 and v1 snapshot migrations both round-trip (v0 preserving the deterministic ascending-UUID compatibility order); read path provably write-free; read-side clock regression projects with `g = 1`, never amplifies; a `RecallHit` leaves per-entity evidence unchanged and a `NoteAccessed` leaves every posterior unchanged; configuration rejects zero, negative, and non-finite `H`; all three write branches tested (later decays-adds-stamps, equal adds without decay, earlier leaves the entry byte-identical). Replay determinism: a replay that starts from a migrated v0 or v1 snapshot and applies post-snapshot events, executed at two different wall-clock times, yields byte-identical stored per-entity state, for both balanced-recall and profile-local state (event-time clock plus persisted migration anchor, ADR-032). Behavior (clock-injected, magnitudes pinned in evidence-mass units): a `B = 1` negative posterior's component-2 term returns within 0.02 of neutral after two configured half-lives; a single-correction `B = 2.0` posterior is provably NOT within 0.02 at two half-lives; a `B >= 8` posterior is NOT within 0.02 at two half-lives but is within 0.02 by `H * (log2(B) + 2.2)`; the mean-to-multiplier mapping `clamp(1 + w_ent * (mean - 0.5), 0.85, 1.15)` is computed explicitly in the assertions rather than approximated. |

Stage D is additive behind the shipped Stage A/B surface; the breakdown field
`entity_posterior_mean` keeps its shape and now reports the decayed mean.

## Amendment 2 (2026-07-12): Closing the Stage D contract

Amendment 1's static review surfaced four gaps that leave Stage D
implementation-ambiguous. This amendment closes them. It changes no algorithm from
Amendment 1; it makes previously implicit inputs explicit, normative, and persisted.

### Normative signal-to-mass mapping

Amendment 1 assigned masses to the semantic signal names only, leaving the
still-supported legacy feedback ladder (`useful`, `not_useful`, `wrong`) at the
shipped unweighted count. Two interpreters could produce different state from the
same event history. The complete mapping is now normative; every signal accepted by
the feedback surface appears here, and Stage D asserts each row:

| Signal              | Polarity | Evidence mass |
| ------------------- | -------- | ------------- |
| `correction`        | negative | 2.0           |
| `explicit_positive` | positive | 1.5           |
| `explicit_negative` | negative | 1.5           |
| `implicit_positive` | positive | 0.1 (clamped) |
| `implicit_negative` | negative | 0.1 (clamped) |
| `useful`            | positive | 1.5           |
| `not_useful`        | negative | 1.5           |
| `wrong`             | negative | 2.0           |

The legacy rows are an intentional change from the shipped unweighted 1.0, effective
at Stage D, not a description of current behavior: `useful` and `not_useful` are
explicit human judgments and carry explicit weight; `wrong` asserts factual
incorrectness and carries correction weight. The clamp on cumulative implicit mass
from Amendment 1 applies to the implicit rows only. This table is versioned as part
of the decay policy below; changing any row is a policy-version bump, never an
in-place edit.

### Determinism restated with the read clock

The accepted decision's property "same store, same query, same profile state produce
the same ranking" predates read-time decay and is now incomplete. It is restated:
ranking is a deterministic, side-effect-free function of **(store, query, profile
state, read time, decay policy)**. Serve-time projection takes the read clock as an
explicit injected input; nothing else about the property changes. Consequences:
evaluation and score-comparison artifacts must record the read time they were
computed at, and every Stage D ranking test pins an explicit clock. Two reads at
different times may legitimately order results differently; that is the designed
decay behavior, not nondeterminism.

### The decay policy is persisted replay state

Amendment 1 made `entity_evidence_half_life_days` bare runtime configuration. That
contradicts the replay contract: replaying the same events against the same snapshot
under a different configured `H` yields different state, so an unrecorded mutable
config value silently breaks byte-identical replay. Ruling: **the decay policy is a
versioned input stamped into persisted state**, superseding Amendment 1's
"one value per running instance" paragraph.

- **Every policy record is complete.** Every persisted decay-policy record has this
  schema, both in a version-2 `BalancedRecallSnapshot` and in each policy-migration
  event payload:

  ```text
  DecayPolicyRecord {
    policy_version: u32,
    half_life_days: f64,
    mass_table_version: u32,
    mass_table: [{ signal: EvidenceFeedbackSignal, polarity: positive | negative, evidence_mass: f64 }, ...]
  }
  ```

  `EvidenceFeedbackSignal` is a new closed persisted-row enum containing exactly the
  eight wire names in the signal table above. Neither shipped type covers all eight:
  `FeedbackSignal` holds only the three legacy names and `FeedbackEventKind` holds
  the five semantic names (`crates/khive-brain-core/src/signal.rs`). Stage D defines
  the new enum with pinned serde names equal to the wire names, a one-way `From`
  mapping from each `FeedbackSignal` and `FeedbackEventKind` value, and a test
  asserting the enum has exactly eight members matching the table. The existing
  public feedback types and their consumers are unchanged.

  `mass_table` is the complete signal-to-polarity-and-mass table above, in the row
  order shown and with every signal present exactly once. It is persisted inline,
  not reconstructed from `mass_table_version`. The version remains a provenance
  label identifying the code-side source table. Code-side mass tables are
  version-indexed and immutable: changing any row adds a new version entry, and an
  existing entry is never edited in place.
- **Ownership is per-state, with one authoritative representation.** Every
  version-2 `BalancedRecallSnapshot` carries its own complete policy record: the
  namespace's built-in state and each profile-local state independently. The live
  `BalancedRecallState` in `BrainState.balanced_recall` or
  `BrainState.profile_states` is authoritative. `ProfileRecord.state_snapshot` is
  a regenerated mirror, never an independent policy owner. Profile creation and
  every mutation that changes a state's policy, including migration, update the
  authoritative state and regenerate the profile-record mirror in the same durable
  mutation, as required by ADR-032. `brain.profile` reports the policy from the
  authoritative live state, with the mirror required to match it. Initial values for
  fresh and migrated state are `policy_version = 1` and `mass_table_version = 1`,
  with a complete inline copy of the mass table in this amendment. v0/v1 migration
  and boundary validation apply to every state, built-in and profile-local alike.
- Folding and replay read `half_life_days` and signal masses only from the state's
  complete persisted policy record. They never consult process configuration, the
  current code default, or the code-side table indexed by `mass_table_version`.
  Configuration and the immutable code-side registry are consulted only to build a
  complete policy for fresh state or v0/v1 snapshot migration. They never migrate
  existing version-2 state implicitly.

### The sole policy-migration write path

Exactly one scoped administrative operation may change the decay policy of existing
state: `kkernel brain migrate-decay-policy`. It is an operator command on the
`kkernel` admin CLI path, not a brain verb, MCP product verb, subhandler, or other
agent-facing operation. Its event payload schema is:

```text
PolicyMigrationPayload {
  scope: global | { profile_id: String },
  expected_old_policy: DecayPolicyRecord,
  new_policy: DecayPolicyRecord,
  actor: String,
  timestamp: DateTime<Utc>,
}
```

`global` targets the selected namespace's built-in state. Both policy fields are
complete records, including their inline mass tables. `new_policy` must pass complete
boundary validation, including a finite, strictly positive `half_life_days`, every
supported signal exactly once with its normative polarity and a finite, strictly
positive mass, and a bumped `policy_version`. At this migration boundary only, and
never during fold or replay, `new_policy.mass_table` must also match the immutable
code-side registry entry named by `new_policy.mass_table_version` field for field;
a payload whose inline table diverges from its named registry version is rejected
before anything is written. Changing any mass therefore requires registering a new
table version first. This keeps `mass_table_version` an auditable provenance claim
rather than a free label, while replay still reads only the inline table.

The operation follows the staged-mutation pattern in
`crates/khive-pack-brain/src/persist.rs`: mutations apply to a proposed state without
touching live state, and event append plus snapshot upsert form one atomic unit. Inside
the same transaction that appends the event, it loads the current scoped durable state,
constructs the proposed state, validates the complete new policy against that proposed
state, and compares `expected_old_policy` exactly with the current policy. The
expected-old comparison is not an advisory preflight outside the transaction. On a
match, the transaction applies the new policy to the proposed authoritative state,
regenerates the corresponding `ProfileRecord.state_snapshot` mirror, appends the
migration event, and upserts the namespace snapshot as one atomic unit. Only after
commit does the proposed state replace the live state. On a stale expected-old record,
invalid payload, append failure, snapshot failure, or commit failure, it writes nothing
and leaves the authoritative state and its mirror unchanged. Transaction serialization
therefore ensures that two concurrent requests carrying the same expected-old record
cannot both append, and no unreplayable migration event can enter the log.

Reducer behavior at the event's fold position is unchanged in shape: validate that
the complete `expected_old_policy` equals the state's current complete record
(mismatch is a hard fold error, not a silent re-stamp), then replace it with the
complete `new_policy`. The migration event touches no evidence counts and no
`last_event_at` stamps; a subsequent event whose decay interval spans the migration
decays its full interval under the new `H`. Because the migration is an ordered event,
every replay applies the same persisted policy to the same fold positions.

v0/v1 snapshot migration stamps the migrating instance's complete configured policy
into the produced v2 state because those formats persisted no policy. Migration is
therefore a pure function of (stored bytes, configured complete policy): the same
snapshot migrated twice under the same configuration is byte-identical, and the wall
clock remains a non-input.

### Storage claim reconciled

The accepted decision's "no new storage" claim remains true at the granularity it
was made: Stage D adds no table, no column, and no external store. It does add
versioned fields inside the existing snapshot payload: per-entry `last_event_at`
and the complete decay-policy record above. For profile-local state,
`ProfileRecord.state_snapshot` is the regenerated mirror inside that same namespace
snapshot, not a second persistence authority. The v0/v1 migration anchor is the
existing `brain_profile_snapshots.updated_at` column, and migration must run at load,
before snapshot deserialization discards that row-level metadata.

### Stage D gate additions

The Stage D gate gains these assertions:

- Every signal in the mapping table folds with exactly its stated polarity and mass,
  including the three legacy names.
- Ranking tests inject and pin an explicit read clock; the projection function is
  pure in it.
- A replay folds a log whose persisted inline mass table differs from the current
  code default and produces byte-identical state across runs using only the persisted
  table; neither the code default, `mass_table_version` lookup, nor process
  configuration supplies a mass to the fold.
- A replay that starts from a pre-migration snapshot and crosses a policy-migration
  event (an `H` change and a mass-table change are both exercised) folds
  pre-migration events under the old policy and post-migration events under the
  new one using the complete inline tables in both event policy records, and is
  byte-identical across runs; a migration event whose complete expected-old record
  does not match the state's current record fails the fold loudly.
- The admin migration operation persists nothing for a stale expected-old record;
  two concurrent operations carrying the same expected-old record cannot both append;
  and an invalid complete new-policy payload persists nothing.
- A migration payload whose inline mass table is complete, normatively signed, and
  strictly positive but does not match the immutable registry entry named by its
  `mass_table_version` appends neither event nor snapshot.
- The `EvidenceFeedbackSignal` persisted-row enum has exactly the eight wire names
  in the signal table, with pinned serde names, and every `FeedbackSignal` and
  `FeedbackEventKind` value maps one-way onto it.
- Two profiles created on opposite sides of a configuration change carry their
  respective creation-time policies, and each serves under its own.
- Migrating the same v0 or v1 snapshot twice under the same configured policy yields
  byte-identical v2 state, and every produced state (built-in and profile-local)
  carries the stamped complete policy record.
- `brain.profile` reports the same decay policy as the authoritative live
  `BalancedRecallState` after profile creation, policy migration, reload, and replay;
  each assertion also verifies that `ProfileRecord.state_snapshot` is the regenerated
  mirror of that live state.

## Amendment 3 (2026-08-03): Required judgment on the automatic feedback surface

### Problem, measured

`brain.auto_feedback` defaults an omitted `signal` to `implicit_positive` and credits
the first recall result. On a production deployment this default dominated the event
history: 6,278 of 7,414 feedback events were `implicit_positive`, several serving
profiles had accumulated zero negative events ever, and a six-hour window showed
individual profiles at 16/16 and 19/22 implicit-positive despite caller-side
instructions requiring an explicit judgment. Instruction-level enforcement is
measured-insufficient; the emitting surface is the lever.

The defect is structural, not volumetric: an omitted judgment is not weak positive
evidence, it is the absence of evidence. Converting it into a positive event makes the
retriever's own ranking its training data — a rank-position feedback loop in which the
first result is credited for having been first. Serving paths that structurally cannot
judge (pre-serving prefetch, session-boot recall) are today indistinguishable from
endorsements. The same rank-position credit also exists below the feedback surface:
the recall path itself awards a per-entity posterior success to the top result of
every recall (see Change 3).

### Change 1 — `signal` is required

`brain.auto_feedback` rejects a call without `signal` as invalid input (the error
enumerates the accepted values); no event is emitted. The serving-path default is
deleted. The accepted values on this surface are `useful`, `not_useful`, `wrong`, and
`unjudged`. The implicit rows remain in the historical store and in the Amendment 2
table for interpreting that history; they are no longer emittable through
`brain.auto_feedback`.

`brain.feedback` (the manual, explicitly-parameterized surface) is unchanged: its
caller always states a signal, so it never had this defect.

### Change 2 — `unjudged`: telemetry without endorsement

A new wire signal value, `unjudged`, records that a serve happened and no judgment was
available. Callers that structurally cannot judge — prefetch hooks, session-boot
recalls, any instrument that fires before a consumer sees the results — pass
`signal="unjudged"`. The enforceable rule on this surface is precisely: omission is
rejected (Change 1), and a caller that sends `unjudged` asserts that no judgment was
available. The wire cannot verify that assertion — the request carries no producer
class or capability from which "structurally unable" could be checked, and this
amendment adds none. What the rule removes is the silent path: before it, an
undecided caller was converted into an endorsement by default; after it, recording
non-judgment requires stating non-judgment, which is an honest claim under the
caller's own name rather than a fabricated positive under nobody's.

**`unjudged` is wire vocabulary, not evidence.** It is not a member of
`EvidenceFeedbackSignal`, it does not appear in the Amendment 2 mass table, and it
never constructs an evidence row. Amendment 2's closed eight-member enum, its
every-row-exactly-once rule, and the migration validator's strictly-positive-mass
requirement are all unchanged by this amendment. The wire vocabulary of
`brain.auto_feedback` and the evidence vocabulary are distinct sets by design: a
signal that carries no evidence must not be representable in the evidence type.

One Amendment 2 sentence is narrowed to make that split well-formed. Amendment 2
states that "every signal accepted by the feedback surface appears" in the normative
mass mapping; that sentence is henceforth scoped to every **evidence-bearing** signal
accepted by the `brain.feedback` evidence surface. Wire-only values — today exactly
`unjudged` — are excluded from the table by design, not by omission. The eight-row
table, its row set, and its positive-mass invariant are unchanged.

**Event kind.** `unjudged` acceptances persist under a new event kind,
`feedback_unjudged`, rather than under `feedback_explicit`:

- `feedback_explicit` flat counts are consumed as judgment counts (including as a
  discipline-ratio numerator against `actor_turn`). A non-judgment row inside that
  kind would silently inflate every such read; the kind's own name would misdescribe
  its contents. With a separate kind those counts stay judgment-only with zero
  consumer changes.
- The event-count surface groups by kind generically, so `feedback_unjudged` appears
  as its own row in existing kind breakdowns with no migration.
- The event-kind enum is closed and compile-time; adding `feedback_unjudged` updates
  the enum, its ALL list and count, canonical name, valid-name list, and parser
  together, following the same pattern as every prior kind addition.

**Fold boundary, stated against the implementation as it is.** The brain event
interpreter today dispatches on the emitting verb alone, and an unknown top-level
signal value degrades to an ignored no-op rather than being quarantined (only invalid
`section_signals` maps are quarantined on replay). Incidental ignorance is not a
contract: it would hold only until the signal parsers are next extended. This
amendment therefore makes the exclusion normative and explicit — the fold boundary is
the pair (emitting verb, event kind), and the interpreter gains an explicit
`feedback_unjudged` arm that classifies the row as irrelevant-to-training _before_
signal parsing. A valid `feedback_unjudged` row is persisted, never quarantined: it is
correct telemetry, not a malformed input.

**Persistence route for unattributed serves.** The current handler permits
profile-less events only through the implicit-mass gate, and rejects every other
profile-less signal. Neither route fits `unjudged`: it must not create an
implicit-mass row, and it must not be rejected. This amendment specifies a third
route: `feedback_unjudged` events append atomically to the event log with full serve
attribution when available (profile id, serve attribution, scorer run, serve-ledger
id, query, candidate ids) and with no profile mutation, no posterior touch, and no
fold-gate row — from attributed and unattributed serves alike. This composes with the
existing invariant that no profile receives explicit credit from an unattributed
serve: `unjudged` grants no credit from any serve. An `unjudged` event carrying
serve-ledger provenance does **not** consume the ledger's grade slot or its
deduplication budget: a later genuine judgment for the same serve must still be
recordable exactly once.

### Change 3 — no synthesized judgments anywhere in the serving path

No serving-path component may manufacture a judgment the caller did not state. This
rule is not new: Amendment 1's Stage D accrual rule already excludes both `RecallHit`
and `NoteAccessed` from per-entity evidence, and its gate already requires that a
`RecallHit` leave per-entity evidence unchanged and a `NoteAccessed` leave every
posterior unchanged. Amendment 1 is itself still Proposed; this amendment does not
ratify it wholesale. It adopts exactly two of its clauses as normative for this
change — the Stage D per-entity exclusion of `RecallHit` and of `NoteAccessed`, with
`RecallHit`'s entity-blind global updates retained — and takes no position on the
rest of that rider. The shipped reducer still applies both per-entity arms, so those
clauses are specified but not yet satisfied by the code. This change supersedes
nothing in Amendment 1 — it schedules the implementation that satisfies the two
adopted clauses, and enumerates the affected paths explicitly:

1. The `brain.auto_feedback` omitted-signal default — deleted (Change 1).
2. The memory pack's post-recall hook, which awards a per-entity posterior success to
   the top result of every non-empty recall — the per-entity arm is removed. The
   entity-blind arms of the same hook (recall availability, latency-bucketed temporal
   state) are measurements of the serving function, not judgments of a result, and
   are retained.
3. The brain core's per-entity signal extraction, which maps a recall hit to a
   positive per-entity update — the recall-hit arm stops yielding a per-entity
   signal. The entity-blind global recall parameter (hit/miss) and the temporal
   posterior remain driven by recall events, exactly as Amendment 1 retains them.
4. The same extraction's note-access arm, which maps a direct record access
   (`get`/`remember`) to a positive per-entity update — removed under Amendment 1's
   existing exclusion, whose gate already requires a `NoteAccessed` to leave every
   posterior unchanged. Nothing here re-decides that rule; the arm is listed so the
   implementing change cannot satisfy the recall arms while leaving this one live.

**Normative rule for consumers.** Per-entity evidence state originates exclusively
from judgment-bearing feedback events. No fold or serving consumer may derive
per-entity state from recall, access, or serve events, whatever fields those events
carry. Recall events retain their `target_id` provenance deliberately: event-level
provenance is the audit instrument that exposed the implicit-positive loop this
amendment corrects, and deleting the audit surface to prove loop-absence would blind
the detector. The acceptance arms below are the point-in-time witness that the
shipped stacks obey this rule at amendment time; this clause, not those tests,
governs every future consumer.

After this change, a serve with no subsequent judgment leaves, at most, a
serve-ledger row (the existing best-effort serve ledger is retained as-is; this
amendment does not make it durable) and a `feedback_unjudged` event — never a
positive update of any per-entity or per-profile evidence state. The
later-judgment and deduplication rule in Change 2 applies when serve-ledger
provenance is available; when the best-effort ledger recorded nothing, an `unjudged`
event still persists on its own.

### Origin stamping: wire contract

Every feedback event written after this amendment carries a required payload field
`originating_verb`, with the closed value set `brain.feedback` |
`brain.auto_feedback` | `brain.emit` (the last for the deprecated legacy path). The
stored event verb remains `brain.feedback` for replay and historical-query stability;
the forwarding path stamps `originating_verb` before delegating, so the stored verb
and the originating surface are separately queryable. Historical rows lack the field;
absence means pre-amendment, and no backfill is performed.

**Executable read contract.** Origin and attribution are read through the generic
event list, not through the aggregate count surface: the acceptance query is
`list(kind="event", event_kind="feedback_unjudged")` (and the same list filtered to
the feedback kinds for judgment rows), with the payload predicate applied
client-side — each returned row's payload is asserted to carry `originating_verb`
with a value from the closed set above, and serve attribution
(`served_by_profile_id`, serve-ledger id when available) intact. The aggregate
count surface groups by kind generically and inspects payload fields for no kind
except explicit judgments, so its kind breakdown serves as a volume check only;
it is not the instrument that verifies attribution or origin survival. Without
this field and this query, residual implicit rows cannot be attributed to an
emitting surface.

### Acceptance arms — executed, not claimed

Each arm is a test that must exist and pass with the implementing change:

1. **Rejection with absence assertion.** A signal-less `brain.auto_feedback` call
   returns invalid input, and the event store is asserted to contain zero new rows
   from the call — the rejection writes nothing.
2. **Wire survival.** A `signal="unjudged"` call writes a `feedback_unjudged` row
   that survives ingestion and is read back through the acceptance query defined
   above — `list(kind="event", event_kind="feedback_unjudged")` — with the
   client-side payload predicate asserting both `originating_verb` and serve
   attribution on the returned row; the event-count kind breakdown is additionally
   asserted to show the row as a volume check. The two surfaces where a validator
   could silently eat the row are named as test targets: the closed event-kind
   valid-name list and parser in the types crate, and the closed signal-string match
   in the brain handlers — both must accept the new values, and the test exercises
   the full write-then-read path rather than the validators in isolation.
3. **Training exclusion, live and replayed.** Folding a stream containing
   `feedback_unjudged` rows produces posterior state identical to folding the same
   stream without them — asserted over global, per-profile, per-section, and
   implicit-mass state, in both the live-write path and the replay path.
4. **No synthesized credit.** A recall returning results, followed by no caller
   judgment, leaves every per-entity posterior unchanged in both the memory-pack
   recall state and the brain-core profile state; only the entity-blind availability
   and temporal arms may move.

### Sequencing and acceptance window

The origin-stamping contract above lands first or in the same change as Changes 1–3;
without it the acceptance window cannot attribute residual implicit rows to a path.
Acceptance: an observation window under normal agent load shows zero newly-emitted
`implicit_positive` events from serving-profile traffic — every event is either a
stated judgment or a typed `feedback_unjudged` row. Historical events are untouched;
this amendment changes what may be written, never what was written.

### Implementation note (2026-11-02)

Items 1-4: only `Feedback` and `SemanticFeedback` yield a per-entity signal (`entity_signal`,
`khive-brain-core/src/brain_signal.rs`), so `apply_feedback_posteriors` is the sole per-entity writer (`apply_signal` calls it for every signal); a `RecallHit`
still moves relevance and temporal state, and a `NoteAccessed` moves no posterior.
Replay (`replay_events`, `khive-pack-brain/src/persist.rs`) folds each event into the one profile the live path credited, in four arms:

- feedback: a row with no `served_by_profile_id` key (a legacy row written before stamping) goes to balanced-recall; a string goes through `apply_feedback_signal` (`pack.rs`), the fold the feedback write path uses, which routes by `feedback_route`; a null value (no profile resolved) or any other non-string value is skipped. The mixed replay fixture covers both the absent key and the null value.
- recall hit: `dispatch_route` follows its attribution; `unattributed` hits and hits crediting an archived profile are skipped.
- note access and every other dispatch signal: balanced-recall.
- `feedback_unjudged` rows are skipped; `brain.section_feedback` rows update section state only.

Tests in `khive-brain-core/tests/judgment_only_entity_evidence.rs`: `recall_without_judgment_leaves_entity_posteriors_unchanged`,
`note_accessed_leaves_all_posteriors_unchanged`, `judgment_still_moves_entity_posterior`. Tests in
`khive-pack-brain/src/persist/replay_routing_tests.rs`: `replay_matches_live_routing_per_profile`, `replay_named_profile_entity_evidence_matches_own_judgments`,
`replay_balanced_only_fixture_matches_prior_reducer`, `replay_folded_event_counts_sum_to_log_length`,
`replay_credits_judgments_to_a_profile_archived_after_them`. Test in `khive-pack-brain/src/durable_write_tests.rs`:
`judgments_survive_archive_and_restart`.

## Amendment 4 (2026-11-02): Default weights while serving, and a fixed shared-pool factor

**Status**: Accepted 2026-11-02 (Proposed 2026-11-02). Changes §1 (serve-time projection of profile weights) and adds
a fixed rank factor carried by a profile. The implementing change lands after
Amendment 3 Change 3 (a recall hit no longer credits the per-entity posterior of the hit it ranked first), on which
Change 2 relies. Rebinding a named profile is gated per profile by Change 4.

### Problem, measured

**1. Projected weights rank worse than the defaults.** §1 serves each profile's three posterior means as its relevance,
salience and temporal weights, with no normalization (`PackTunable::project_config`). On the measured deployment those
posteriors move mostly on signals that are not explicit judgments: relevance gains on every recall that returns a hit,
temporal moves with recall latency, and salience gains on every positive feedback event, implicit ones included (the
measured export holds 8,070 implicit positive events beside 7,136 explicit judgments). The served means are near
relevance 1.0, salience 0.8 and temporal 0, against the defaults 0.7, 0.2 and 0.1. The composite then passes 1.0 and is
clamped, so inside the top ten clamped hits are ordered by the entity term, lowered by the duplicate penalty where
one applies, and then by the id tiebreak, not by relevance.

On a frozen copy of the deployment's store, three readers, 30 questions per bank, answers checked by hand:

| Measure                                                                                  | No profile         | Profile served     |
| ---------------------------------------------------------------------------------------- | ------------------ | ------------------ |
| Top-10 hits at the clamp, own-namespace banks, each of four profiles in turn (see below) | 0 of 900           | 363 to 418 of 900  |
| Top-10 hits at the clamp, shared-pool banks, each of four profiles in turn (see below)   | 0 of 900           | 378 to 416 of 900  |
| MRR@10, answers in the reader's own namespace, the reader's own profile (90 questions)   | .297               | .273               |
| MRR@10, answers in the shared pool, the reader's own profile (30 questions, 3 read sets) | .355 / .355 / .354 | .320 / .322 / .351 |

The clamp rows count top-10 hits whose returned rank score, divided by the entity term recomputed from the stored
posterior, is at least 0.999. A hit lowered by the duplicate penalty is not counted, and a hit just under 1.0 is, so
the counts are close to the number of clamped hits rather than exact.

hit@10 is equal between the arms on every bank: the projected weights reorder the answers already in the top ten.
Rescoring the no-profile arm's returned hits, whose rank scores already use the default weights, with each profile's
stored entity term gives .298 on the own-namespace questions, and with an entity term rebuilt from explicit judgments
only, .296, against .297 without the term and .273 served. On this sample the entity term moves MRR@10 by at most .002
once the weights are the defaults, while the served profile, projected weights and entity term together, loses .024; the
rescoring reorders only the 50 returned hits, so it bounds the term's effect on these questions, not end to end. A live
probe on the deployment showed the saturation directly: one query returned three hits all at `rank_score` 1.000 under
the bound profile, so their order fell to the id tiebreak, and the same query with the binding removed returned them at
0.693, 0.598 and 0.529, in a different order.

**2. A recall that names a namespace serves only the profiles held there.** Brain state, profiles included, is loaded
from the request's primary namespace, and an explicit `namespace=` makes that namespace primary. On a deployment whose
profiles live in `local`, a recall with `namespace=<the caller's own>` returned `serve_attribution: "unspecified"`
while a binding was active, and adding `profile_id` returned `profile_id ... is not a known profile`. A recall naming
`namespace="local"`, or a namespace that holds the profile, serves it. This amendment does not change that path.

**3. The shared pool outranks the caller's own rows on equal terms.** No ranking stage reads a candidate's namespace.
Recomputing the no-profile arm's 50 returned hits with `local` rows scaled by a fixed factor `f`:

| Answers in                                         | `f` = 1            | `f` = 0.7          | `f` = 0.5          |
| -------------------------------------------------- | ------------------ | ------------------ | ------------------ |
| Caller's own namespace, MRR@10 (90 questions)      | .297               | .375               | .444               |
| Caller's own namespace, hit@10                     | .433               | .533               | .567               |
| Own-namespace questions ranked lower than at f = 1 | -                  | 0 of 90            | 0 of 90            |
| Shared pool, MRR@10, three read sets               | .355 / .355 / .354 | .355 / .355 / .315 | .346 / .338 / .282 |

The cost falls on the read set that holds the most rows of its own that compete with the shared answer. These numbers
reorder only the 50 hits the no-profile arm returned and apply the factor after admission, so they motivate the value
and do not decide it; Change 4 decides it end to end.

### Change 1: serve the default weights

A served profile scores with the configured default weights; §1's projection is no longer applied to the request.
The projection stays computed and reported:

- `brain.profile` adds a top-level `posterior_means` object, `{relevance, salience, temporal}`, each the mean of the
  stored posterior, or `null` when the profile has no state snapshot.
- With `include_breakdown=true`, each hit's `breakdown` adds `weights_served`, the string `"default"`, and keeps
  `profile_component` with its current definition (the hit's composite under the projected weights divided by its
  composite under the defaults), computed for the breakdown only. It no longer enters `rank_score`.
- `brain.config` keeps returning the projected configuration as a diagnostic; it is not what recall serves.
- The `min_score` help text stops listing a weight-reprojection component among the terms that adjust `rank_score`.

Tests that assert a projected weight reorders results (the Stage A and B ordering tests) are replaced by tests that a
served profile's order equals the default-weight order with the same entity term, while `profile_component` still
differs from 1.0 where the posteriors differ from the defaults. Serving a projection again requires an amendment that
defines how explicit judgments alone train the three weight posteriors and that passes the Change 4 gate.

### Change 2: a fixed shared-pool factor carried by the profile

`brain.create_profile` accepts an optional `local_factor`, a finite number in [0.5, 1], stored as given and rounded to a
32-bit float when applied to the 32-bit rank score. Scaling by a positive constant never reverses the order of two
scores and can tie them only where they already lie within rounding of each other; the floor bounds how far a
shared-pool row can fall below its unscaled score, and the measured values (0.7, 0.5) lie inside the range. The
duplicate penalty is subtracted after the factor (step 3 below) and floors at zero, so a later shared-pool copy whose
scaled rank score is at or below the penalty falls to zero, where the id tiebreak orders it against other zeroed copies
and a positive `min_score` drops it, as happens today to an unscaled copy at or below the penalty. The factor widens
that set to copies whose unscaled score is at most the penalty divided by the factor; the floor of 0.5 keeps it to at
most twice the penalty. It is a field of the profile record, persisted with the record in the brain state snapshot, and returned by `brain.profile` as a
top-level `local_factor`. Only `brain.create_profile` sets it; no verb edits it, `brain.reset` keeps it, and a different
value is a new profile. A record without the field, every existing profile included, loads as factor 1, an exact no-op.
`memory.recall` reads it from the `brain.profile` response beside `state_snapshot`.

When such a profile serves a recall, each candidate whose namespace is `local` (`Namespace::LOCAL`, the shared pool;
not configurable today) has its rank score multiplied by `local_factor`; every other candidate is multiplied by 1. The
factor depends on the serving profile alone, whoever the caller is.

Placement, in `memory.recall`'s order of stages:

1. The returned `score`, absolute relevance in [0, 1], is taken before the factor, so it is unchanged for every hit,
   text-only hits included.
2. The factor multiplies the rank score at the point where the entity term is applied, after the optional weighted
   reranker (which, when configured, replaces the composite before the entity term) and before the first `min_score`
   admission.
3. The duplicate penalty, supersedes suppression, the second admission, the sort, the `limit` truncation and the
   character budget then run on adjusted rank scores, as they do today. The duplicate penalty sorts by rank score,
   leaves the first copy of a shared content prefix unpenalized and lowers each later copy by the penalty, so the
   copy spared is whichever ranks higher after the factor.
4. Supersedes suppression is unchanged. When it is enabled (the default): a candidate with an incoming `supersedes` edge
   the reader can see is dropped whatever the scores, and a candidate named in another row's `supersedes` property is
   dropped when that row was admitted. A superseding `local` row the factor scales below `min_score` therefore still
   suppresses through its edge, and suppresses through its property only if admitted, as for any row below `min_score`
   today.

`min_score` and `score_floor` filter the adjusted `rank_score`; their help text states that under a profile with a
factor, `local` rows' `rank_score` is scaled by it. With `include_breakdown=true`, each hit's `breakdown` adds:

- `namespace`, the hit's namespace, which the request was already permitted to read;
- `pre_entity_score`, the rank score before the entity term: the weighted composite, or the weighted reranker's output
  when one is configured;
- `entity_term`, the multiplier the entity posterior applied (1 when none);
- `local_factor`, the factor applied to the hit (1 when none);
- `duplicate_penalty`, the amount the duplicate penalty subtracted from the hit's rank score (0 when none).

A returned hit's `rank_score` then equals `pre_entity_score` × `entity_term` × `local_factor` − `duplicate_penalty`,
since no later stage changes a score. The arithmetic is 32-bit floating point in that order, left to right, with
`local_factor` reported as the 32-bit value applied; the breakdown reports each operand as the 32-bit value used, so a
consumer reproduces `rank_score` exactly by repeating those operations in 32-bit arithmetic. The product of the first
three is the rank score the first `min_score` admission saw.

The factor reorders candidates the request already retrieved and was permitted to read. It adds no candidate, widens
no visibility, writes no posterior and is not authorization. It applies only where a profile serves, which excludes the
namespace-scoped recalls of Problem 2 that find no profile.

### Change 3: storage

No table and no column. `local_factor` is an optional field of the profile record, which the brain state snapshot
already persists; a record without it loads as factor 1. Event replay rebuilds posterior state and keeps the record
field as loaded.

### Change 4: rebind gate, per profile

A named profile is bound for recall again only after an end-to-end run, on a fresh copy of a frozen store, with the
implementing change and Amendment 3 Change 3 built in:

- **Arms**, per bank: no profile; and the candidate profile served by `profile_id`, which scores with the default
  weights, its own entity term and its `local_factor`. Both arms run as the same caller, with the same primary namespace
  (the one holding the candidate profile) and the same readable namespaces, equal to the bank's read set; the harness
  records both sets for each arm and refuses to compare arms whose sets differ. A recall without `profile_id` still
  serves the runtime's configured `brain_profile` when one is set, ahead of any binding, and otherwise a profile bound to
  the caller; so the harness starts the copy's process with no `brain_profile` setting and records it unset, reads the
  caller's bindings before the run and refuses to start if one resolves, and the candidate arm names its profile with
  `profile_id`. Every hit the no-profile arm returns must carry `serve_attribution: "unspecified"` and no
  `served_by_profile_id`; every hit the candidate arm returns must carry `serve_attribution: "profile"` and the
  candidate's id. A hit carrying any other attribution voids the run. A candidate recall that returns no hits counts as a
  miss; a no-profile recall that returns no hits voids its round, since every bank answer is present on the copy and
  the pass is read against that arm. A candidate created for a new factor value is a new profile and starts with no per-entity posteriors, so its
  entity term is 1 for every hit; the report states which applies.
- **Banks**, each named with its count: the reader's own-namespace bank (answers in that reader's namespace, 30
  questions), and the shared-pool bank under that reader's read set (answers in `local`, 30 questions). Answers in a
  third namespace have no bank on the measured store; this gate makes no claim about them.
- **Recall shape**: every recall in every arm asks for 50 hits (`top_k=50`), with `include_breakdown=true`,
  `include_timings=true` and the token budget at the pack's cap. Ranks 1 to 10 of that list are the ranking under test;
  ranks 11 to 50 make the loss split below decidable. At the default configuration the first retrieval round fetches a
  fixed number of candidates whatever the limit, and only the widening rounds, which run while fewer eligible candidates
  than the limit remain, fetch more at 50 than at 10; the timings report lists each retrieval round, and the report
  counts per arm the questions that widened. A hit the token budget cuts counts as not returned, and the report counts
  per arm the recalls that returned fewer than 50 hits. A recall that reports degraded retrieval (each hit stamped
  `degraded`, or an empty response with `degraded: true`) voids its round, which is run again; the report counts
  degraded recalls per arm and round. The verbose envelope returned when more than one embedding model answered
  carries no top-level `degraded` field, so on that shape an empty response cannot be told from a degraded one and
  also voids its round; a question that voids three runs in a row stops the gate, and the report names it.
- **Rounds**: the arms run adjacent in time on the same copy, in the same question order, for two rounds a few minutes
  apart, each round in a fresh process. Ranking reads the clock (row age, and expiry), so two rounds need not return
  identical lists, and the gate draws no conclusion from where they differ. The pass below is read in each round
  separately and must hold in both; MRR@10 and hit@10 are reported per round and as the mean of the two. Beside them
  the report gives, per arm, the number of questions whose top ten differs between the rounds. The property the gate
  relies on, that a recall moves neither the entity term nor the served weights, is tested by Amendment 3's acceptance
  arm 4 (no per-entity change) and by the Validation test below that recalls without judgment leave the served
  weights at the defaults, not by this gate.
- **Pass**, fixed before the run, in each round: on the own bank, MRR@10 not below the no-profile arm; on the
  shared-pool bank, MRR@10 loss at most 0.04 absolute, and at most 1 of the 30 questions whose answer is in the top ten
  without the profile having it outside the top ten with the profile. Both banks must pass; the result is per profile
  and rebinds only that profile.
- **Reported beside the pass**: hit@10 per arm; for each lost question, whether its answer is absent from the profile
  arm's 50 returned hits or returned at ranks 11 to 50; for each gained question, the same split for the no-profile arm;
  per arm, the count of top-10 hits whose `pre_entity_score` is 1.0, which is the composite at its clamp when no
  weighted reranker is configured; the harness records the reranker setting, and with one configured the count is
  reported as not available, since `pre_entity_score` is then the reranker's output.
- **Freshness**: the gate is re-run when the store changes in a way that moves answers between namespaces, such as rows
  re-attributed to their writers.

### Alternatives considered

| Alternative                                                              | Disposition                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| ------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Normalize the projected weights instead of dropping them                 | Rejected for now: the posteriors are trained by non-judgment signals, so a normalized projection serves the same drift at a smaller scale.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| Learned per-namespace multiplier from judgments                          | Deferred: profile state is held per request namespace, not per caller, and a judged target's namespace is not checked against the caller's read set. Both must hold before evidence can move a factor.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| Per-class factors with a separate value for other namespaces             | Rejected: the measured arm scaled `local` only, and a third-namespace stratum has no questions on the measured store.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| Set `local_factor` on existing profiles                                  | Rejected: an editable factor is a migration surface with no replay contract. A new profile version needs none.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| Exempt callers whose actor namespace is `local` from the factor          | Rejected: it would silently ignore an explicit `profile_id` from such a caller, and an actor with no rows of its own is not always named `local`. The factor follows the serving profile.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| A separate field for the profile a judgment trains (`reader_profile_id`) | Deferred to its own amendment: event replay, the fold gate, the serve ledger and the event counts already key a judgment on `served_by_profile_id`, so a second field needs one resolution rule across all of them. Until then a caller names the profile its judgment trains in `served_by_profile_id`, with `serve_attribution` omitted or `"profile"`: `"unspecified"` or `"unattributed"` beside a profile id is refused. Subject to the fold gate's own admission, this trains the named profile when the feedback carries no serve ledger ids; serve ledger ids beside an explicit judgment are accepted only from `brain.auto_feedback` with `useful`, `not_useful` or `wrong` (every other path, `brain.feedback` included, refuses them with any signal but `implicit_positive` or `implicit_negative`), and then train the named profile when the serve's ledger row names no accounting profile and is stamped `"unspecified"`, as an unbound serve's row is; a row that names an accounting profile trains that profile (a different name is refused), and any other row accepts implicit feedback only. A ledger row must match the feedback's namespace and target, and a pair trains once per scorer run: a repeat returns `emitted: false` and trains nothing. |
| Apply the factor after the duplicate penalty and supersedes suppression  | Rejected: the duplicate penalty would then keep a shared copy over an own copy the factor scales below it.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| Unbind named profiles and add nothing                                    | Kept as the interval state until Change 4 passes; it gives up the own-namespace gain in Problem 3.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |

### Validation

The implementing change carries tests for: a served profile ranking in the default-weight order while `brain.profile`
reports its `posterior_means` and the breakdown reports `weights_served: "default"` and a `profile_component` different
from 1.0; the same profile, after recalls with no judgment that move its entity-blind posteriors (asserted moved),
serving the same query with the default weights, the same entity term for every hit and the same order as before
them; a profile without `local_factor` returning results identical to no factor; `brain.create_profile` refusing a
`local_factor` that is not finite or lies outside [0.5, 1]; at factor 0.5 and the default penalty, a later
shared-pool copy with unscaled rank score 0.3 keeping 0.05 while one at 0.15 falls to zero, against 0.2 and 0.05 at
factor 1; `brain.profile` returning the stored factor, and
`brain.reset` and a snapshot reload keeping it; `local` rows scaled and every other namespace unscaled; the returned
`score` of a text-only `local` hit unchanged by the factor; an own copy spared by the duplicate penalty over a
higher-scoring shared copy that the factor scales below it; a superseded row staying suppressed through its edge when
the factor scales its superseding `local` row below `min_score`; `min_score` applied to the adjusted score; each returned hit's `rank_score` equal to
`pre_entity_score` × `entity_term` × `local_factor` − `duplicate_penalty` from its breakdown, with and without a
weighted reranker configured and with a duplicate pair present; the gate harness failing a profile that passes
one round and not the other; the harness voiding a round on a degraded recall in each response shape, an empty
verbose multi-model envelope included; and the harness refusing to start when the runtime has a `brain_profile` set
or the caller has a recall binding.

## Amendment 5 (2026-11-05): A fixed factor on rows outside a profile's own namespace

**Status**: Proposed. Amends Amendment 4 Change 2 (a fixed rank factor carried by a profile) by adding a second factor
and replacing its four-term `rank_score` identity with the five-term one below; extends Change 3's storage rules to the
two new profile fields; and revisits Amendment 4's Alternatives row "Per-class factors with a separate value for other
namespaces" with the measurement below. Every other part of Amendment 4 stands, Change 4's gate included; a profile
carrying the new factor rebinds only by passing that gate.

### Problem, measured

Amendment 4's `local_factor` lowers shared-pool (`local`) rows. On the measured store it lifts the reader's
own-namespace bank and costs the shared-pool bank: the whole shared-pool loss at factor 0.7 is one question whose answer, a `local` row,
falls from first to second behind an unrelated row from another readable namespace, which the factor does not lower.
The factor has one setting, so the own-bank gain and the shared-bank cost move together.

The Amendment 4 gate harness now records, for every recall, each returned hit's namespace, `rank_score` and the terms
of `rank_score = pre_entity_score × entity_term × local_factor − duplicate_penalty`. One gate run therefore lets any
rule over those terms be replayed offline on the recalls it made. A run on a fresh copy of the frozen store (the same
copy source, binary and banks as the Amendment 4 measurement; candidate at `local_factor` 0.7) reproduced the earlier
run's metrics exactly, and its record holds 240 recalls and 12,000 hits with every term present. Replaying the
no-profile arm with 0.7 on `local` rows reproduces the candidate arm's served top ten in 120 of 120 recalls.

Replayed over the no-profile arm, with `local_factor` f on `local` rows and a factor r on every row outside
`lambda:khive`, the namespace whose questions form the own bank (`local` rows get f × r; `lambda:khive` rows get 1).
The gate's recalls run as one actor whose request namespace is `local` in both banks, so the favoured namespace in
this rule comes from the bank, never from the request:

| f    | r     | Own bank MRR@10 / hit@10 | Shared-pool bank MRR@10 / hit@10 | Certified | Certified, clear by the penalty |
| ---- | ----- | ------------------------ | -------------------------------- | --------- | ------------------------------- |
| 1    | 1     | 0.065 / 0.133            | 0.326 / 0.533                    | 120       | 114                             |
| 0.7  | 1     | 0.141 / 0.300            | 0.310 / 0.533                    | 120       | 80                              |
| 1    | 0.8   | 0.107 / 0.200            | 0.326 / 0.533                    | 120       | 98                              |
| 0.85 | 0.8   | 0.146 / 0.300            | 0.325 / 0.533                    | 120       | 48                              |
| 1    | 0.667 | 0.150 / 0.300            | 0.325 / 0.533                    | 120       | 38                              |
| 0.85 | 0.667 | 0.222 / 0.333            | 0.325 / 0.533                    | 112       | 8                               |
| 0.7  | 0.8   | 0.221 / 0.333            | 0.309 / 0.533                    | 112       | 6                               |
| 0.7  | 0.667 | 0.229 / 0.333            | 0.297 / 0.533                    | 42        | 2                               |
| 0.5  | 1     | 0.224 / 0.333            | 0.294 / 0.533                    | 92        | 4                               |

Each metric is the mean over two rounds of 30 questions; the certified columns count recalls out of 120 under the rule
below and under its penalty-margin form. Scores are recomputed in 32-bit arithmetic in the engine's order.

The first two rows equal the measured no-profile and candidate arms. At r = 0.667 with no `local_factor`, the own bank
gains as much as `local_factor` 0.7 gives, and the shared-pool bank stays within 0.001 of no profile. The last four
rows scale further and gain more on the own bank, but leave recalls uncertified even under the rule, so the replay
does not establish them.

**Population.** The sample is one reader's 30 own-namespace questions and 30 shared-pool questions, asked twice against
one frozen copy of one deployment's store. The claim is about that reader on that store and nothing wider. Answers in a
third namespace have no bank on this store, as under Amendment 4, so the replay makes no claim about third-namespace
rows, which this factor lowers.

**Certification rule (verbatim; the live arm is compared against it).** A replayed top ten is certified when the
recall asked with `min_score` 0 and no `score_floor`, the no-profile arm returned all 50 requested hits with no
character-budget cut, and the replayed tenth score is strictly greater than the largest multiplier the rule applies to
any namespace times the smallest `rank_score` among those 50 hits, both sides in 32-bit arithmetic. A recall that
fails the rule is reported as uncertified and is not counted for or against the rule.

What a certified recall establishes, and the one condition it rests on. With `min_score` 0 and no `score_floor`, first
admission keeps every scored candidate in both arms, since a factor in (0, 1] never makes a score negative. Supersedes
suppression then drops the same candidates in both, with one exception: a `supersedes` property holding an abbreviated
id suppresses the first candidate in rank order whose id starts with it, so where two candidates share that prefix the
factor can change which one is dropped. A candidate scored but not returned was cut by the sort and
`limit`, so its `rank_score` was at most the smallest returned one; no multiplier exceeds the largest, and the strict
comparison leaves no tie for the id order to break, so it cannot reach the tenth score while its duplicate penalty is
unchanged. Candidates the retrieval stage never produced are absent from both arms alike. Two conditions remain:
no duplicate penalty changes copy, and no abbreviated `supersedes` value matches two candidates. The engine picks the unpenalized copy of each duplicate pair after the factors
(Amendment 4 Change 2, step 3), so a rule that reorders a pair moves the penalty, by at most the configured amount (0.1
by default), and a returned record cannot show whether it happened: a pair whose second copy was not returned, or was
suppressed, is invisible to it. The same holds for an abbreviated `supersedes` value, since the record holds the
returned ids only; the prefix is at least 8 hexadecimal digits, so two of a recall's candidates sharing it is unlikely
but not excluded. The penalty-margin column counts recalls that the rule certifies whose tenth score also
clears the bound plus the penalty, with no penalized hit among the 50. It rules out an unreturned copy that loses its
penalty and enters; it does not rule out a returned hit that gains one.

Two measurements bear on the conditions on this store: 6 of the 6,000 no-profile hits carry a penalty, and the replay of
`local_factor` 0.7 reproduces the candidate arm's served top ten in all 120 recalls, including the 40 the margin form
does not count. The replay table is therefore an estimate that held exactly at one setting. The live arm below is the
measurement, and its comparison with the replay is the test of the conditions at the proposed setting. Recording, for each recall, the duplicate groups over all
scored candidates and the candidate each abbreviated `supersedes` value resolved to would remove both conditions; this
amendment does not add that.

With a positive `min_score` or `score_floor` the rule does not apply: a factor can move a row below first admission,
and a row it suppressed through its `supersedes` property can then return.

### Change: an optional `own_namespace` and `outside_factor` on the profile

`brain.create_profile` accepts an optional pair: `own_namespace`, a namespace name validated by the same parser as every
other namespace argument (`Namespace::parse`, so an empty or malformed name is refused at creation rather than stored to
match nothing), and `outside_factor`, a finite number in [0.5, 1]. A request giving one without the other is refused. Both take the same storage, persistence,
`brain.profile` reporting and immutability as `local_factor` (Amendment 4 Changes 2 and 3), and the factor is rounded
to a 32-bit float when applied. A record without them loads as factor 1, an exact no-op, so every existing profile
ranks as it does today.

When such a profile serves a recall, each candidate whose namespace is not `own_namespace` has its rank score
multiplied by `outside_factor`, and a candidate in `own_namespace` is multiplied by 1. A `local` candidate is
multiplied by both `local_factor` and `outside_factor`, unless `own_namespace` is `local`, in which case
`outside_factor` scales every other namespace and `local_factor` applies to `local` as Amendment 4 defines. As with
`local_factor`, the factor depends on the serving profile alone, whoever the caller is and whatever the request's
namespace. The profile serving a reader is chosen by Amendment 4's bindings, so a profile naming the reader's own
namespace is the one bound to that reader.

Placement is Amendment 4 Change 2's: the factor multiplies where the entity term and `local_factor` apply, before the
first `min_score` admission and before the duplicate penalty, supersedes suppression, the second admission, the sort,
`limit` and the character budget. `min_score` and `score_floor` filter the adjusted score, so under a positive
`min_score` more outside rows fall below it; their help text says so. The returned `score` is taken before any factor
and is unchanged.

**Why a factor at most 1, rather than a boost on own rows.** In exact arithmetic and before any threshold, a boost g on
the profile's own rows orders candidates the same way as the factor 1/g on every other row. The two differ where a
score is compared with a fixed amount (the duplicate penalty, and `min_score` or `score_floor`, whichever applies), and
where 32-bit rounding ties two scores and hands their order to the id. `rank_score` is not bounded by 1 today: the
composite before the entity term is clamped to [0, 1], and the entity term lies in [0.85, 1.15], so `rank_score` lies
in [0, 1.15]. A boost would raise that ceiling, and Amendment 4 Problem 1 measured what a score at a clamp does to
ordering. A factor at most 1 never raises a `rank_score` above its unscaled value, so it keeps every score inside
today's range and adds no clamp. The replay computed in both forms gives the same metrics, and the same counts under the
rule, in every row above. The floor of 0.5 bounds how far an outside row can fall, for the same reason as `local_factor`'s.

With `include_breakdown=true`, each hit's `breakdown` adds `outside_factor`, the factor applied to the hit (1 when none),
and a returned hit's `rank_score` equals
`pre_entity_score × entity_term × local_factor × outside_factor − duplicate_penalty`, in 32-bit arithmetic in that order.
The factor reorders candidates the request already retrieved and was permitted to read: it adds no retrieval
candidate, widens no visibility, writes no posterior and is not authorization. Like any ranking change it changes
which retrieved candidates are returned, and under a positive `min_score` that includes a candidate another row's
`supersedes` property no longer suppresses, because that row fell below admission. Each hit already reports its
namespace, so the breakdown discloses nothing new about it. Naming a namespace in `own_namespace` grants nothing on it:
rows the caller cannot read are never candidates, so a profile naming such a namespace scales every candidate it
serves.

### Live arm and comparison with the replay

The candidate is a new profile created with `own_namespace` `lambda:khive`, `outside_factor` 0.667 and no
`local_factor`, measured by Amendment 4
Change 4's gate unchanged: same arms, banks, recall shape, two rounds, and its pass fixed before the run. The live run
uses a fresh copy of the same frozen store, so both arms see the same candidate sets as the replay.

In addition, the report compares the live candidate arm with this amendment's replay at the same setting, recall by
recall, and states three counts: recalls the rule certifies whose live top ten equals the replayed top ten in order;
certified recalls that differ; and uncertified recalls that differ. Any certified recall that differs means the replay
does not describe the engine at this setting (a moved duplicate penalty is the known way). The report names each one,
and the replay table is withdrawn until the difference is explained. The gate's pass or fail stands on the live
measurement alone.

### Alternatives considered

| Alternative                                                 | Disposition                                                                                                                                                                                                                                                                |
| ----------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A boost above 1 on the reader's own rows                    | Rejected: it can lift `rank_score` above 1 and needs a clamp, which Amendment 4 Problem 1 showed collapses ordering at the top.                                                                                                                                            |
| Lower `local_factor` further                                | Rejected: it moves the own-bank gain and the shared-pool cost together; at 0.5 the replayed shared-pool MRR@10 falls to 0.294 (92 of 120 recalls certified).                                                                                                               |
| A factor on third-namespace rows only, leaving `local` at 1 | Not chosen: no bank on the measured store has third-namespace answers, so that stratum cannot be measured; the measured gain comes from scaling `local` and other rows together relative to the reader's own.                                                              |
| Key the factor on the request's primary namespace           | Rejected: the measured gate recalls with request namespace `local` while the favoured rows are `lambda:khive`, so a request-keyed rule would measure a different rule from the one replayed; and a ranking that changes with the caller departs from Amendment 4 Change 2. |
| Learned per-namespace multipliers                           | Deferred for the reasons Amendment 4 gives.                                                                                                                                                                                                                                |

### Validation

The implementing change carries tests for: a profile without the pair returning results identical to no factor;
`brain.create_profile` refusing an `outside_factor` that is not finite or lies outside [0.5, 1], and refusing either
field without the other, and refusing an empty or malformed `own_namespace`; `brain.profile`, `brain.reset` and a snapshot reload keeping both fields; rows in
`own_namespace` unscaled and every other namespace scaled; a `local` row scaled by the product of both factors; a
profile whose `own_namespace` is `local` scaling the other namespaces' rows only by `outside_factor`; the same profile
ranking identically for two callers with different request namespaces over the same candidate set; the factor applied
the same way whether the profile is named by `profile_id` or resolved through a binding; a positive `score_floor` overriding `min_score` on the adjusted score; an active
weighted reranker, the factor applying after it; two scores that tie after 32-bit rounding ordered by id; a row lowered
below a positive `min_score` releasing the candidate its `supersedes` property named; an abbreviated `supersedes` value
shared by two candidates whose order the factor reverses; a duplicate pair whose order the
factor reverses moving the penalty, the zero floor on the penalty included; an entity term above 1 with both factors
at 1; a snapshot written before this amendment loading with factor 1; the
returned `score` unchanged by the factor; `min_score` applied to the adjusted score; and each returned hit's
`rank_score` equal to the five-term expression from its breakdown, with and without a duplicate pair present. The gate
harness records `outside_factor` among the per-hit terms and refuses a response missing it, and its replay reports as
uncertified any recall that returned fewer hits than requested or was cut by the character budget.
