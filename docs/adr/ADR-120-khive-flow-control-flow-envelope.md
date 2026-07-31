# ADR-120: Khive Flow — A Bounded Control-Flow Envelope in the Request DSL

**Status**: proposed\
**Date**: 2026-07-21\
**Scope**: authorization-gated, anonymous, read-only control flow over existing verbs; the
semantic scalar primitive that flow (and, later, an optional GQL adapter) may branch on\
**Depends on**: [ADR-016](ADR-016-request-dsl.md) (`request` DSL wire contract, which this
ADR extends with a new AST form)

## Context

lionagi's `flow` gives declarative orchestration of reasoning and action: a terse
surface over branch state, tool calls, and multi-step reasoning, backed by real
machinery an agent author never has to hand-write. Khive Flow is the same design
philosophy applied one layer down, at retrieval instead of reasoning: declarative
dynamic context retrieval — a terse, auditable surface over the verb dispatcher,
substrate scorers, and the existing `request` DSL parser, so that "which corpus, at what
confidence, with what fallback" becomes a server-side program instead of client-side
glue.

The product problem is concrete. An agent deciding where to ground an answer today runs
a manual loop: `memory.recall`, inspect the top hit, judge whether it clears a confidence
bar, and if not fall back to `knowledge.search`, then judge again. That loop costs 3-4
round trips per decision, each round trip re-serializes context across the wire, and the
judgment step is usually eyeballed rather than recorded. Khive Flow's goal is to let that
exact loop — recall, judge, fallback, judge, return — become one request: one auditable
program, dispatched once, with every branch decision captured in a trace instead of
scattered across an agent's own reasoning transcript.

The motivating concept sketch establishes the shape agents actually want: similarity and
agreement scores as first-class control-flow values, not merely query predicates, with a
retrieval-routing fallback chain (`memory.recall` → `knowledge.search`) collapsed into one
server-side program. The sketch is a concept illustration, not literal syntax; this ADR
follows the Decision's grammar, not the sketch's spelling.

Three technical constraints converge on the same architecture. The scoring primitive already
exists at the storage layer (`SqliteVecStore::score_candidates`) and needs only promotion into
the `VectorStore` trait — no new scoring machinery, only a new contract boundary. Comparison
against other graph+vector systems (Kùzu, Memgraph, Neo4j) shows that fused graph/vector query
is not a unique claim, but that an inline, text-input boolean predicate with declared threshold
semantics and reproducible scores, usable anywhere in a control-flow program rather than only
inside a query pattern, is not common; bare, undeclared thresholds are a common footgun in
existing systems. An earlier design's shipped and prototype forms separate a transferable
lesson — a small, deterministic, symbolic execution boundary (`let`, bounded conditionals,
bounded `map`, mandatory `return`) — from parts that do not transfer to khive's retrieval
layer: an LLM-in-the-loop reasoning layer is a different concern from bounded, deterministic
control flow over retrieval primitives.

Two claims from an earlier synthesis do not hold and are rejected here before any
implementation proceeds: that fixed-point score comparison makes a _run_ reproducible (it only
makes branch selection deterministic given the same vectors, corpus, and embedder), and that a
`declared_verbs` manifest is a sufficient capability model for a stored, third-party-invocable
program (it is not — it does not prevent a confused deputy or argument-level injection). This
ADR encodes the narrowed decision that survives those two rejections.

## Decision

Adopt **Khive Flow** as a minimal, anonymous control-flow envelope in the existing
`request` DSL/AST. It is not a new standalone language, not a new MCP verb, and not an
extension of GQL into an orchestration language. GQL remains a read-only pattern query
language; it cannot sequence arbitrary verb calls, bind their canonical results, branch
on a prior result, or bound a data-dependent fan-out. A new independent `batch` verb is
equally insufficient: independent batch dispatch and linear `$prev` chains cannot express
conditional branch selection in one round trip.

The smallest accepted surface:

```text
flow {
  let name = existing_verb(...)
  if structural_predicate { ... } else { ... }
  let items = map item in $name.items limit N => existing_read_verb(...)
  return json_projection
}
```

### V1 properties

1. **Anonymous execution only.** No stored programs. A flow is submitted, executed once,
   and discarded; nothing about it is name-addressable or re-invocable by another caller.
2. **Existing verb calls only.** No dynamic verb names, no source templating, no general
   loops, no recursion, no exception handling, no hidden state, no LLM calls. Every
   dispatch inside a flow is a call to a verb the caller could already invoke directly.
3. **Read-only verbs only.** `map` is non-nested, carries a literal limit, preserves
   input order in its result, and stops scheduling new children after the first observed
   failure (see Error semantics, below).
4. **Structural JSON predicates plus one optional scalar semantic primitive:**
   `score(entity_ref, query_text) -> DeterministicScore`. This means the stored canonical
   entity body compared against query text — nothing else. It does not accept arrays,
   result envelopes, arbitrary bindings, or two transient text values; text-to-text
   scoring would require two ad hoc embeddings and an undefined role/model contract that
   the current substrate does not supply, and envelope scoring would require a projection
   and reduction contract (field selection, item weighting, aggregation across
   `min`/`max`/mean/quorum, missing-vector behavior, provenance) that has no resolved
   semantics yet. V1 exposes only the entity-body-to-query operation the substrate
   actually supports today.
5. **Mandatory explicit `return`.** Bindings hold canonical pre-presentation results,
   following existing `$prev` precedent; nothing is returned implicitly.
6. **Static worst-case execution budget validation before dispatch.** Each verb call,
   each scheduled `map` child, and each semantic score expression consumes one of at most
   100 flow steps, validated before the first operation runs. Independent ceilings
   additionally bound candidates scored, embeddings computed, result bytes, and wall-clock
   deadline — a nominal 100-step flow must not be able to hide unbounded work behind one
   of those axes. The budget composes at the request level: a request's total step
   budget sums across all of its ops — a plain verb call counts as one step and a flow
   counts as its statically validated step count — and the same 100-step cap applies to
   the whole request. Without this composition rule, a batch of maximal flows would
   multiply the per-flow cap by the batch width and defeat this property one layer up.
7. **Fail-fast result, no rollback claim.** Once a child is started it may finish; every
   started-step outcome, including children already in flight when the flow fails,
   remains in the trace even when the containing flow fails. See Error semantics.

### Threshold and cardinality semantics

A semantic predicate requires an explicit result `LIMIT`, a server-side candidate
ceiling, and exposed score metadata — never a bare, undeclared threshold. Threshold
comparison is exact over raw fixed-point values; tie order is score, then stable UUID.
Flow scalar branch comparisons remain bounded by the flow step budget in property 6. A
bare `> 0.7` with no limit, no declared metric, and no visible candidate ceiling is the
exact footgun the landscape survey identifies as the dominant operational failure mode in
surveyed systems, and this ADR does not ship it.

### Score contract

One surface contract, pinned: normalized cosine similarity in `[0,1]`, quantized once to
a signed i64 at scale `2^32`, with no per-call metric or model override. The runtime
resolves a model/index contract internally and exposes its immutable identifiers in the
trace (see below) rather than accepting one from the caller. Per-call metric or model
selection is rejected because it permits a caller-selected metric to mismatch the stored
vector's actual metric, silently destroying the comparability that makes a threshold
meaningful; exposing the backend's raw, unnormalized score is rejected because it leaks
implementation-specific (sqlite-vec) conventions into a contract meant to be portable.

### Error semantics

Fail-fast, with precise concurrent semantics: on the first observed error, stop
scheduling new `map` work; let already-started read operations settle; preserve their
ordered outcomes in the trace; fail the containing flow. V1 flow does not offer typed
`try`/per-item recovery and does not permit error inspection or continuation from within
a flow — multiplying type, branch, audit, and partial-success states is deferred until
core semantics have shipped and stabilized. Because completed reads cannot be rolled
back and parallel `map` siblings are independent, "fail-fast" for a concurrent map cannot
mean "as if nothing ran"; it means "stop starting new work and report exactly what ran."
This is also why v1 flow is read-only for its entire body: a write verb inside a `map`
body would turn "already-started work may finish" into a partial, unrecoverable side
effect, which the product scope (dynamic context retrieval) does not need to accept.

### Authorization

The parser and AST ship unconditionally. Execution of flow/semantic AST nodes sits
behind two independent gates: gateway authorization **and** runtime capability
validation, and the runtime fails closed — it rejects those nodes whenever the signed or
effective grant is absent, regardless of which transport the request arrived through. A
gateway-only check is not treated as sufficient: the gateway is one policy boundary, but
an alternate transport that reaches the runtime directly must not be able to execute
syntax the gateway would have rejected. This ADR does not add a second public query verb
and does not add a separate verb namespace; `flow` and `score(...)` ride the one existing
`request` surface, gated by capability rather than by a separate endpoint.

## Trace and reproducibility contract

"Deterministic score" does not by itself make a _run_ reproducible. Fixed-point
comparison makes branch evaluation deterministic given the same vectors; a repeated
execution can diverge if the corpus, canonical entity body, stored vector, embedding
artifact, embedder configuration, or candidate population changes, or if a mutable model
alias or remote inference service lets the query embedding itself drift. The honest
contract has three parts:

- **deterministic-at-execution**: the recorded raw score and threshold always select the
  same branch, for that one execution;
- **trace-verifiable**: the trace proves which inputs, scores, policy, and branch were
  actually used;
- **replayable only against pinned artifacts/snapshots**: absent a retained
  corpus/vector snapshot and a pinned embedder artifact, replay against a live corpus is
  not promised.

Every semantic flow trace MUST record:

- request/program AST digest and runtime build identifier;
- effective invoker/tenant identity and entitlement-policy version;
- each invoked verb's handler contract digest and canonical pre-presentation result hash;
- embedding model name **and immutable artifact/revision digest**, embedder
  role/configuration, vector dimension, and score-contract ID;
- corpus/vector snapshot or watermark, canonical-body version, candidate IDs in
  deterministic order, and vector/content digests where available;
- query-embedding digest (and a retained replay handle if exact replay is promised);
- raw i64 scores, raw thresholds, stable tie-break values, selected branches, skipped
  branches, and budget consumption; and
- started/completed/failed/aborted status for every step, including children already in
  flight at failure.

If the system cannot pin or retain the model and corpus/vector state, the trace MUST say
`replayable=false`. Marketing copy MAY say "deterministic score comparison" and
"auditable semantic routing." It MAY NOT say "same query, same result" or "reproducible
across runs" without snapshot replay evidence to back the claim.

## Shipping sequence

Implementation order is **S2-first** — the flow spine before the public semantic
adapter — with a non-public substrate gate ahead of both:

1. **Contract gate.** Promote candidate scoring behind the storage trait
   (`VectorStore`); define the canonical-body, score, trace, budget, and entitlement
   contracts. This stage also establishes the effect-classification source of truth:
   each verb's read/write effect class becomes verb-registry metadata, consumed by both
   the static validator and the rejection tests, with a fail-closed default — a verb
   with no declared effect class is treated as a write and rejected from flow bodies. A
   hand-maintained verb list inside the flow executor is the fail-open form of the same
   check and is not an acceptable substitute. No public semantic syntax ships at this
   stage.
2. **Khive Flow spine.** Implement and validate anonymous, read-only
   `let` / `if` / bounded `map` / `return` over existing verbs, with structural
   predicates only.
3. **Semantic scalar.** Add `score(entity_ref, query_text)` to flow once the contract
   gate has passed.
4. **Optional GQL adapter.** Add `semantic(n, "text")` to GQL WHERE only if product
   evidence shows inline graph-pattern filtering is valuable beyond composing flow with
   an existing retrieval verb. It must reuse the same scorer and the same trace contract
   as flow — it is a second frontend onto one scoring primitive, not a second primitive.

## Consumers

A stage of this ADR is consumed only when a named consumer executes it live — a merged
implementation with no live traffic does not satisfy the gate.

**First live consumer (stage 2)**: a recurring agent wake-time sweep (fetch the task queue
and inbox; if unread messages exist, fetch each unread message body under a bounded map;
return one orientation envelope). Today this runs as two to three separate request round
trips with client-side branching between them; it is expressible entirely in stage-2
constructs (`let` / `if` / bounded `map` / `return` with structural predicates, no
semantic scalar). Stage 2 is LIVE when this program is the default orientation path
agents use in daily operation, issuing flow requests against the production server.

**First live consumer (stage 3)**: the retrieval-routing fallback chain described in
Context (recall, score-branch on the top hit, fall back to corpus search, return with
the branch decision traced), run in daily agent operation. Stage 3 does not merge until
the stage-2 consumer is live, and stage 4 (the optional GQL adapter) is not considered
until the stage-3 consumer has produced the product evidence its own acceptance bar
requires.

S1-first (public semantic GQL predicate before the flow executor exists) is rejected as
the entry point: its own scoping estimate is 11-15 files and roughly 650-1050 LOC for the
predicate alone, which commits public field, score, limit, trace, and entitlement
contracts before the reusable executor exists underneath them — not a cheap probe, but a
contract-first commitment to the wrong layer. Building S1 and S2 in parallel is also
rejected: it duplicates contract discovery across two surfaces at once and makes
convergence between them harder to prove, rather than easier.

## Out of scope

- **Stored named programs** (a persisted, name-addressable `FlowProgram` invocable by a
  third party). `declared_verbs` plus invocation identity is not a sufficient capability
  model: it does not prevent a confused deputy, does not constrain a code-bearing
  argument inside an otherwise-allowlisted verb call (a GQL string, for instance), and
  does not stop a named verb from silently changing behavior out from under a program
  that only pinned its name. This requires a separate capability/storage ADR with its own
  threat model before it can return to scope.
- **`match`-on-range syntax and `ctx.agree`.** Range dispatch is already expressible
  today via ordered structural comparisons over one scalar score, and a bounded `map` of
  scores covers result-set validation; dedicated `match` syntax would add overlap,
  exhaustiveness, and boundary-rule questions without adding new capability. `ctx.agree`
  has no resolved semantic unit: a verb result can be an envelope, array, entity, note,
  projection, or error, and "agree" does not yet state field selection, item weighting,
  aggregation across multiple items, missing-vector behavior, or provenance handling.
  Those choices change routing outcomes and are deferred pending usage evidence; if
  repeated need is demonstrated, a future ADR should spell query-vs-query similarity and
  per-result validation as two distinct, separately named forms rather than overloading
  one form across scalar and collection semantics.
- **Write verbs inside flow.** V1 flow is read-only end to end; see Error semantics
  above for why a concurrent `map` cannot safely host writes under a fail-fast, no-rollback
  contract.
- **Per-field vectors** (`semantic(n.field, ...)`). The storage substrate holds one
  canonical entity-body vector per entity today, not per-field vectors; expanding storage
  identity, backfill, and lifecycle before field-level demand is proven would commit
  infrastructure ahead of evidence. The `semantic(n, "text")` spelling is reserved now so
  a later per-field ADR does not have to invent new syntax, but v1 must not pretend a
  field argument selects anything — that would create a permanent semantic lie and block
  true per-field vectors later.
- **LLM-in-loop constructs** (synthesis, tool-selection policy, multi-round reasoning
  inside a flow body). These belong to a reasoning layer, not a retrieval layer, and
  admitting them here would reintroduce non-determinism into a surface whose entire value
  proposition is deterministic, auditable branching.

## Alternatives considered

**Extend only GQL plus the existing batch/chain forms.** Rejected: GQL should remain a
database pattern-query language, and independent batch dispatch plus linear `$prev`
chains lack named multi-result bindings, conditional dispatch, and bounded
data-dependent fan-out. Neither can implement the one-round-trip retrieval-routing loop
this ADR targets without pushing orchestration back onto the client.

**A standalone semantic orchestration language and runtime.** Rejected: it would
duplicate the existing request parser, verb dispatcher, result envelope, error rules, and
policy surface. The needed capability fits inside a small, four-construct extension of
the existing request AST.

**Dedicated top-k-only retrieval verbs, no control-flow language.** Rejected as the sole
surface: it is operationally the safest option and remains valid as an underlying
retrieval implementation informing limits and score exposure, but it provides no reusable
control flow and discards the inline boolean predicate's composability inside a larger
program. It does not solve the fallback-chain product problem.

**Ship the original four-stage ladder (S1 public predicate → S2 flow → S3
match/ctx.agree → S4 stored programs) unchanged.** Rejected: it locks a public threshold
adapter in before the executor it depends on exists, treats an undefined `ctx.agree`
contract as ready-to-ship syntax, overclaims cross-run reproducibility from fixed-point
scoring alone, and places durable, delegated code behind a capability model
(`declared_verbs`) that this ADR has shown is insufficient.

## Verification / acceptance criteria

1. Parser rejection tests for every prohibited construct (dynamic verb names, write
   verbs, stored programs, `match`/`ctx.agree`, text-to-text or envelope scoring,
   per-call metric/model override) and for execution attempted with entitlement absent.
2. Static-budget property tests proving no accepted AST can schedule more than 100
   steps, plus separate tests for the candidate, embedding, result-byte, and deadline
   ceilings, plus request-level composition tests proving a batch whose summed step
   count (plain calls at one step each, flows at their validated step counts) exceeds
   100 is rejected before any op runs.
3. Effect-classification tests proving the read/write class is read from verb-registry
   metadata (not an executor-local list) and that a verb with no declared class is
   rejected from a flow body as a write.
4. Deterministic-score golden tests covering decimal-to-i64 conversion, boundary
   operators, missing-vector handling, stable UUID tie-breaking, and canonical-body
   versioning.
5. Trace golden tests proving raw inputs, raw scores, branch decisions, contract
   digests, canonical result hashes, in-flight children at failure, and the
   `replayable` flag are all recorded as specified above.
6. Concurrency tests proving that a first observed error stops new `map` scheduling,
   already-started reads settle and appear in order, output order follows input order,
   and the containing flow fails.
7. Security tests through every supported transport proving a gateway bypass cannot
   reach the runtime without also satisfying runtime capability validation and tenant
   scope.
8. Benchmarks at the declared candidate ceilings, run before any optimizer or pushdown
   claim is made and before any latency claim is published.
9. A separate, threat-modeled ADR before stored programs or
   third-party invocation are reintroduced to scope.

## Risks & unknowns

- **Cloud enforcement seam is unverified.** No cloud gateway source is available against
  which to verify this ADR's enforcement claim directly. Mitigation: a fail-closed
  integration test through every supported transport is required before enforcement is
  accepted as implemented, not merely as designed.
- **Candidate-scale behavior is unmeasured.** Exact batch scoring at declared ceilings may
  exceed latency or resource targets. Mitigation: benchmark at the declared candidate
  ceiling before any optimizer or pushdown claim; make no such claim until then.
- **Missing-vector semantics are unresolved.** Whether a missing vector produces a typed
  query error or a non-match must be chosen before public syntax ships; it must never
  silently trigger an embed-and-persist side effect during what is nominally a read.
- **Model artifact pinning may be unavailable.** If the embedder is a mutable remote
  service rather than a pinned local artifact, the replay contract must downgrade
  gracefully to trace-verifiable-only, never silently claim more than that.
- **Read-only v1 may undershoot future automation demand.** This is intentional for v1;
  writes return only with their own atomicity, idempotency, and semantic-side-effect
  threat analysis, not as an incremental extension of this grammar.
- **Future stored-program persistence has no settled storage substrate.** A later ADR
  must select an existing blob/artifact substrate plus metadata, or explicitly reopen
  that taxonomy decision — this ADR does not pre-select one.

## Implementation fences

### MAY

- Add `flow` as a parser/AST form under the existing `request(ops=...)` surface.
- Reuse existing verb dispatch and canonical results; generalize named references
  without changing legacy `$prev` behavior.
- Implement anonymous, read-only `let`, `if`/`else`, one-level bounded `map`, mandatory
  `return`, structural predicates, and the single entity-body scorer.
- Ship parser syntax in OSS while gating cloud execution behind gateway and runtime
  entitlement.
- Meter existing capacity and embedding consumption as usage; meter future execution
  only after a separate stored-program decision is made.

### MAY NOT

- Add a second public query verb, a separate verb namespace, a new KG taxonomy kind, an
  LLM call, dynamic tool selection, a general loop, recursion, `try`, hidden scratch
  state, or a stored program under this ADR.
- Accept `semantic(n.field, ...)`, arbitrary binding/envelope scoring, text-to-text
  scoring, `ctx.agree`, or `match similar` in v1.
- Execute write verbs inside v1 flow, or treat already-started concurrent work as if it
  had been rolled back.
- Allow per-call metric/model selection, mutable model aliases without a traced
  immutable resolution, source interpolation, or dynamic verb names.
- Describe a live-corpus rerun as reproducible without pinned model/vector/corpus state
  backing that claim.

## Consequences

Agents get one server-side program for the retrieval-routing loop they currently run by
hand across 3-4 round trips, with every branch decision captured in a trace instead of
left implicit in an agent's own reasoning. The scoring primitive that both flow and any
later GQL adapter depend on gets exactly one contract (score, quantization, tie order,
trace fields), so the two frontends cannot drift into incompatible semantics. Stored
programs, `ctx.agree`, per-field vectors, and writes inside flow are named and deferred
rather than silently absent, each with the specific evidence or decision that would
reopen it. The public wire surface gains one new AST form under the existing `request`
tool; it does not gain a new verb, a new endpoint, or a second query language, and OSS
users see the parser without gaining cloud execution capability.
