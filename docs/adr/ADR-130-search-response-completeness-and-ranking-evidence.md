# ADR-130: Search response completeness and ranking evidence

- Status: accepted (2026-07-26)
- Date: 2026-07-26
- Depends on: ADR-006 (deterministic scoring), ADR-012 (retrieval composition),
  ADR-029 (substrate coordinator), ADR-033 (recall pipeline),
  ADR-045 (verb response presentation)

## Context

The `search` verb is served by two surfaces: the KG pack handler
(`crates/khive-pack-kg/src/handlers/search.rs`) and the MCP multi-backend path
(`crates/khive-mcp/src/server.rs`). Both emit the same response envelope, and two
of that envelope's fields have a most-natural reading that is false. Neither is a
documentation defect: the shape itself invites the wrong inference, so a caller
that reads the wire correctly still concludes the wrong thing.

### Defect 1 — a degraded read is indistinguishable from a clean no-match

The coordinator marks a fan-out partial when any backend leg reported an error:
`partial = per_backend.iter().any(|r| r.error.is_some())`
(`crates/kkernel/src/coordinator/service.rs:135`). The MCP layer recomputes the
failed-backend list, sorts and dedupes it, and ORs it into the same flag
(`crates/khive-mcp/src/server.rs:1401-1409`). It then builds a _successful_
envelope: `ok=true`, `tool`, `result`, and — only when partial — the two
degradation fields (`ok_envelope`, `crates/khive-mcp/src/server.rs:1517-1535`).

So a search whose only backend was unreachable returns

```json
{ "ok": true, "tool": "search", "result": [], "partial": true, "missing_backends": ["main"] }
```

while a genuine no-match returns `{"ok": true, "tool": "search", "result": []}`.
The distinction exists on the wire. The harm is that the obvious call-site
reading — `if response.ok && response.result.is_empty() { NoMatch }` — is
identical for both, and callers do write exactly that. A caller performing
duplicate detection decides on the negative: it concludes "no matching record
exists" and creates a duplicate, while the truth is that the question was never
answered. Frame-budget omission reinforces the framing by preserving `partial`
and `missing_backends` beside an omitted result as advisory metadata on a
success (`crates/khive-mcp/src/server.rs:2387-2400`).

Additive metadata cannot repair this. A caller that ignores `partial` today will
ignore `degraded` or `status` tomorrow; the false conclusion survives the new
field. The only reliable correction is to move the case onto the failure branch
that callers already inspect.

### Defect 2 — `score` is a rank artifact that reads as a similarity

`score` is serialized as `h.score.to_f64()` on both surfaces
(`crates/khive-pack-kg/src/handlers/search.rs:153,270`;
`crates/khive-mcp/src/server.rs:1431,1450`), and it does not carry one quantity.

- Reciprocal rank fusion structurally discards input relevance. The loop
  destructures every candidate as `(rank_0_indexed, (id, _score))` and
  contributes `rrf_score(rank_1_indexed, k)` regardless of the value it dropped
  (`crates/khive-fusion/src/rrf.rs:33-38`), with `DEFAULT_RRF_K = 60`
  (`crates/khive-fusion/src/strategy.rs:7`). Observed hybrid scores are therefore
  the ordinal sequence `1/(60+rank)`: 0.016393, 0.016129, 0.015873, 0.015625. An
  exact title match and pure noise both score 0.016393 when they land first.
- The `VectorOnly` and `KeywordOnly` strategies do not fuse at all. They select a
  source and return it untouched, and `passthrough_source` treats a lone source
  as authoritative regardless of the requested index
  (`crates/khive-fusion/src/fuse.rs:29-30,39-50`). Those hits carry raw backend
  relevance.
- The KG runtime's entity path fuses locally with `k=10` plus an exact-match
  boost and retains no backend evidence on the hit
  (`crates/khive-runtime/src/retrieval.rs:1222-1280`).

One wire field therefore carries an ordinal under one strategy and a magnitude
under another, with no way for the caller to tell which. Callers threshold it,
compare it across queries, and render it as a percentage match. `min_score` is
applied against whichever quantity happens to flow: it filters
`h.score.to_f64() >= score_floor` on both surfaces
(`crates/khive-pack-kg/src/handlers/search.rs:136-139,254-257`;
`crates/khive-mcp/src/server.rs:1339-1343,1425,1444`), so the same threshold
means different things on different requests.

### Existing authority

ADR-006 requires deterministic fixed-point scoring; raw backend floats may not
be carried into decision-bearing paths. ADR-012 owns retrieval composition.
ADR-029 owns the coordinator's partial-read contract and the
`partial`/`missing_backends` fields. ADR-033 owns recall-pipeline scoring and
threshold behaviour. ADR-045 owns presentation-time handling and lists the score
field names truncated in agent mode (`SCORE_FIELDS`,
`crates/khive-runtime/src/presentation.rs:394-402`); it governs rendering, not
producer semantics.

Both defects concern what a caller may infer from one envelope: whether its
candidate universe was complete, and what its numeric fields mean. Splitting
them across two authorities would force every consumer to compose two contracts
before interpreting a single response. This ADR takes both.

## Decision

### Scope

This ADR governs the KG `search` verb and the MCP multi-backend `search` path.
It does not change knowledge-pack normalization or squashing.

### 1. Completeness contract

For successful search responses:

- `ok` MUST be `true`.
- `status` MUST be present and MUST be either `complete` or `partial`.
- `status="complete"` MUST mean every selected backend leg completed
  successfully.
- `status="partial"` MUST mean at least one selected backend leg failed and at
  least one hit survived all server-side filters.
- `missing_backends` MUST be present and non-empty exactly when
  `status="partial"`.
- Legacy `partial=true` MUST be emitted during the compatibility release as an
  alias for `status="partial"`, and MUST be omitted for `complete`.

`status` is a field of the successful operation envelope, beside `result`, on
both governed surfaces — the KG `search` verb and the MCP multi-backend path
emit it identically. It is not a per-hit field and never appears inside the
hit array.

If any selected backend fails and no hit survives all server-side filters, the
operation MUST return:

- `ok=false`;
- no successful `result` field;
- `error.kind="search_incomplete"`;
- `error.retryable=false` — **amended: see Amendment 2**, which makes this
  conditional on every failed leg having timed out. `false` remains the default
  and the value required whenever that condition does not hold;
- `error.missing_backends` as a sorted, deduplicated, non-empty array;
- `error.message` stating that no-match was not established.

This includes the case where pre-filter fusion found hits but `min_rank_score`
removed all of them. Completeness is a property of the answer _after_ the
requested filter, because that is what the caller observes.

### 2. Ranking and evidence fields

Each hit MUST expose:

- `rank_score`: the deterministic value used to order that response under the
  selected strategy. It is strategy-local and ordering-only, and MUST NOT be
  described as a probability, a percent match, a calibrated relevance, or a
  cross-query comparable quantity.
- `rank_score_kind`: a closed discriminator over the initial taxonomy `rrf`,
  `vector`, `keyword`, `weighted`, `union`.
- `signals`: an object containing only named component evidence actually
  available for that hit. Initial keys MAY include `vector_similarity` and
  `keyword_score`. Absent evidence MUST be omitted and MUST NOT be synthesized
  as zero.

The per-hit requirements above bind the canonical successful envelope. A hit
with no retained component evidence carries `signals: {}` canonically;
Agent-mode presentation drops the empty object under ADR-045's existing
empty-value rules, and that omission is not a violation of this contract.

During one compatibility release, `score` MUST equal `rank_score` bit-for-bit
after deterministic-to-wire conversion. `score` MUST be documented as deprecated
and ordering-only, and MUST NOT preserve the old
passthrough-versus-fusion semantic split.

A scalar `relevance` is deliberately not introduced: vector similarity, keyword
score, and fused rank are different measures over different domains, and
collapsing them into one nullable number relocates the ambiguity rather than
removing it.

### 3. Threshold rule

`min_rank_score` filters each candidate against the same deterministic
`rank_score` value used to order the final response, after fusion and
exact-match boosts and before response truncation. The comparison is inclusive:
retain a candidate iff `rank_score >= min_rank_score`. The threshold is
strategy-local and MUST NOT be interpreted across strategies or across queries.

For one compatibility release, `min_score` is a deprecated exact alias for
`min_rank_score`. Supplying both MUST return an invalid-argument error, even
when the two values are equal. The server MUST NOT reinterpret `min_score` as
`vector_similarity`, `keyword_score`, or any other component signal.

Future typed filters, if needed, require explicitly named parameters such as
`min_vector_similarity`. They are not part of this decision.

### 4. Determinism fence

All rank and component evidence that affects filtering, ordering, deduplication,
or equality MUST be converted into the ADR-006 fixed-point `DeterministicScore`
domain at the backend boundary. Fusion MUST operate on fixed-point values. Wire
floating-point values are presentation projections only and MUST NOT be read
back for decisions. This ADR refines ADR-006 by naming where the conversion
happens; it does not relax deterministic fixed-point scoring.

### 5. Knowledge-pack exclusion

The knowledge pack performs its own post-fusion normalization and squashing
(`normalize_rrf_score`, `crates/khive-pack-knowledge/src/knowledge/search.rs:49-84`;
squash multiplier, `:151-154`). Knowledge search MUST remain behaviourally
unchanged by this ADR and MUST NOT be mechanically migrated to `rank_score` or
to typed signal filtering. The knowledge pack MAY adopt the field vocabulary
only through a separate amendment that preserves its current ranking and
threshold behaviour.

### 6. Retry contract

Callers MUST NOT automatically retry `search_incomplete`. Opt-in retries MUST
pass backoff and circuit-breaker admission before reissuing. Retry
documentation MUST state this, because a generic "retry every `ok=false`" client
would otherwise multiply load against exactly the backend that is already
failing: ten callers issuing five searches per second, each retrying three
times, turn 50 logical searches per second into as many as 150 attempts per
second for the duration of the outage. `error.retryable=false` reduces that risk
but cannot eliminate it for clients that ignore the field.

**Amended: see Amendment 2.** The prohibition on automatic retry is narrowed to
the cases where a retry cannot succeed, and the admission requirements above are
promoted from a condition on opt-in retries to a precondition for advertising
retryability at all. The arithmetic in this section is not disputed by that
amendment; Amendment 2 §3 addresses it directly, including why the concession in
the last sentence above is load-bearing against this section's own conclusion.

## Wire examples

### Degraded-empty

**Amended: see Amendment 2 §5.** This example types a leg as `backend_error`
while its own message reports a timeout, so as written it does not survive
Amendment 2's two-value `kind` vocabulary. Amendment 2 §5 carries the corrected
form for the all-timeout case; the example below remains correct for a genuine
`backend_error`, with a message that does not describe a timeout.

```json
{
  "ok": false,
  "tool": "search",
  "error": {
    "kind": "search_incomplete",
    "message": "No-match was not established because selected backends failed.",
    "retryable": false,
    "missing_backends": ["main"],
    "backend_errors": {
      "main": {
        "kind": "backend_error",
        "message": "backend search timed out after 5000ms"
      }
    }
  }
}
```

### Clean no-match

```json
{
  "ok": true,
  "tool": "search",
  "status": "complete",
  "result": []
}
```

### Hybrid hit

```json
{
  "ok": true,
  "tool": "search",
  "status": "complete",
  "result": [{
    "id": "4534fe50",
    "title": "Cormack et al. 2009 RRF Paper",
    "rank_score": 0.0325224749,
    "rank_score_kind": "rrf",
    "signals": {
      "vector_similarity": 0.842,
      "keyword_score": 18.375
    },
    "score": 0.0325224749
  }]
}
```

`score` appears only as the compatibility alias.

### Single-source hit

```json
{
  "ok": true,
  "tool": "search",
  "status": "partial",
  "missing_backends": ["text"],
  "backend_errors": {
    "text": {
      "kind": "backend_error",
      "message": "text index unavailable"
    }
  },
  "partial": true,
  "result": [{
    "id": "797e929c",
    "title": "Hybrid Search with RRF",
    "rank_score": 0.8125,
    "rank_score_kind": "vector",
    "signals": {
      "vector_similarity": 0.8125
    },
    "score": 0.8125
  }]
}
```

The value remains ordering-only even where it happens to equal the vector
component.

## Compatibility

The compatibility window is two releases, fixed here rather than left as "one
release":

- **Release N = v0.8.0.** Emits deprecated `score == rank_score` on every hit,
  accepts deprecated `min_score` as an exact alias for `min_rank_score`, and
  MUST emit `partial=true` beside every `status="partial"` success while
  omitting it for `complete`. Degraded-empty moves to the failure branch in
  this release; it is not deferred.
- **Release N+1 = v0.9.0.** Removes `score`, `min_score`, and the `partial`
  boolean. A caller still sending `min_score` receives an explicit
  unsupported-field error rather than a silent reinterpretation; a caller still
  reading `score` finds it absent rather than redefined.
  **Amendment 2's wire changes ship in this release** — the two-value
  `backend_errors[].kind` vocabulary and `retry_after_ms`. They are not eligible
  for v0.8.0: Amendment 1 shipped in v0.8.0 with `kind` fixed to the single
  constant `backend_error`, so a strict reader may have pinned that value.
  Widening a closed vocabulary that a released reader validates is a breaking
  change for that reader, and takes the next release rather than a point update.
  Amendment 1 could be introduced additively; Amendment 2 cannot.

Per consumer class:

**KG pack serializer.** MUST add `rank_score`, `rank_score_kind`, and `signals`
to every hit, and its successful responses MUST carry `status` on the operation
envelope. MUST emit `score == rank_score` through release N. MUST accept
`min_score` only as the exact alias above. Verified by serializer tests covering
every strategy and asserting alias equality, the presence of all three ranking
fields, and envelope `status`.

**MCP multi-backend search.** MUST add `status` to every success. MUST convert
degraded-empty-after-filtering into the typed error. MUST preserve `status`,
`missing_backends`, `backend_errors`, and the typed degradation error under
frame-budget omission.
Verified by tests for complete-empty, partial-with-hit, backend-failure-empty,
and post-threshold degraded-empty.

**MCP envelope builder and frame-budget omission.** MAY continue using the common
`ok` envelope builder. MUST NOT construct a successful empty result for an
incomplete search. MUST retain `status` on successful omission and the full,
small `search_incomplete` error metadata on failure omission.

**Coordinator and coordinator tests.** MAY continue reporting per-backend errors
and merged hits internally. MUST expose enough evidence to distinguish backend
failure from filter-produced emptiness. Existing partial-with-hit tests remain
valid with `status` added; new tests MUST pin degraded-empty as `ok=false`.

**Runtime retrieval and fusion.** MUST replace tuple-only score plumbing with a
typed evidence carrier where signals must survive fusion. MAY retain
strategy-specific internal implementations. MUST NOT reintroduce floating-point
comparison, ordering, or accumulation. Verified by deterministic golden tests
across repeated runs and input-order permutations.

**Runtime presentation.** MUST add `rank_score`, `vector_similarity`, and
`keyword_score` to the score truncation policy, including nested `signals.*`
keys. MUST truncate for presentation only after filtering and ordering
decisions. MUST keep `score` truncation for its compatibility window.

**Knowledge search.** MUST remain behaviourally unchanged. Verified by
before/after golden results over the existing normalization and squash paths.

**Schema, help, and downstream JSON callers.** Help and schema MUST describe
`rank_score` as ordering-only and `signals` as typed evidence. Tolerant readers
receive additive fields in v0.8.0 but must migrate off `score`. Strict readers
need schema updates in v0.8.0. Generic error-retry callers MUST honour
`error.retryable=false`. Verified by the release-gated schema fixtures and the
retry-policy row in Verification. **Amended: see Amendment 2** — callers must
now honour the field's value rather than a constant, and a caller that acts on
`retryable=true` must also honour `retry_after_ms`.

## Supersession

- Supersedes ADR-029 for the externally visible search degradation envelope as
  a whole: the empty-partial outcome (now the `search_incomplete` failure), the
  partial-with-hit signal (now per-operation `status="partial"` with
  `missing_backends`), and the retirement of the `partial` boolean at v0.9.0.
  The coordinator's internal partial-read model, fan-out, and per-backend error
  reporting stand unchanged.
- Supersedes ADR-033 only for `search` threshold naming and comparison. The
  recall pipeline's own scoring and thresholds are untouched.
- Refines ADR-006 by defining the backend-boundary conversion fence. It does not
  relax deterministic fixed-point scoring; it names where the fence sits.
- Amends ADR-045 in two narrow respects. First, by field list: the new score
  field names are added to the presentation truncation policy. Second, by error
  shape: ADR-045's canonical error envelope carries a string `error`; for
  `search_incomplete` the `error` field is the structured object specified
  above, and ADR-045's no-transform rule applies to it unchanged — presentation
  MUST pass the typed object through untransformed in every mode, including
  frame-budget omission. ADR-045 retains full authority over presentation
  behaviour; this ADR does not acquire any share of it.

## Consequences

- A clean no-match becomes distinguishable using the `ok` branch that callers
  already inspect, with no new field for a caller to remember to read.
- Degraded-empty becomes a failure. Callers that treated it as an empty result
  will see errors where they previously saw silence; that is the intended
  correction, and it is a breaking change in v0.8.0.
- A residual remains on `status="partial"` with hits: the response is `ok=true`,
  so a caller whose decision requires a complete candidate universe — duplicate
  detection by absence, create-on-not-found — MUST treat `status="partial"` as
  inconclusive for the absent case. The absent record may live in a failed
  backend. This protection is caller-side by construction, so the guidance
  belongs in help and schema text, not only in this document.
- Ranking evidence becomes explicit, at the cost of cross-crate plumbing through
  retrieval, fusion, and serialization. This is not a serializer-only change.
- One compatibility release carries duplicated score spelling; the duplication
  ends at v0.9.0 by schedule, not by discretion.
- Component signals remain backend-scoped and model-scoped. Field documentation
  MUST state the applicable backend and model scope; a `vector_similarity` from
  one embedding model is not comparable with another's.
- Exact-match boosts affect `rank_score` only, and MUST NOT be folded into
  `vector_similarity`. Whether a boost becomes its own named signal is left to a
  later amendment.
- The per-operation `status` field defined here is distinct from the batch-level
  `status` already present on the multi-operation response
  (`crates/khive-mcp/src/server.rs:877-888`), which reports `success` or
  `partial` over the operation set. The separation is structural, not only
  documentary: batch `status` takes values in `{success, partial}` and appears
  only at the response top level; per-operation `status` takes values in
  `{complete, partial}` and appears only inside a successful search operation's
  own entry. No schema, renderer, or future envelope change may flatten or
  merge the two. Help and schema text MUST keep the vocabularies visibly
  separate.

## Rejected alternatives

- **Additive `degraded` object on a successful response** — ignored metadata
  leaves the false no-match conclusion intact.
- **A successful `status` discriminator alone** — legacy callers do not branch on
  a new field; retained here for complete-versus-partial discrimination, but not
  sufficient as the safety fix.
- **Caller helper plus documentation, no wire change** — relies on every caller
  voluntarily abandoning the obvious `ok && empty` reading.
- **Rename `score` to `rank_score` only** — honest but incomplete; it does not
  supply the typed source evidence callers need for similarity gating.
- **Add `score_kind` beside the unchanged `score`** — labels the trap while
  preserving it.
- **`score` plus a scalar `relevance`** — "relevance" is not one comparable
  quantity across vector, keyword, and hybrid retrieval.
- **Single-source passthrough of raw relevance** — makes field meaning depend on
  runtime backend availability, recreating the present defect.
- **Normalizing RRF into a `[0,1]` range** — an ordinal rendered in a
  similarity-shaped interval adds no evidence and makes percentage language more
  tempting.
- **Independent amendments to ADR-006, ADR-029, and ADR-033** — consumers would
  need to compose several authorities to interpret one envelope.

## Verification

1. Contract tests for all four wire examples above, in both verbose and agent
   presentation modes.
2. A degraded-empty test where backend failure produces zero candidates.
3. A degraded-empty test where surviving candidates are all removed by
   `min_rank_score`.
4. Strategy matrix tests proving `score == rank_score` during the compatibility
   release and correct `rank_score_kind` per strategy.
5. Determinism tests with reordered source input, repeated runs, and fixed-point
   component conversion.
6. Schema and help snapshots, plus strict and tolerant JSON consumer fixtures.
7. Knowledge-pack before/after golden results proving no semantic drift.
8. Retry-policy tests proving `search_incomplete` is not retried by default;
   generic error-retry client behaviour is covered by this row. **Amended: see
   Amendment 2 §6** — the default is unchanged, but a test asserting
   `retryable` is *always* `false` now contradicts the record; assert the
   default and the all-timeout case separately.
9. Compatibility-alias contract tests: `partial=true` present beside every
   `status="partial"` success and absent on `complete` in v0.8.0.
10. Frame-budget omission tests in verbose and agent modes asserting the full
    typed `search_incomplete` metadata survives omission untransformed.
11. Release-gated schema fixtures: v0.8.0 fixtures for strict readers including
    the degraded-empty error; v0.9.0 fixtures proving `score`, `min_score`, and
    `partial` are absent on emit and `min_score` is rejected with an
    unsupported-field error. **Amended: see Amendment 2 §7** — the v0.9.0
    fixtures additionally pin `backend_errors[].kind` accepting both `timeout`
    and `backend_error`, and `retry_after_ms` present on every `retryable=true`
    error. The v0.8.0 strict-reader fixture keeps `kind` pinned to the single
    constant, since that is the contract that release shipped; the two fixture
    sets are expected to disagree on this field, and a change making them agree
    is a defect in one of them.
12. A batch fixture containing one partial search plus one other operation,
    asserting batch-level and per-operation `status` remain distinct.
13. An Agent-mode fixture for an evidence-free hit, asserting `signals` is
    `{}` in canonical form and dropped in Agent presentation while
    `rank_score` and `rank_score_kind` survive.
14. Partial-success and degraded-empty fixtures asserting exact retained
    backend/cause parity, deterministic bounded truncation, credential masking,
    and frame-budget preservation of `backend_errors`.

## Amendment 1 (2026-08-11): bounded per-backend failure evidence

Every incomplete multi-backend search MUST carry `backend_errors`, an object
keyed by exactly the same sorted backend ids retained in `missing_backends`.
Each value is `{kind: "backend_error", message: <captured cause>}` — **amended:
Amendment 2 replaces this single constant with a closed two-value vocabulary**.
A partial
success carries it beside `result`; degraded-empty `search_incomplete` carries
it inside `error`. Complete searches omit it. Presentation and frame-budget
omission preserve the diagnostics at the same location.

Diagnostics are mandatory but bounded: retain at most 16 causes and no more
than one fixed per-operation wire budget. Before exposure or warning, scan at
most 4,096 input scalar values from both backend ids and causes with the
canonical credential masker. Cap backend ids at 256 Unicode scalar values,
using a stable hash suffix whenever masking or truncation changes the displayed
id; a changed value carries `backend_id_masked=true` and a length-truncated
value carries `backend_id_truncated=true`. Cap cause messages at 1,024 scalar
values plus an ellipsis. Empty causes become
`backend search failed without diagnostic detail`. If any failed legs are not
retained, emit `backend_errors_truncated=true` and the exact
`backend_errors_omitted` count. At least one cause MUST survive whenever a leg
failed, and retained `missing_backends` and `backend_errors` keys MUST have
exact parity. The server logs each retained masked cause at warning level and a
bounded aggregate warning for omitted causes.

This amendment is additive. It does not change completeness, retry, filtering,
or ranking semantics; it supplies bounded evidence for the already-declared
degradation state.

## Amendment 2 (2026-08-29): cause classification and server-paced retry

Unlike Amendment 1, **this amendment does change retry semantics.** Decision §1
requires `error.retryable=false` unconditionally, and Decision §6 forbids
automatic retry. This amendment makes retryability conditional on cause. It
therefore has to answer Decision §6's argument rather than restate the benefit,
and §3 does that.

Section numbers in this amendment are ambiguous without a qualifier, because the
body and the amendment both number from 1. Throughout: **"Decision §N" means the
original section N under `## Decision`**; a bare "§N" means section N of this
amendment.

### 1. Cause classification

Amendment 1 fixes each `backend_errors` value to
`{kind: "backend_error", message: <captured cause>}` — a single constant, not a
vocabulary. `kind` becomes a closed vocabulary of exactly two values:

- `timeout` — the leg exceeded a deadline, whether the coordinator's outer
  fan-out deadline or a typed runtime deadline. Classification MUST be
  structural. It MUST NOT be derived by matching text in a rendered message.
- `backend_error` — every other failure.

Classification MUST be computed **before** Amendment 1's diagnostic truncation,
so an omitted cause cannot change it. Amendment 1's parity, masking, capping and
budget rules are unchanged and apply to both values.

This also corrects an inconsistency in this record's own degraded-empty wire
example, whose cause reads
`{"kind": "backend_error", "message": "backend search timed out after 5000ms"}`.
That is supporting evidence that the single-constant vocabulary was already
carrying two meanings; it is not the argument for changing retryability.

### 2. Conditional retryability

`error.retryable` MAY be `true` only when **every** retained and omitted failed
leg classified as `timeout`. Any mixed or non-timeout failure keeps it `false`.
Because the classification precedes truncation, this holds over the full failure
set rather than the retained sample.

When `retryable=true`, the error object MUST additionally carry
`retry_after_ms`, a positive integer. Clients MUST NOT reissue before it
elapses.

The field is mandatory rather than optional because §3's answer to the
amplification argument depends on the *server* naming the pace; an absent field
returns that decision to the client, which is the situation the retry contract
already distrusts. A surface with no better estimate MUST therefore emit a
documented default floor rather than omitting the field — "the server always
names a pace" is the property being relied on, and a floor satisfies it. The
default floor is a per-surface constant published with the retry policy required
by §4; deriving a sharper value from observed backend state is permitted and
preferred, and is what the §6 verification row asks for.

### 3. Why Decision §6's arithmetic does not forbid this

Decision §6 is correct that a naive client multiplies load against an already-failing
backend, and its arithmetic stands: ten callers at five searches per second,
retrying three times, can turn 50 logical searches per second into 150 attempts
per second. That figure is not disputed here, and one part of it is conceded
below: absent admission control, delays alone do not lower it. What follows is
why conditional retryability *under §4* does not leave that outcome standing.

**Backoff bounds the instantaneous burst, and only that.** Read as an
instantaneous rate, the 150/sec figure requires the three retries for one logical
request to be issued without delay, so that one request's retries stack on the
next request's first attempt. Under §4's mandatory backoff those attempts are
spread across the backoff window, which removes the burst. That disposes of the
*transient* case — a blip that resolves inside one backoff window, where the
retries land after recovery and cost the failing backend nothing.

**It does not dispose of the sustained case, and this amendment does not claim it
does.** Under a bounded budget with exponential backoff, every logical request
still issues its full allotment within a window far shorter than a sustained
outage, so with arrivals continuing at 50/sec the steady-state attempt rate
converges on Decision §6's figure regardless of the delays. Backoff moves the
ramp; it does not lower the plateau. The sustained case is answered by the
breaker below, not by this bullet.

What `retry_after_ms` contributes here is narrower than pacing away the volume:
it makes the *server* the party that sets the interval, which is what lets the
breaker's own recovery probe and the clients' reissue schedule agree instead of
being chosen independently by every caller.

**A boolean was never the load control.** Decision §6 concedes the decisive point itself:
`retryable=false` "reduces that risk but cannot eliminate it for clients that
ignore the field." The client in the amplification scenario is precisely a
client that retries every `ok=false` — that is, one ignoring the field. Against
that client the flag's value is inert, so the protection Decision §6 attributes to
`retryable=false` is unavailable exactly where it is needed. Meanwhile the cost
of the unconditional `false` falls entirely on **well-behaved** clients, the ones
that honour the field: they are told not to retry a timeout that would likely
have succeeded. The current contract's benefit lands on nobody and its cost lands
on the compliant.

**The breaker, not backoff, is the load control for a sustained outage — and it
makes that case strictly better.** Circuit-breaker admission opens after
consecutive timeouts and suppresses attempts *including first attempts*. That
last property is what distinguishes it from every retry-shaping rule: it removes
load the current contract cannot reach, because `retryable=false` governs only
reissues and says nothing about first attempts.

The reach of that claim must be stated exactly, because the breaker is a
client-side obligation. It bounds the **conforming** population only. For those
clients, during the sustained outage Decision §6 describes, offered load falls
**below** their share of the 50/sec baseline — strictly better than today, where
they contribute their full baseline and are merely forbidden to retry. The
non-conforming population is unchanged in both directions, per the inertness
argument above.

So no population is worse off and one is materially better off, which is the
whole of the argument. It rests on the breaker and the inertness of the flag, not
on the backoff bullet above.

**What this amendment does not claim.** It does not claim typed causes are more
accurate and therefore justify retrying; accuracy is true and does not answer
Decision §6. It does not claim backoff lowers the sustained-outage rate — the
second bullet concedes it does not. And it does not claim clients will comply,
nor that admission control reaches those who do not: the breaker is a client-side
obligation, so a client that ignores `retryable` ignores the breaker too. The
answer for that population is not enforcement but *inertness* — it behaves
identically before and after this amendment, because it already ignores
`retryable=false` today.

### 4. Admission control is mandatory, not advisory

`retryable=true` does not mean "retry now"; it means a retry could succeed. The
obligations below fall on two different parties, and the amendment is unsound if
they are read as one, so each is stated with its actor.

**The client that acts on `retryable=true`** MUST implement all three:

- **Bounded budget** — a maximum attempt count per logical request, documented
  by the client's retry policy.
- **Backoff** — exponentially increasing delays with jitter, never a fixed
  interval, and never shorter than the `retry_after_ms` the server supplied.
- **Circuit breaking** — consecutive timeouts open a breaker that suppresses
  further requests to that backend, **first attempts included**, until a probe
  succeeds. This is the clause that makes the outage case in §3 improve rather
  than merely hold: the breaker removes offered load that the current contract
  does not touch.

**The surface that emits `retryable=true`** MUST NOT do so unless it publishes,
in the retry documentation Decision §6 already requires, the budget, the backoff
schedule and the breaker threshold that a conforming client is expected to apply
— and MUST emit a `retry_after_ms` on every `retryable=true` response, at
minimum the default floor published with that policy. A surface that cannot
publish all three MUST keep `retryable=false`, which remains the default and the
safe value.

The asymmetry is deliberate. The server cannot enforce client backoff, so this
amendment does not claim it can. What the server controls is (a) whether it
advertises retryability at all, (b) the pace it names in `retry_after_ms`, and
(c) whether a conforming client has a policy to conform to. Those are the levers
assigned above; a non-conforming client is handled by §3's observation that such
a client already ignores the field today.

### 5. Wire example — degraded-empty, all legs timed out

This supersedes the degraded-empty example under "Wire examples" for the
all-timeout case. That example remains correct for a `backend_error` cause,
except that its message text should not describe a timeout.

```json
{
  "ok": false,
  "tool": "search",
  "error": {
    "kind": "search_incomplete",
    "message": "No-match was not established because selected backends failed.",
    "retryable": true,
    "retry_after_ms": 2000,
    "missing_backends": ["main"],
    "backend_errors": {
      "main": {
        "kind": "timeout",
        "message": "backend search timed out after 5000ms"
      }
    }
  }
}
```

With one leg timing out and another failing for any other reason, `retryable` is
`false` and `retry_after_ms` is absent.

`retry_after_ms` is deliberately not equal to the leg's deadline here. It is the
server's estimate of when a retry could succeed, not a restatement of how long
the failed attempt took; the two are unrelated quantities and an example showing
them equal would read as a rule that they must be.

### 6. Verification

- A degraded-empty response whose legs all timed out reports `retryable=true`
  with a positive `retry_after_ms`; one with any non-timeout leg reports `false`.
- Classification survives truncation: a failure set whose only non-timeout cause
  is omitted by Amendment 1's budget still reports `retryable=false`.
- Classification is structural — a `backend_error` whose message contains the
  word "timeout" is not reclassified.
- `kind` admits exactly the two values; any other value is rejected.
- A surface that emits `retryable=true` without publishing the budget, backoff
  schedule and breaker threshold required by §4 fails review.
- `retryable=true` is never emitted without `retry_after_ms`. A constant value
  is acceptable only as the default floor published with the retry policy; an
  undocumented constant fails review, as does omitting the field on the grounds
  that no estimate was available.

### 7. Release

**Amendment 2's wire changes ship in v0.9.0** — Release N+1 in the compatibility
window — not in v0.8.0.

Amendment 1 was additive: it introduced `backend_errors` with `kind` fixed to the
single constant `backend_error`, so a reader that ignored the new object was
unaffected. That form shipped in v0.8.0, which means a strict reader may have
pinned `kind` to that one value. Widening it to a closed two-value vocabulary
makes such a reader reject a well-formed response, so the widening is a breaking
change *for that reader* and takes the next release rather than a point update.
`retry_after_ms` is a new field and would be additive on its own; it ships in the
same release as the vocabulary change because §2 makes the two jointly
observable — a `retryable=true` response is exactly one whose legs all typed as
`timeout`.

Within v0.9.0 there is no intermediate state: a surface either emits the typed
vocabulary with conditional retryability and `retry_after_ms`, or it emits the
v0.8.0 contract. Emitting `timeout` while keeping `retryable` unconditionally
`false` is not a valid partial adoption, because it publishes a cause the reader
can act on while denying the action the cause licenses.

## References

- the two follow-up items this record's fixes were split from
- ADR-006 deterministic scoring; ADR-012 retrieval composition; ADR-029
  substrate coordinator; ADR-033 recall pipeline; ADR-045 verb response
  presentation
- issue #1829 (backend failure causes were discarded from response and logs)
