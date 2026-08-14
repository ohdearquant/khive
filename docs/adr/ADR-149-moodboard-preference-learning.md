# ADR-149: Moodboard Pairwise Preference Learning

**Status**: accepted\
**Date**: 2026-08-08\
**Authors**: khive maintainers

## Context

ADR-148 establishes byte-exact visual assets, an immutable Lattice descriptor identity, and
exact visual retrieval. It deliberately does not claim that cosine proximity is an aesthetic
judgment or that a visual embedding alone measures coherence. A useful curation loop needs a
small learned layer grounded in explicit interactions, while preserving the distinction between
human preference, conformal evidence, retrieval similarity, and any later board-level coherence
statistic.

Khive already provides the necessary durable boundaries: actor-attributed namespace tokens,
append-only events, artifact entities, and `BlobStore`. `lattice-fann` provides a compact governed
network representation and CPU inference. Its 0.9.0 `BackpropTrainer`, however, computes MSE; it
does not expose Bradley--Terry binary cross-entropy. Calling that trainer a pairwise logistic
learner would be mathematically false.

This ADR adds the smallest preference-learning slice that can be reproduced, calibrated, loaded
after restart, and refused when its provenance is wrong. It makes no state-of-the-art claim.

## Decision

### D1 — Four additive verbs amend ADR-148

The opt-in `moodboard` pack retains ADR-148's three visual verbs and adds four preference verbs:

| Verb                         | Contract                                                                                                                                                               |
| ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `moodboard.serve`            | Validate two scored visual-asset occurrences, randomize their displayed sides, and append an immutable serve record.                                                   |
| `moodboard.judge`            | Append one `left`, `right`, `tie`, or `abstain` judgment for the exact served occurrence pair.                                                                         |
| `moodboard.train_preference` | Snapshot one actor/board/descriptor/feature scope, enforce grouped support, fit and calibrate a deterministic logistic head, and publish a FANN-backed model artifact. |
| `moodboard.preference`       | Load an exact identity-bound calibrated model and run FANN inference for two scored occurrences.                                                                       |

The pack remains opt-in, keeps `REQUIRES = ["kg"]`, adds no base entity or note kind, and reuses
ADR-148's `artifact/moodboard_model` subtype. These verbs reject Khive's canonical
`actor_is_unattributed` predicate before accepting a payload, including anonymous fallback and an
explicit actor ID of `local`. Unattributed interactions cannot become training data or a serving
identity.

### D2 — Frozen ten-feature identity

Every occurrence carries exactly ten finite `float32` values in `[0,1]`, in this order:

1. `visual_local_max_similarity_01`
2. `visual_local_top3_mean_similarity_01`
3. `visual_local_mean_similarity_01`
4. `style_conformal_p`
5. `style_interval_width`
6. `local_support_fraction`
7. `local_effective_support_fraction`
8. `palette_compatibility`
9. `tone_compatibility`
10. `composition_compatibility`

The pair input is `left - right`, so each model coordinate is in `[-1,1]`. The canonical compact
schema JSON is:

```text
{"bounds":[0.0,1.0],"dtype":"float32","features":["visual_local_max_similarity_01","visual_local_top3_mean_similarity_01","visual_local_mean_similarity_01","style_conformal_p","style_interval_width","local_support_fraction","local_effective_support_fraction","palette_compatibility","tone_compatibility","composition_compatibility"],"pair_transform":"left_minus_right","schema_version":"moodboard.preference-features.v1"}
```

Its SHA-256 `feature_schema_id` is
`f691fc73bf9a50d72157e21601fa579caa707bf2c448df546c63e915b4e42175`.
Changing a name, order, dtype, bound, or transform requires a new schema version and digest.
Callers may send that digest as a fence to `serve` and `train_preference`; `preference` requires
it. A mismatch fails before inference or persistence.

The immutable learning scope is the tuple:

```text
(namespace, actor_kind, actor_id,
 board_entity_id, board_id,
 model_key, descriptor_fingerprint,
 feature_schema_id)
```

After Gate authorization, `board_entity_id` must resolve by globally unique UUID to a live
`artifact/moodboard` whose `properties.board_id` equals the supplied 64-lowercase-hex fingerprint.
Occurrence asset IDs likewise resolve globally to live `artifact/visual_asset` entities whose
attached `content_ref` equals the supplied live BlobStore reference. Handlers perform no inline
entity-namespace equality checks, per ADR-007 Rev 6. The scope namespace remains immutable event
and training attribution, not a by-ID visibility boundary. Descriptor fingerprints and report
digests are 64-lowercase-hex SHA-256 strings.

### D3 — Explicit serve and judgment provenance

`moodboard.serve` accepts exactly two `state="scored"` candidates, the scope, an upstream
`source_report_sha256`, and selection provenance:

- non-empty `policy_revision`;
- optional finite `pair_propensity` in `(0,1]`;
- optional `candidate_pool_sha256`.

Its optional `exposure` request object also records whether source ranks or a learned probability
were shown. The business object is deliberately not named `presentation`: ADR-045 (Amendment 2) reserves
both `presentation` and `presentation_per_op` for the outer request envelope, whose values control
wire rendering rather than experiment provenance. A shown learned probability requires a fully
validated `served_preference_model_id`; a model ID is rejected when no probability was shown.
Probability-exposed judgments remain auditable but are excluded from v1 training to avoid an
immediate self-confirming feedback loop. Durable v1 serve and judgment event payloads retain their
original internal `presentation` member so this request-contract repair does not rewrite or fork
immutable evidence.

The server generates a UUIDv4 `serve_id` and one UUIDv4 `result_occurrence_id` per candidate. It
computes:

```text
digest = SHA256("moodboard-side-v1" || NUL || serve_id raw UUID bytes)
swap_applied = (digest[0] & 1) == 1
```

and stores the revision, digest, swap decision, source candidate index, displayed side, asset,
content reference, rank, and frozen feature row in a `moodboard.serve_record` `Audit` event. The
event ID is the serve ID, its target is the board, and its aggregate is `moodboard_serve/serve_id`.
The response returns the exact displayed left/right occurrence IDs. The client cannot choose a
stored side assignment.

`moodboard.judge` reloads that event and requires the same namespace and actor plus exact displayed
left/right occurrence IDs. Choice is closed to `left | right | tie | abstain`. Reason codes are
closed and choice-compatible:

- decisive: `style | palette | tone | composition | other`;
- tie: `equally_good | equally_bad | other`;
- abstain: `insufficient_context | both_unacceptable | render_failure | other`.

An abstention reason is required. Optional `response_ms` is at most one hour. The judgment record
copies the complete immutable serve provenance into a `moodboard.judgment_record`
`FeedbackExplicit` event. Its ID is UUIDv5 with namespace
`8fc455de-533c-5d1d-9228-09b81ef18e33` and a name consisting of exactly the serve UUID's 16 raw
RFC 4122 network-order bytes, with no prefix or terminator. This namespace and byte framing are
persistent wire identity.
An exact retry returns `created=false`; any conflicting second judgment for the serve fails. Tie is
used only for indifference calibration. Abstain is counted only; it is never a negative label.

### D4 — Unordered-pair split and support gates

Training groups records by the unordered pair of sorted asset `content_ref`s. A pair, including a
later side-reversed presentation, can occur in only one split. For each pair:

```text
digest = SHA256(
  "moodboard-pair-split-v1" || NUL ||
  board_id || NUL ||
  descriptor_fingerprint || NUL ||
  feature_schema_id || NUL ||
  min(content_ref_a, content_ref_b) || NUL ||
  max(content_ref_a, content_ref_b) || NUL
)
bucket = big_endian_u64(digest[0..8]) mod 20
0..13 = train; 14..16 = calibration; 17..19 = test
```

Before optimization the exact scope must contain:

- at least 64 distinct decisive train pair groups;
- at least 16 distinct decisive calibration pair groups;
- at least 16 distinct decisive test pair groups;
- both displayed-side labels in every decisive split; and
- at least 16 distinct calibration tie pair groups.

Repeated decisive observations within one unordered-pair group share total weight one. Thus a
frequently served pair cannot dominate a split merely through repetition. A deterministic
training snapshot digest covers sorted matching judgment IDs, timestamps, and payloads. One
single-query actor judgment snapshot is bounded to 50,000 events, then filtered to the exact
scope; using one query avoids offset-page drift under concurrent appends. Test data is not consulted by fitting,
temperature selection, or tie-band selection.

### D5 — Logistic BCE is fitted locally, then FANN is the governed head

The learned model is zero-intercept Bradley--Terry logistic regression over the ten differences.
The pack initializes ten `float64` weights to zero and minimizes grouped weighted binary
cross-entropy plus `0.5 * 1e-2 * ||w||²`. Optimization is deterministic full-batch gradient descent
with Armijo backtracking, initial step 1, shrink 0.5, Armijo coefficient `1e-4`, at most 64
backtracks per step and 2,048 iterations. It stops on gradient infinity norm `1e-8` or relative
objective improvement `1e-12`; inability to converge or descend fails without publishing a
model. The provenance seed is exactly zero because the optimizer uses no random initialization.

The pack does **not** call `lattice_fann::BackpropTrainer` and does not describe MSE as
Bradley--Terry/BCE. After fitting, it converts the ten weights to finite `float32` and constructs
exactly one `lattice_fann::Layer`:

```text
10 inputs -> 1 output, Activation::Linear, bias exactly 0
```

That `Network` is the governed representation. Calibration and test metrics use logits from its
actual FANN `forward` path after `float32` materialization. The workspace and pack dependency pin
`lattice-fann = "=0.9.0"`.

Temperature is selected on decisive calibration groups by a deterministic 128-iteration
golden-section search over log-temperature `[-4,4]`. The indifference half-band is selected from
group-mean calibration margins `abs(p - 0.5)` to minimize equal-class balanced error between tie
and decisive groups; the smaller threshold wins an exact objective tie. Stored test metrics are
group-weighted decisive log loss, Brier score, accuracy, support counts, and tie detection rate
when test ties exist. They are measurements, not a quality claim.

### D6 — Two BlobStore objects and an authenticated model artifact

All preference graph identity and immutable learning provenance use
`pack.runtime().core()`: board and visual-asset validation, serve/judgment/model events,
`moodboard_model` lookup/create, and its `derived_from` edge therefore remain in the shared main
graph in a multi-backend deployment. The installed `BlobStore` capability is shared by the pack
and core handles. No preference verb writes descriptor vectors or pack-auxiliary vector tables;
those remain governed by ADR-148 on the pack-selected runtime.

Training serializes the exact FANN binary with `Network::to_bytes`, stores it in `BlobStore`, and
records both its BLAKE3 `content_ref` and SHA-256. It then serializes a deterministic JSON bundle
containing:

- the complete scope and feature-schema identity;
- training snapshot/split/support/optimizer/seed provenance;
- calibration values and test metrics;
- `lattice-fann` crate and binary-format identity;
- exact `10 -> 1 Linear, bias=0` architecture; and
- FANN blob reference and SHA-256.

The bundle is a second BlobStore object attached to an `artifact/moodboard_model`; its SHA-256 is
the model fingerprint. The model is linked `derived_from` its board. A pack-only immutable
`moodboard.model_record` `Audit` event binds actor, entity ID, bundle reference/fingerprint,
network reference/digest, and full scope. Generic KG properties are display mirrors and cannot by
themselves authenticate a serving model. Its event ID is UUIDv5 with namespace
`1dc2337e-b200-5bd1-824f-265311645c16` and a name consisting of the model UUID's 16 raw RFC 4122
network-order bytes, one NUL byte, then the bundle `content_ref`'s 64 lowercase ASCII hexadecimal
bytes. This namespace and byte framing are persistent wire identity. Training reloads the
just-published model and executes a neutral FANN forward before acknowledging it.

On load, the pack globally resolves and verifies the entity type, then verifies the exact
attributed bundle/event scope, both BlobStore BLAKE3 references, bundle and network SHA-256 values,
the model provenance event, support/calibration gates, FANN version, one-layer shape, Linear
activation, finite parameters, and exactly zero bias. FANN's binary parser
validates shape and exact length but accepts non-finite parameters, so the pack performs the
additional finite-parameter walk. The bundle and FANN blobs are capped at 1 MiB each.

`Network::forward` reuses mutable activation buffers. Serving therefore keeps no shared mutable
network instance: a validated network is cloned for each prediction. Concurrent calls use
independent buffers, and restart loads reconstruct the network from BlobStore bytes.

### D7 — Learned preference is not conformal evidence

`moodboard.preference` accepts a model ID, exact scope, `source_report_sha256`, and two distinct
scored occurrences. It returns:

- `prediction_kind = "learned_pairwise_preference"`;
- `probability_left_given_decisive` and its right complement;
- raw FANN logit and calibrated temperature;
- whether the probability lies inside the calibrated indifference band; and
- `conformal_evidence.state = "not_computed_by_this_verb"`.

The probability is conditional on a decisive human judgment. It is not a conformal p-value, a
retrieval score, or a board-coherence statistic. Although the frozen input vector includes an
upstream `style_conformal_p`, the learned output does not replace or merge with that evidence.
Wrong actor, namespace, board, descriptor, schema, asset/content identity, corrupt bytes,
non-finite input, or uncalibrated model fails closed; there is no fallback score.

### D8 — Deferred learning layers

LoRA visual adapters, online approximate-nearest-neighbor feedback indexes, multi-user pooling,
causal debiasing beyond exposure exclusion, and a board-level coherence estimator remain out of
scope. They require separate evidence and identity contracts. This slice establishes durable
interaction data and a reproducible conditional preference head on which those later decisions
can build.

## Alternatives Considered

| Alternative                                  | Why rejected                                                                                                                              |
| -------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| Treat cosine similarity as preference        | It has no interaction grounding and conflates retrieval geometry with human judgment.                                                     |
| Train with `BackpropTrainer` and call it BCE | Version 0.9.0 hard-codes MSE, so the claimed objective would be false.                                                                    |
| Store only JSON weights                      | It would not exercise or govern the requested FANN serialization and inference path.                                                      |
| Use individual-event random split            | Side-reversed or repeated unordered pairs would leak across train/calibration/test.                                                       |
| Fold ties into label `0.5`                   | It changes the decisive Bradley--Terry likelihood and hides the separate indifference decision.                                           |
| Treat abstain as right/negative              | It fabricates preference evidence from missing judgment.                                                                                  |
| Share one mutable FANN network behind a lock | It serializes otherwise tiny inference and makes buffer ownership implicit; per-call clones are explicit and cheap for eleven parameters. |

## Consequences

### Positive

- Every learned prediction is reproducible from explicit, immutable Khive events and exact model
  bytes.
- Unordered-pair grouping prevents the most direct evaluation leakage.
- FANN is used for the persisted model and actual inference without misrepresenting its MSE
  trainer.
- Tie, abstain, exposure, calibration, and test evidence remain distinct and inspectable.

### Negative

- The minimum publishable dataset requires at least 96 decisive pair groups plus calibration tie
  support, so early boards correctly remain untrained.
- V1 is actor- and board-specific; it does not pool sparse feedback across people or boards.
- Full-batch fitting and event snapshots are bounded rather than incremental.
- The linear head can express only monotone combinations and interactions already present in the
  frozen features.

## Verification

Required tests cover the feature-schema golden digest, deterministic side provenance, judgment
idempotency/conflict, anonymous rejection, side-swap probability symmetry, reversed-pair split
identity, support failure, both labels, tie/abstain exclusion, deterministic training and bytes,
FANN corrupt/wrong-shape/non-finite rejection, independent concurrent buffers, wrong scope/schema,
uncalibrated rejection, BlobStore/entity/event round-trip across runtime restart, and the complete
public `serve -> judge -> train_preference -> preference` path.
