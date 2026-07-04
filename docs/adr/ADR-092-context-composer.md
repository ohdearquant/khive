# ADR-092: Cross-substrate context composer — ContextContributor trait + `context.assemble`

**Status**: Proposed
**Date**: 2026-07-04
**Depends on**: ADR-002 (edge ontology), ADR-007 (namespace attribution), ADR-016 (request DSL),
ADR-017 (pack standard), ADR-035 (three-tier profile resolution), ADR-049 (daemon warm state),
ADR-058 (shared `resolve_consumer_profile` helper), ADR-081 (recall retune driver / serve ledger),
ADR-089 (kg `context` verb — the kg contributor's retrieval body)
**Relates to**: ADR-091 (memory-backend connector reader rules — align §Execution when it lands)

## Context

An agent injecting khive context into every model turn needs _cross-substrate_ context: graph
neighborhoods (kg), decay-weighted memories (memory), and reranked knowledge sections (knowledge),
assembled under one hard output budget in a single call. ADR-089 gives entity-anchored graph context
from the kg substrate alone. `memory.recall` and `knowledge.compose` each rank their own substrate.
No verb assembles across substrates. Today the per-turn prefetch hook does this caller-side: multiple
MCP round-trips (recall + search + per-anchor neighbors), caller-side budget math, and no place for
the runtime to learn how much budget each substrate deserves per consumer.

Three shapes were considered (Ocean ratified the third):

- **A — pack-blind substrate retrieval + traversal** (one hybrid search across all kinds + annotates
  expansion). Rejected: discards each pack's ranker (memory decay/salience/posteriors, knowledge
  section reranking) — the quality lives there — and bypasses ADR-081 serve attribution.
- **B — composer dispatches existing MCP verbs with hardcoded adapters.** Rejected: verb-name +
  response-shape coupling that rots with every pack change; violates the decoupling ADR-017 exists
  for. (In-process `registry.dispatch` cross-pack calls do ship today — e.g. `memory.feedback`
  routing to `brain.feedback`, `crates/khive-pack-memory/src/handlers/feedback.rs:108` — but they
  fire one known side-effect at one known pack; consuming N unknown packs' structured returns through
  that path reintroduces exactly this coupling.)
- **C — a capability trait packs optionally implement + a runtime-registry-fanning composer verb.**
  Adopted.

## Decision

### 1. `ContextContributor` capability trait

A new object-safe async trait in `khive-runtime`, attached to `PackRuntime` by one additive optional
method defaulting to `None` (the `kind_hook` pattern, `crates/khive-runtime/src/pack.rs:141`, ADR-017):

    // crates/khive-runtime/src/pack.rs — on trait PackRuntime
    fn context_contributor(&self) -> Option<Arc<dyn ContextContributor>> { None }

    #[async_trait]
    pub trait ContextContributor: Send + Sync {
        fn source_pack(&self) -> &'static str;
        async fn contribute(
            &self,
            req: &ContextRequest,
            token: &NamespaceToken,
        ) -> Result<Vec<ContextSlice>, RuntimeError>;
    }

`contribute` MUST return owned data only and MUST release every read snapshot before returning
(§Execution). `VerbRegistry` gains one additive accessor,
`context_contributors() -> Vec<Arc<dyn ContextContributor>>`, iterating the existing pack set
(mirrors the existing `for pack in self.packs.iter()` sweep in `dispatch`,
`crates/khive-runtime/src/pack.rs:1014`). All existing packs compile unchanged; a pack that does not
implement the method contributes nothing.

There is no runtime-built-in (pack-less) verb mechanism to attach to instead: every verb, including
`verbs()` itself, is a pack-owned `HandlerDef` (`crates/khive-pack-kg/src/handler_defs.rs:692`)
dispatched by iterating `self.packs`; a bare built-in would require special-casing the dispatch loop,
which ADR-017 disallows.

### 2. Request and slice contracts

    pub struct ContextRequest {
        pub query: Option<String>,
        pub entity_ids: Vec<String>,
        pub consumer_kind: String,
        pub budget_hint: usize,       // total assembly budget; NOT a per-contributor allocation
        pub hops: u8, pub fanout: u8, pub direction: Direction, pub relations: Vec<String>,
    }

    pub struct ContextSlice {
        pub source_pack: &'static str,
        pub kind: String,
        pub id: String,               // == brain serve/feedback target_id
        pub content: serde_json::Value,
        pub score: Option<f64>,       // partition-internal; never fused across packs
        pub score_semantics: ScoreSemantics, // decay_weighted | rerank | graph_proximity | relevance | none
    }

Scores are partition-internal provenance and a within-contributor ordering key, never a cross-pack
fusion input. No normalization is mandated. `id` is the single serve/feedback target. Char budgeting
counts Unicode scalar values of the compact JSON serialization of each slice as emitted — identical
to ADR-089 — and is computed by the composer on the bytes it emits, never trusted from the
contributor.

### 3. The `context.assemble` verb

A new pack `khive-pack-context` (pack name `context`, `REQUIRES = &[]`) registers one
`Visibility::Verb` handler:

    context.assemble(query?, entity_ids?, budget?, consumer_kind?, hops?, fanout?,
                     direction?, relations?, namespace?)

At least one of `query`, `entity_ids` is required. `budget` defaults to 4096, clamped 256..=65536
(ADR-089). `namespace` defaults to `local` (ADR-007) with the standard explicit escape;
`consumer_kind` defaults to `"context"`. The handler is contributor-agnostic: with no contributor
packs loaded it returns a valid empty assembly.

### 4. Budget allocation

This is a **one-round design**: allocation happens strictly after fanout, never before it, and
`ContextRequest` never carries a per-contributor allocation. The caller passes total `budget`.
The sequence is:

1. The composer runs all contributors concurrently (§Execution). Each receives the request's
   `budget_hint` (the total assembly budget) purely as an advisory cap on how much it retrieves —
   contributors are not required to honor it precisely and MUST NOT rely on it as their allocation;
   they return their natural top-k candidates bounded by their own caps (memory `limit`, kg
   `fanout`, knowledge top-k), not by budget, and MUST self-filter below their own usefulness floor
   rather than pad.
2. Only after fanout completes does the composer form the success set S (§Execution), look up
   per-`consumer_kind` weights from config, restrict them to S, and renormalize; absent config →
   equal split over S. Contributors are ordered by the registry's topological pack order.
3. Per-contributor allocation is applied **only during composer assembly** — it never reaches the
   contributor. Assembly is a two-pass greedy fill over each contributor's already-returned
   candidates: pass 1 appends each contributor's ranked slices up to its computed allocation; pass 2
   (reflow) distributes `budget − Σused` to contributors in the same order until the global cap.
   Reflow is mandatory — it is what lets a substrate with no relevant content yield its budget to
   substrates that do, which is why partitioned budgets (no cross-pack fusion) do not underperform
   fused ranking. Truncation sets `truncated: true` with dropped counts and is a view decision
   (nothing mutated).

This is explicitly not a two-phase bid: contributors are invoked exactly once per assembly: there is
no pre-round where they are told their allocation before retrieving.

Config:

    [context]
    default_budget = 4096
    per_contributor_timeout_ms = 150   # clamp 50..=1000
    max_concurrent_contributors = 4
    [context.weights.<consumer_kind>]  # e.g. prefetch: kg=0.5, memory=0.3, knowledge=0.2

Contributors never see how the split was computed — that computation is entirely composer-internal
and happens after they return — so v2 learned allocation (ADR-081 posteriors per
(consumer_kind, source_pack)) drops in with no trait change.

### 5. Execution model

Contributors run concurrently under a semaphore of `max_concurrent_contributors`, each under
`per_contributor_timeout_ms`. A failed or slow contributor is skipped, its budget reflows, and it is
named in the response `skipped` list — it never fails the assembly. Zero successful contributors →
valid empty assembly.

Read-snapshot hygiene (issue #580): #580 was a long-lived reader pinning the WAL checkpoint, not a
count problem. `contribute` MUST materialize all reads into owned `ContextSlice` data and release
every snapshot/transaction/statement before returning; no reader survives the `contribute` call or
the assembly boundary. The per-contributor timeout (≤1000ms) bounds reader lifetime; bounded
concurrency caps simultaneous acquisition. Full serialization is rejected (latency cost, no #580
benefit). This invariant aligns with ADR-091's reader-lifetime rules.

### 6. Serve attribution

**Consumer-kind vocabulary is closed, and this ADR must extend it.** `resolve_consumer_profile`
(`crates/khive-brain-core/src/profile.rs:118`) takes a `ConsumerKind` enum, not a bare string
(`crates/khive-brain-core/src/profile.rs:31` currently declares only `Recall`, `KnowledgeCompose`,
`Rerank`), and ADR-058 is explicit that adding a new consumer means adding a variant, never a bare
string literal at a new call site (`docs/adr/ADR-058-brain-posterior-read-path.md:343`). This ADR
does **not** propose a string-based resolver, and does not treat `context.assemble`'s free-form,
caller-supplied `consumer_kind` request field (§3, used for budget-weight lookup) as the same thing
as the closed Rust-side profile-resolution vocabulary — the two are deliberately decoupled:

- **Required addition**: add `ConsumerKind::Context` (`as_str() == "context"`) to the enum in
  `crates/khive-brain-core/src/profile.rs`, alongside the `brain.record_serve` wrapper, as part of
  this ADR's implementation PR — the same ADR-058-conformant pattern used for `Recall` and
  `KnowledgeCompose`.
- **Profile resolution** (`served_by_profile_id`) is attempted **only** when the request's
  `consumer_kind` string maps to a known `ConsumerKind` variant (today: exactly `"context"` →
  `ConsumerKind::Context`, resolved once at the composer level via the existing shared three-tier
  helper `khive_brain_core::resolve_consumer_profile`, ADR-035/ADR-058 — the same helper the memory
  pack (`ConsumerKind::Recall`) and knowledge pack (`ConsumerKind::KnowledgeCompose`) already share).
- **Budget-weight lookup is unconstrained and separate.** The request's `consumer_kind` string
  (§4's `[context.weights.<consumer_kind>]` config keys, e.g. `"prefetch"`) is used for allocation
  regardless of whether it maps to a `ConsumerKind` variant — this is config-table lookup, not
  Rust-enum resolution, and ADR-058's closed-enum rule does not apply to it.
- **Any `consumer_kind` string that does not map to a known `ConsumerKind` variant** (e.g. the
  response example's `"prefetch"`) skips profile resolution entirely: `served_by_profile_id` is
  `None` on the response, and the `brain.record_serve` dispatch (below) carries
  `served_by_profile_id: None`. Budget-weight lookup for that request is unaffected. This is not a
  silent failure — the response field is simply absent/null, which is the existing "no resolvable
  profile" fail-safe shape ADR-081 §4 already defines for zero-weight recording.

`served_by_profile_id`, when resolved, is stamped on the response.

The ADR-081 §4 cross-session serve ledger (`brain_serve_ledger`) already models exactly the row shape
this needs: one row per served target, keyed `(namespace, target_id, query_class, served_at)`. Its
write side, `record_serve` (`crates/khive-pack-brain/src/serve_ledger.rs:83`), is implemented but
**not yet wired to any call path or exposed as a dispatchable verb** — the doc comment on that
function states it is "provided so the ledger's write contract is implemented, not stubbed, ahead of
that wiring," and it takes a single `target_id`, not an array. There is currently no `brain.record_serve`
MCP verb. This ADR corrects an earlier draft's assumption that such a verb and an array-taking
`target_ids` parameter already existed — they do not; the schema and the row shape do exist and need
no change.

Concretely, this ADR requires one small addition to `khive-pack-brain`: a thin `brain.record_serve`
verb (`Visibility::Verb`, params: `target_ids: Vec<String>`, `consumer_kind`,
`served_by_profile_id?`, `query_raw`, `served_at`) whose handler derives every field the existing
`record_serve` function (`crates/khive-pack-brain/src/serve_ledger.rs:83`) requires but the wrapper's
own params do not carry directly:

- `namespace` — derived from the dispatch token (the same `NamespaceToken` the registry mints at the
  dispatch boundary for every verb call, ADR-007), not a caller-supplied param.
- `id` (row id) — one freshly generated id per target, generated by the handler, not the caller.
- `query_class` — derived from `query_raw` per ADR-081 §4's deterministic normalization algorithm
  (lowercase, strip punctuation, collapse whitespace, sort unique tokens, join, first 16 hex chars of
  the SHA-256; `docs/adr/ADR-081-recall-retune-driver.md:182`), computed once per dispatch and reused
  for every target in the loop (all targets in one assembly share one `query_raw`).
- `resolved_profile_id` / `resolved_at` — always `None` on this path; ADR-081 §4's
  `accounting_profile_id` derivation (`COALESCE(served_by_profile_id, resolved_profile_id)`) already
  handles a row where only `served_by_profile_id` (or neither) is set.

With those four fields derived, the handler loops the existing single-target `record_serve` function
once per `target_id` in `target_ids`, writing one ledger row per target — no schema migration, since
the table already stores one row per `target_id`; every column the table requires is now accounted
for. The composer fires this dispatch once per assembly, asynchronously off the response path
(`tokio::spawn`, fail-soft — the same fire-and-forget shape already used elsewhere in the pack, e.g.
`crates/khive-pack-memory/src/ann.rs:289`), carrying `target_ids` = all appended slice ids across all
contributors, `consumer_kind`, `query_raw`, `served_at`, `served_by_profile_id` (per the vocabulary
constraint above: `None` when `consumer_kind` does not map to a known `ConsumerKind` variant). This is
the least-event-volume design that still gives ADR-081 per-target — and therefore per-substrate
(target id → owning substrate) — granularity at one dispatch per assembly.

**Feedback path.** The response lists every slice with `{id, source_pack, score_semantics}` and the
assembly-level `served_by_profile_id`, so the caller (or the ADR-081 out-of-band scorer) emits
`brain.auto_feedback(query=…, results=[{id}], served_by_profile_id=…)` per served target — the
existing recall→auto_feedback pattern, unchanged (`brain.auto_feedback` credits the first object's id
per call, so this means one call per slice id, not a batch call).

**Reject one-record-per-slice as a separate design axis.** It is in fact what the ledger schema
requires (one row per target); what this ADR avoids multiplying is the _dispatch_, not the row: one
`brain.record_serve` dispatch carrying all ids writes all rows, rather than one dispatch per slice.

**Profile resolution, once at composer level.** The composer is a distinct serve surface with its own
`consumer_kind`; resolving one profile per assembly (rather than per contributor) is what lets
ADR-081 learn _this surface's_ budget-split usefulness (F3/§4 v2). Stamping a composer-level profile
onto memory-origin or kg-origin targets means those ledger rows attribute to the composer's profile,
not to `memory.recall`'s own recall profile — this is intended: the composer is a different
`consumer_kind`, and conflating its serves with direct `memory.recall` serves would corrupt both
signals.

### 7. Response shape

    {
      "slices": [
        { "source_pack": "kg", "kind": "entity", "id": "…", "score": 0.91,
          "score_semantics": "graph_proximity", "content": { "entity": {…}, "neighbors": [ … ] } },
        { "source_pack": "memory", "kind": "memory", "id": "…", "score": 0.78,
          "score_semantics": "decay_weighted", "content": "…" }
      ],
      "served_by_profile_id": null,
      "consumer_kind": "prefetch",
      "truncated": false,
      "dropped": { "slices": 0 },
      "skipped": []
    }

## Alternatives considered

- **A — pack-blind substrate retrieval.** Rejected: discards pack rankers and ADR-081 attribution.
- **B — composer dispatches MCP verbs with hardcoded adapters.** Rejected: verb-name + response-shape
  coupling; the discover-many-and-consume case reintroduces it even via in-process `registry.dispatch`
  and re-enters the gate + re-mints a token per contributor
  (`crates/khive-runtime/src/pack.rs:902-1012` runs on every `registry.dispatch`).
- **Runtime built-in (pack-less) verb.** Rejected: no such mechanism exists — every verb is
  pack-owned and dispatched by iterating packs (`verbs()` itself is a kg handler). A built-in would
  need dispatch-loop special-casing, violating ADR-017's no-special-casing rule; it is more surface,
  not less.
- **Housing the verb in the session pack.** Considered and close: session pack exists and
  `REQUIRES=["kg"]`. Rejected for API coherence — the session pack owns session-record verbs
  (store/list/resume/export of `kind=session` notes); a cross-substrate assembler is a categorical
  stranger there. A dedicated pack is the honest home; the verb can relocate by rename if this is
  reversed.
- **Forced 0..1 score normalization.** Rejected: scores are partition-internal and never fused;
  normalization would imply false comparability.
- **One serve-ledger dispatch per slice.** Rejected: multiplies dispatch/event volume; one
  `brain.record_serve` dispatch carrying all target ids already produces the same per-target ledger
  rows the schema requires.
- **Two-phase bid (cheap estimates first) for v1.** Deferred to v2: doubles contributor invocations
  and reader acquisitions; mandatory reflow already recovers unused budget from one round.

## Consequences

- One additive optional `PackRuntime` method (default `None`); one new object-safe trait + three
  types in `khive-runtime`; one registry accessor; one new pack crate with one verb; kg/memory
  (/knowledge) contributors. No new edge relations, no schema migration, request-only MCP surface
  (ADR-016) unchanged.
- One small addition to `khive-pack-brain`: a thin `brain.record_serve` verb wrapping the existing
  (currently unwired) ADR-081 §4 write function, deriving `namespace`/row `id`/`query_class` and
  looping over `target_ids`. This is new verb surface, scoped to this ADR's implementation PR, not a
  schema change.
- One small addition to `khive-brain-core`: a new `ConsumerKind::Context` variant
  (`crates/khive-brain-core/src/profile.rs:31`), added the same ADR-058-conformant way `Recall` and
  `KnowledgeCompose` were — this is what lets `served_by_profile_id` resolve for the composer's
  default `consumer_kind` at all; it does not open the closed-enum resolver to arbitrary strings.
- The per-turn prefetch hook can replace its caller-side recall+search assembly with one
  `context.assemble` call — gated on measured wall-time beating the caller-side chain (ADR-089's 2.2s
  baseline is the comparator; if it does not beat it, the verb does not ship). This measured
  comparison against the caller-side chain is required acceptance evidence for this ADR's
  implementation PR, exactly as it is for ADR-089.
- ADR-081 gains a second serve surface (`consumer_kind="context"`/caller-supplied) whose per-target
  ledger rows enable future learned budget allocation.
- ADR-023 surface amendment: the verb catalog gains `context.assemble` (and `brain.record_serve`);
  AGENTS.md and the verb reference document them. kg's `context` (ADR-089) and `context.assemble`
  share the graph-assembly implementation (one internal helper); two surfaces, one code path.

## Provenance note

This ADR's Option-C shape, trait design, and budget/execution/attribution model were hardened through
an internal design review before drafting. Two claims from that review did not survive verification
against the source and are corrected here rather than carried forward: (1) the review's cited
"`brain.record_serve` already accepts a `target_ids` array" does not exist in the code — `record_serve`
takes a single `target_id` and is not yet wired to any verb; §6 above specifies the small addition
needed instead of asserting it as already shipped. (2) the review's cited `recall.rs:589`
"`resolve_serving_profile`" helper does not exist under that name or in that file; the real, reusable
three-tier helper is `khive_brain_core::resolve_consumer_profile`
(`crates/khive-brain-core/src/profile.rs:118`), which §6 cites correctly. All other file:line
citations underpinning this ADR (`pack.rs:141`, `pack.rs:1014`, `pack.rs:902-1012`,
`handler_defs.rs:692`, `feedback.rs:108`) were verified against the worktree at time of writing.
