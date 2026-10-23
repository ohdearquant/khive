# ADR-201: Brain module boundaries and shared event readers

- Status: Proposed
- Date: 2026-10-11
- Relates to: #4510, #4554, #3709
- Source baseline: `94cc01b13d88ee1d72c4e1b68830fdd4a21ed8af`

## Context

The brain pack combines registration, profile administration, feedback intake,
event-plane queries, persistence, replay and dispatch coordination in a few
modules. Those concerns have different owners even when they share a file.
In particular, reading stored public events does not require interpreting them
as training signals or changing posterior state.

Some separation already exists. Runtime owns actor-scope resolution, cursor
walking and typed event pagination. Brain has separate selected-result feedback,
transactional feedback persistence and section-feedback modules. This decision
uses those boundaries and preserves their behavior.

The event handlers have a less obvious dependency: `BrainPack::dispatch` takes
the dispatch gate, calls `ensure_loaded`, and drains queued hook signals before
selecting **any** handler, including event reads. Moving a query body does not
authorize removing that prelude or its cold-load errors.

This is an additive ownership decision. It does not supersede the profile,
attribution or event contracts in ADR-032, ADR-081, ADR-103 or ADR-104. It does
not adopt proposed amendments to those records. ADR-171 remains a separate
Proposed process/storage split: this work does not create a brain daemon, change
event carriers, move database tables or make feedback eventually consistent.

## Decision

### 1. Organize brain implementation by concern, preserving its facade

Keep `BrainPack` and existing public import paths. Definitions may move behind
facade re-exports; public signatures, constructors and trait implementations
remain available at their current paths. The target organization is:

| Concern                     | Implementation home                                                                                             | Boundary retained                                                                                 |
| --------------------------- | --------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| Verb metadata               | `handlers/definitions.rs`                                                                                       | Existing handler order, names, visibility, parameter metadata and `handlers::BRAIN_HANDLERS` path |
| Profile reads and lifecycle | `handlers/profiles.rs` and `handlers/bindings.rs`                                                               | Existing profile validation and mutation coordinator; event actor policy stays distinct           |
| Feedback intake             | `handlers/feedback.rs`, existing `handlers/auto_feedback.rs`                                                    | Validation, target resolution, attribution and route selection in their current order             |
| Adapter registration        | `handlers/adapters.rs`                                                                                          | Existing revision checks and KG artifact write, separate from profile snapshot mutation           |
| Durable state and mutation  | `persist.rs` facade with private `persist/storage.rs` and `persist/mutation.rs`; existing `persist/feedback.rs` | One transaction owner per mutation and current publication-after-commit boundary                  |
| Tracker and replay          | Private `persist/tracker.rs`, `persist/replay.rs`, `persist/load.rs`                                            | Namespace parking, queue order, snapshot generation checks and replay policy                      |
| Hooks and dispatch          | Private `hook_queue.rs` and `hook_signals.rs`; existing dispatch facade                                         | Shared pack identity, gate ownership, queue election and complete runtime forwarding              |
| Stored event reads          | Private runtime `event_plane_read` module and its children                                                      | Compatibility readers described below; brain retains its verbs and dispatch prelude               |

Keep the factory and generic `BrainPackRuntime` forwarder in `pack.rs` for this
program. Its source-inventory test reads that file to check complete trait
forwarding. Moving the forwarder is unnecessary to separate queue and signal
logic, and would require an additional explicit test-path change.

Keep `section_feedback.rs` as the distinct trusted, target-free capability. Do
not turn it into an arbitrary-target public verb. Keep `event.rs` as the single
event-to-`BrainSignal` interpreter used by training paths. Optional router code
may move into its own module with the same feature gate and call ordering.

### 2. Runtime owns the complete stored-event compatibility readers

Select **khive-runtime**, not a new pack or crate, as the home for the existing
event query orchestration and stored-event projection. Runtime already owns the
scoped event store, actor resolver and pagination mechanisms. Its private
`event_plane_read` module owns recent-event selection, count-window traversal,
the count accumulator and page-response formatting.

Expose only these thin methods on `KhiveRuntime`:

```rust
pub async fn read_brain_events(
    &self, token: &NamespaceToken, params: serde_json::Value,
) -> Result<serde_json::Value, RuntimeError>;

pub async fn read_event_counts(
    &self, token: &NamespaceToken, params: serde_json::Value,
) -> Result<serde_json::Value, RuntimeError>;

pub async fn read_event_page_response(
    &self, token: &NamespaceToken, params: serde_json::Value,
) -> Result<serde_json::Value, RuntimeError>;
```

These methods preserve the existing brain event-read request and response
contracts. `read_brain_events` retains its specific debug verb filter; its name
does not imply an unrestricted recent-event query. The two other methods are
compatibility adapters over the existing typed event mechanisms, not a new
query language. They register no new verbs and grant no additional namespace or
actor access. They take the dispatch-authorized token; namespace fields in raw
JSON must never manufacture or widen that token.

The brain handlers delegate after the **unchanged** gate/load/queue prelude.
Brain continues to own the `brain.*` registration, visibility and parameter
metadata. Runtime performs the same parsing, policy checks, reads and formatting
that the handler bodies perform today, in the same order. It uses the existing
serving `brain.fleet_readers` configuration through the shared actor resolver;
there is no replacement allowlist or alternate default actor rule.

The stored-event projection includes profile, originating-verb, signal,
work-class and cost fields already present in event payloads. It is read-only
interpretation of the public event schema, not posterior training. Move that
accumulator and its tests with the readers; do not retain a second accumulator
in brain. At the stated baseline `event_counts_grouping.rs` still imports
`crate::handlers::{event_cost_unit, event_work_class}` and calls
`BrainPack::truncatable_total_key`; E2 severs that coupling by moving those three
helpers into runtime's private `event_plane_read` module with the accumulator, so
the moved implementation uses only storage events, shared event kinds and JSON
values. Runtime must not depend on `khive-pack-brain` or import its state,
interpreter, fold or persistence types.

Reuse existing `page_events`, actor-scope and cursor-walk helpers. Keep them as
the mechanism owners rather than copying their algorithms into the new module.
No new storage trait, SQL query family, service trait or generic plugin layer is
needed for this move. The new callable Rust methods are an additive API boundary;
their introduction must be reviewed explicitly, even though wire behavior stays
the same.

Telemetry continues to read its durable stream/sequence carriers. That surface
has different paging and filtering semantics and is not an interchangeable
EventStore consumer. This ADR does not move telemetry onto the brain response
schema or claim that the two entire surfaces have been deduplicated.

### 3. Preserve atomicity, loading and observation semantics

The following are invariants of every phase:

- Preserve lock order, await order and the scope of `dispatch_gate`. Preserve the
  factory's shared `Arc<BrainPack>` between hook and runtime wrapper, complete
  trait forwarding, and queued-signal ordering.
- Keep feedback's durable-generation rebase, lifecycle recheck, dedup claim,
  mass gate, public-event append, grade backfill, private-event append and
  snapshot write in the same existing atomic unit. Publish proposed state only
  after commit. A helper receiving a held writer must not acquire another one.
- Preserve replay's namespace, timestamp ordering, quarantine versus policy
  exclusion, effective weights and snapshot/replay/cold-signal sequence. Public
  event pagination is not a replacement for private brain-log replay.
- Preserve request validation order, error variants and strings, actor alias
  handling, exact namespace scope, time parsing and half-open windows. Keep
  audit/non-audit admission budgets separate and preserve current caps.
- Keep bounded and exhaustive reads' live-view consistency. Their counts and
  returned rows need not describe one snapshot. Do not claim otherwise or add
  a transaction while relocating them.
- Preserve which rows feed payload and crossed aggregates, signed saturating
  cost addition order, optional-field omission, truncation key names, cursor
  tie handling and the final serialized page-response size check.

The grouped admission change in #5150 is a separate adoption. If it lands before
a move, relocate the resulting implementation and tests; do not reintroduce the
old scalar admission queries. Never replace admitted-row payload/cross folds
with full-window SQL aggregates. This ADR does not waive #5150's own validation.

### 4. Retain the public pure folds

Keep `fold::{BalancedRecallFold, SectionPosteriorFold}` and their documented
constructors and trait implementations. They have no direct production caller
inside this repository at the stated baseline, but they are public and documented
for embedders. Their supplied-state reduction also differs from live
profile-routing and transaction coordination.

No removal, deprecation or substitution into the live paths is part of this
split. Any later retirement needs its own compatibility decision. Keep
`fold_gate` and `serve_ledger` public paths and transactional responsibilities;
score/decay consolidation proceeds under its own numerical contract.

## Rollout and review units

Rollout batches by crate: the B units land as one `khive-pack-brain` pull request
(two only if B4 requires it), and E1 with E2 as one `khive-runtime` pull request;
the table below remains the review map for those pull requests.

The table below identifies review units and their dependencies, not a mandatory
one-PR-per-unit schedule. Related units and issues may share one PR when their
implementation and validation form a coherent change. Batch those units to avoid
repeating the same review and integration work; retain acceptance evidence for
each issue. Split a batch where dependencies or unrelated behavior require it. Overlapping changes such as #5150 and
#5177 must land or be reconciled on the selected implementation base before their
files move. A module extraction moves complete existing bodies and introduces
only the imports, module declarations, internal visibility and facade wiring
needed to keep them callable. Do not mix algorithm changes into an extraction.

| Review unit                | Planned scope and size qualification                                                                                                                                                                                                                                                                                                                                                |
| -------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| B1: registration           | Move the intact handler table and preserve its old facade. The baseline table is 691 source lines, so an ordinary addition/deletion diff is at least 1,382 lines before wiring. Keep this coherent table together rather than rewrite its metadata to shrink the stat.                                                                                                              |
| B2a: state/config reads    | Move state and config reads; leave profile reads for B2b and mutation bodies for B3.                                                                                                                                                                                                                                                                                                |
| B2b: profile reads         | Move profile listing, individual profile reads and resolution. Report the exact moved spans and diff stat at implementation.                                                                                                                                                                                                                                                        |
| B3: lifecycle and bindings | Group lifecycle/reset and binding/create methods by shared mutation coordinator; each method moves intact and keeps the existing mutation coordinator.                                                                                                                                                                                                                              |
| B4: feedback intake        | Move intake, its compatibility alias and target helper. The baseline `handle_feedback_from` body alone is 549 lines, implying at least 1,098 changed lines before its surrounding adapters. Preserve this atomic review unit; no algorithmic decomposition is authorized by the size goal.                                                                                          |
| B5: tracker                | Move tracker and parked/cold queue ownership behind the existing `persist` facade; retain public type paths.                                                                                                                                                                                                                                                                        |
| B6: persistence            | Keep durable storage helpers and generic mutation coordination distinguishable, retaining `persist/feedback.rs` as the authoritative feedback unit.                                                                                                                                                                                                                                 |
| B7: replay and loading     | Move the replay loader and load coordinator as identifiable units, with their relevant existing test bodies and compatibility exports.                                                                                                                                                                                                                                              |
| B8: hook queue and routing | Move queue and signal-routing code; retain factory, forwarder and their source-inventory anchor in `pack.rs`.                                                                                                                                                                                                                                                                       |
| B9: adapters/router        | Keep adapter registration and the optional router as distinct units; preserve the router feature gate and all existing behavior.                                                                                                                                                                                                                                                    |
| E1: recent events          | Introduce the runtime module and `read_brain_events`; delegate the existing brain handler under its unchanged prelude.                                                                                                                                                                                                                                                              |
| E2: count and page readers | Move the count accumulator, window orchestration, shared time parser and both response projections as one coupled unit with their pure controls. Add `read_event_counts` and `read_event_page_response`; retain both brain adapters, metadata and dispatch tests. Reuse runtime `page_events`. No duplicate production implementation or temporary public parser bridge may remain. |

The #4554 estimate of two PRs below approximately 500 changed lines cannot be
promised for a literal whole-surface move with tests. This ADR proposes the two
event-reader units above. Count and page readers share the private
`parse_rfc3339_micros` helper, so E2 moves them together to remain independently
buildable without a temporary public parser or duplicate implementation. E2 may
exceed that estimate when its coupled helpers and controls move together. B1 and B4 likewise have the explicit lower
bounds shown above. These are **proposed packaging exceptions**, not already
approved implementation permissions. Review of this ADR must explicitly settle
that packaging before those follow-ups proceed. If it is rejected, revise the
phase plan; do not silently omit tests, copy implementations or split an atomic
algorithm merely to meet a nominal line budget.

Every implementation PR must publish its actual `git diff --stat` and a mapping
of old and new code blocks. A rename statistic alone is insufficient: reviewers
must compare bodies, metadata and ordering. Existing test assertions, fixtures
and negative controls remain unchanged. Mechanical module/import/include-path
adjustments needed to find moved code are allowed only when identified in that
mapping; source-inventory checks must still inspect the actual production code.
That is the precise meaning of “tests unchanged” for this proposal. No assertion
may be weakened to accommodate a move.

## Acceptance

Ratification of this record decides ownership and the qualified rollout; it does
not demonstrate that the moves or #4554 adoption have happened. Each follow-up
must retain public Rust imports, handler metadata and feature combinations, and
run the relevant existing brain and runtime tests on its final integrated head.

For event moves, retain real dispatch controls for actor/namespace refusal,
default-alias coalescing, time boundaries, dense cursor ties, separate audit
budgets, exhaustive limits, payload/cross aggregation, cost saturation and page
response size. Keep tests proving the brain load/gate prelude and its error
ordering. Pure moved controls follow the mechanism into runtime; pack integration
controls remain with the brain facade. Any test-only export needed by an existing
cross-crate control must remain feature-gated, with no new production verb.

For state moves, retain feedback rollback/commit publication, concurrent dedup,
generation rebase, namespace interleaving, cold queued evidence, quarantine and
full-forwarder controls. Retain the pure fold examples and tests.

Existing source/spec discrepancies, including ADR-104 amendment language versus
shipped `auto_feedback` abstention, are separate reconciliation work. Neither
side of such a discrepancy is silently implemented during relocation.

## Consequences

The pack remains the owner of profile state, training and its verb catalog.
Runtime becomes the single owner of the stored-event compatibility readers,
without a dependency back to the pack. Existing clients retain their interfaces
and failure behavior. The cost is a small additional runtime Rust API and several
mechanical integration steps; some intact moves have large textual diffs even
though their algorithms do not change.

## References

- [ADR-032: Brain as Profile-Orchestration over Fold + Objective](ADR-032-brain-profile-orchestration.md)
- [ADR-081: Recall Retune Driver — Governed Ingestion of Implicit Feedback](ADR-081-recall-retune-driver.md)
- [ADR-103: Resource Attribution Model](ADR-103-resource-attribution-model.md)
- [ADR-104: Profile Posteriors in the Recall Read Path](ADR-104-posterior-serving-recall.md)
- [ADR-171: Brain daemon — profile state leaves the domain process](ADR-171-brain-daemon-split.md)
- `crates/khive-pack-brain/src/handlers.rs`, `persist.rs`, `persist/feedback.rs`,
  `pack.rs`, `pack_runtime_tests.rs`, `event_counts_grouping.rs`, `fold.rs`
- `crates/khive-runtime/src/event_actor_scope.rs`, `event_cursor_walk.rs`,
  `event_page.rs`

## Amendment 1 (2026-10-23) -- implementation base and packaging

**Status**: Proposed 2026-10-23. This amendment requires specification approval. It settles the packaging that the
base text leaves to review ("Review of this ADR must explicitly settle that packaging before those follow-ups
proceed") and moves the implementation base forward.

**Addresses**: [#4510](https://github.com/ohdearquant/khive/issues/4510),
[#4554](https://github.com/ohdearquant/khive/issues/4554).

Line references are to `dev/khive` at `698d5812` unless they name this record.

### Decision 1 -- the implementation base is the current tree

The source baseline `94cc01b1` predates `63e78032e` (#5609), which split `handlers.rs` into submodules under the
file-size rules of #5605. The `impl BrainPack` block that `handlers.rs` held at the baseline is now
`crates/khive-pack-brain/src/handlers/brain_handlers.rs:12` (a 2,049-line file), the event count helpers are
`handlers/event_counts.rs`, and the router is `handlers/router.rs`. The pack's other four `impl BrainPack` blocks are
in `handlers/auto_feedback.rs`, `handlers/event_page.rs`, `pack.rs` and `section_feedback.rs`, as they were. `handlers.rs`
keeps the handler table (`handlers.rs:57-757`), the dispatch prelude and the module declarations. #5609 moved code
verbatim by file size; it does not implement any home in section 1, which stays the plan.

Each move in section 1 starts from the file that holds the code on its implementation base, and the block mapping its
pull request publishes names that file. #5150 (`978caa14f`) and #5177 (`1ccae15b1`) are merged and are part of the
base, so the overlap condition in "Rollout and review units" is met. #3709 is closed.

### Decision 2 -- packaging is approved on measured floors

The proposed packaging exceptions are approved:

- B1: the handler table is 701 lines (`handlers.rs:57-757`), so its floor is 1,402 changed lines before wiring. This
  replaces the 691-line table and 1,382-line floor in the B1 row.
- B4: `handle_feedback_from` is 549 lines (`handlers/brain_handlers.rs:864-1412`), so the 1,098-line floor in the B4 row
  holds; the body is now in `brain_handlers.rs`.
- E1 and E2 land as one `khive-runtime` pull request at their measured size. For that pull request this supersedes the
  budget of approximately 500 changed lines in #4554.

Each implementation pull request re-measures its floor on its own base and states it beside the `git diff --stat` and
block mapping the base text requires. The B units land as one `khive-pack-brain` pull request by default; the general
split rule ("Split a batch where dependencies or unrelated behavior require it") is the only ground for a second, and B4
is one case of it.

For #4510's "moves only, tests unchanged", the meaning is the one the base text gives: module, import and include-path
adjustments named in the block mapping are allowed, and no assertion, fixture or negative control changes.

### Decision 3 -- the grouped count is already adopted for window totals

#4554 asks that `brain.event_counts` use a grouped count from khive-db rather than per-group queries. #5150 did this for
the window totals: `grouped_window_total` and the unfiltered audit and non-audit totals call `count_events_grouped`
(`handlers/event_counts.rs:40-51, 88-97`). Payload and crossed aggregates keep folding the admitted rows, as section 3
requires ("Never replace admitted-row payload/cross folds with full-window SQL aggregates"). So #4554's second pull
request has no grouped-count work left under this record, and E2 moves the grouped totals and the admitted-row folds as
they are.

### Decision 4 -- corrections

- "Count and page readers share the private `parse_rfc3339_micros` helper": the helper is `pub(super)`
  (`handlers/event_counts.rs:298`), visible inside `handlers` and not outside it. The reason E2 moves both readers
  together holds.
- The References entry for `handlers.rs` also covers `handlers/brain_handlers.rs` and `handlers/event_counts.rs`, which
  hold the bodies this record moves.

### Acceptance

| ID   | Statement                                                                                                                            | Evidence                       |
| ---- | ------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------ |
| A1.1 | The B pull request states the B1 and B4 floors re-measured on its base, with the file each block moves from.                         | the pull request body          |
| A1.2 | E1 and E2 land as one `khive-runtime` pull request with its diff stat and block mapping.                                             | the pull request body          |
| A1.3 | After E2, `brain.event_counts` window totals still call `count_events_grouped`, and no brain-side copy of the moved readers remains. | source grep on the merged head |
