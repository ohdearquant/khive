# khive-pack-brain Design

## ADR Compliance

### Brain Pack (ADR-032)

The brain pack implements profile-oriented Bayesian auto-tuning over the shared event log.

**Profile registry**: `BrainState` holds the profile registry and lifecycle metadata. Posteriors
live inside each profile's own state, opaque to brain core. This means the brain pack has no
knowledge of the internal structure of profile state — it stores opaque snapshots.

**Balanced-recall-v1 profile**: The built-in default profile uses three Beta posteriors:

- `relevance_weight` — prior $\text{Beta}(7, 3)$: warm-starts expecting 70% retrieval relevance
- `salience_weight` — prior $\text{Beta}(2, 8)$: conservative; salience rarely drives recall
- `temporal_weight` — prior $\text{Beta}(1, 9)$: pessimistic; temporal recency is a weak signal

Posterior mean: $\mu = \alpha / (\alpha + \beta)$

**Feedback signal taxonomy** (`FeedbackSignal` + `FeedbackEventKind`):

| Signal | Update magnitude | Posterior effect |
|--------|-----------------|-----------------|
| `useful` / `explicit_positive` | 1.0 / 1.5× | $\alpha += w$ |
| `not_useful` / `explicit_negative` | 1.0 / 1.5× | $\beta += w$ |
| `wrong` / `correction` | 1.0 / 2.0× | $\beta += w$ (+ relevance $\beta$ for correction) |
| `implicit_positive` | 0.5× | $\alpha += w$ |
| `implicit_negative` | 0.5× | $\beta += w$ |

**Profile lifecycle DAG**: `Active ↔ Inactive → Archived`. Archived is terminal. Active profiles
must go through Inactive before archiving. `brain.reset` is only valid on non-archived profiles.

**Profile resolution** (`brain.resolve`): Longest-match wins — actor + namespace + consumer_kind
scores higher than actor + consumer_kind, which scores higher than consumer_kind alone.
Archived profiles are filtered out before scoring. The `balanced-recall-v1` profile is the
system-default fallback for `consumer_kind="recall"` when no explicit binding matches.

**Event interpretation** (`event::interpret`): The `brain.feedback` verb is the
`FeedbackExplicit` event emitter. `brain.emit` predates this design and its log entries are
treated as `Irrelevant` to avoid spurious updates during event replay.

### Section Posteriors (ADR-048)

Section posteriors track per-section relevance weights within a knowledge atom. Each profile
maintains a `SectionPosteriorState` keyed by `SectionType` (10 canonical types: Overview,
CoreModel, BoundaryConditions, Formalism, OperationalGuidance, Examples, FailureModes,
ExpertLens, References, Other).

**Default priors per section type**:

| Section | $\alpha$ | $\beta$ | Mean |
|---------|---------|---------|------|
| Overview | 2 | 2 | 0.50 |
| CoreModel | 4 | 2 | 0.67 |
| BoundaryConditions | 2 | 3 | 0.40 |
| Formalism | 1.5 | 4 | 0.27 |
| OperationalGuidance | 6 | 1.5 | 0.80 |
| Examples | 5 | 2 | 0.71 |
| FailureModes | 3 | 2 | 0.60 |
| ExpertLens | 3 | 2 | 0.60 |
| References | 2 | 2 | 0.50 |
| Other | 2 | 2 | 0.50 |

**Thompson sampling** (explore mode, `exploration_epoch > 0`):

$$\tau = \tau_0 \cdot \frac{\text{exploration\_epoch}}{\text{DEFAULT\_EXPLORATION\_EPOCH}}$$

$$\theta_i \sim \text{Beta}(\alpha_i, \beta_i) \quad \text{(Gamma-ratio method)}$$

$$w_i = \text{softmax}(\theta_i / \tau), \text{ then floored at } w_{\min} \text{ and renormalized}$$

**Deterministic weights** (exploit mode, `exploration_epoch == 0`):

$$w_i = \text{softmax}(\mu_i / \tau_{\text{exploit}}), \text{ floored and renormalized}$$

where $\mu_i = \alpha_i / (\alpha_i + \beta_i)$, $\tau_{\text{exploit}} = 0.1$.

**ESS cap**: Evidence is capped at `DEFAULT_ESS_CAP = 100` to prevent overconfidence.
When $\text{ESS} = \alpha + \beta > \text{cap}$:

$$\text{scale} = \frac{\text{cap} - \text{prior\_ess}}{\text{ESS} - \text{prior\_ess}}$$

$$\alpha' = \alpha_{\text{prior}} + (\alpha - \alpha_{\text{prior}}) \cdot \text{scale}$$

$$\beta' = \beta_{\text{prior}} + (\beta - \beta_{\text{prior}}) \cdot \text{scale}$$

**Merge formula** (combining evidence from two observers sharing the same prior):

$$\text{Beta}(\alpha_1 + \alpha_2 - \alpha_{\text{prior}},\; \beta_1 + \beta_2 - \beta_{\text{prior}})$$

## Consistency Notes

- `brain.emit` log entries from before the `brain.feedback` rename are treated as
  `Irrelevant` by `event::interpret` to prevent spurious posterior updates during replay.
- `sync_balanced_recall_record` must be called on both the `handle_feedback` path and the
  `on_dispatch` hook path to keep `profile_record.total_events` in sync with the live
  `balanced_recall.total_events`. Removing either call causes profile record drift (issue #356).
- Archived profiles are filtered out by `brain.resolve` before scoring so that stale bindings
  pointing at archived profiles do not block resolution of live lower-priority bindings (issue #357).
- `brain.feedback` validates `target_id` existence against the KG before folding, and validates
  `served_by_profile_id` lifecycle before updating (must be non-archived). Lifecycle check
  precedes the event-log append so rejected calls leave no trace in the log (issue C4, R3-1).
- `brain.unbind` requires at least one filter (`profile_id`, `actor`, `namespace`, or
  `consumer_kind`) to prevent accidental wipe of all bindings (issue C2).
- `brain.bind` rejects archived profiles to prevent creating unresolvable bindings (issue C3).
- `brain.create_profile` enforces a profile-id grammar: alphanumeric + hyphens only, no dots,
  underscores, or wildcards; leading/trailing whitespace is trimmed (issue R3-3).
