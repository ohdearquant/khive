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
- `error.retryable=false`;
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

## Wire examples

### Degraded-empty

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
retry-policy row in Verification.

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
   generic error-retry client behaviour is covered by this row.
9. Compatibility-alias contract tests: `partial=true` present beside every
   `status="partial"` success and absent on `complete` in v0.8.0.
10. Frame-budget omission tests in verbose and agent modes asserting the full
    typed `search_incomplete` metadata survives omission untransformed.
11. Release-gated schema fixtures: v0.8.0 fixtures for strict readers including
    the degraded-empty error; v0.9.0 fixtures proving `score`, `min_score`, and
    `partial` are absent on emit and `min_score` is rejected with an
    unsupported-field error.
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
Each value is `{kind: "backend_error", message: <captured cause>}`. A partial
success carries it beside `result`; degraded-empty `search_incomplete` carries
it inside `error`. Complete searches omit it. Presentation and frame-budget
omission preserve the diagnostics at the same location.

Diagnostics are mandatory but bounded: retain at most 16 causes and no more
than one fixed per-operation wire budget; cap backend ids at 256 Unicode scalar
values using a stable hash suffix and cap cause messages at 1,024 scalar values
plus an ellipsis. Before exposure or warning, scan at most 4,096 input scalar
values with the canonical credential masker. Empty causes become
`backend search failed without diagnostic detail`. If any failed legs are not
retained, emit `backend_errors_truncated=true` and the exact
`backend_errors_omitted` count. At least one cause MUST survive whenever a leg
failed, and retained `missing_backends` and `backend_errors` keys MUST have
exact parity. The server logs each retained masked cause at warning level and a
bounded aggregate warning for omitted causes.

This amendment is additive. It does not change completeness, retry, filtering,
or ranking semantics; it supplies bounded evidence for the already-declared
degradation state.

## References

- the two follow-up items this record's fixes were split from
- ADR-006 deterministic scoring; ADR-012 retrieval composition; ADR-029
  substrate coordinator; ADR-033 recall pipeline; ADR-045 verb response
  presentation
- issue #1829 (backend failure causes were discarded from response and logs)
