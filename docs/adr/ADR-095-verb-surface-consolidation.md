# ADR-095: Verb-Surface Consolidation and Field-Validation Governance

**Status**: Proposed
**Date**: 2026-07-04
**Authors**: khive maintainers
**Depends on**: ADR-016 (request DSL, single-tool contract), ADR-017 (pack standard),
ADR-023 (pack verb surface, visibility, and composition), ADR-083 (session pack T1 verbs),
ADR-084 (verb-surface
consistency contract)
**Scope**: the proposal-time multi-pack MCP surface; the KindHook create-time extension
seam; per-verb field validation (status enums, memory_type enums, datetime checks). Out
of scope: brain and knowledge pack-private storage, closed taxonomies.
**Implementation state (2026-10-23)**: the F2 `gtd.assign` /
`create(kind="note", note_kind="task")` create-parity slice is shipped through a shared task-create
pipeline. The update-time KindHook seam is implemented and used by three packs; the GTD transition guard remains
in the GTD handler (corrected by Amendment 1, Decision 2).

## Context

Issue [#1730](https://github.com/ohdearquant/khive/issues/1730) tracks a design request with two goals: (a)
consolidate specialized-pack CRUD-shaped verbs (the example given was `session.resume`) into the kg pack's
dispatch-by-kind pattern (`search(kind="session", ...)`), and (b) centralize EDGE_RULES
validation so endpoint rules are "bundled and compiled together, instead of per verb
enforcement."

Two facts reframe the directive before any design begins.

First, part (b) is already shipped. PR #621 (merged 2026-07-04) centralized endpoint validation. Endpoint
acceptance flows through a single validator, `validate_edge_relation_endpoints`
(`crates/khive-runtime/src/operations/link_ops.rs:47-53`), which consults runtime-composed pack rules
(`crates/khive-runtime/src/operations/link_ops.rs:190-202`). The EDGE_RULES half of the directive is already
shipped (#621), so this ADR records that state without reopening it.

Second, the still-per-verb validation layer that the centralization goal actually
lands on is not edge endpoints. It is KindHook-shaped field validation: the status-enum
checks in gtd, the memory_type enum in memory, the RFC-3339 datetime checks in schedule,
each living inside its own pack handler with no composed-registry equivalent. That is the
real target of part (b), and F3 below decides it.

The verb/single-tool contract is ADR-016 (Request DSL), not ADR-015 (schema migrations in
this repo).

### Proposal-time surface inventory

The 2026-07-04 snapshot counted 74 user-facing verbs across eight packs (corrected by Amendment 1, Decision 5;
#624, #692). This count is historical evidence for the alternatives considered here, not current surface authority.
The live authority is each pack's handler declaration, composed by the registry; the smoke contract checks that the
reported `total` equals the returned verb list length rather than pinning a repository-wide literal. This ADR's
delivered F2 work changes neither handler declaration involved, so its wire delta is zero without restating a mutable
global count.

The `request` tool description enumerates the verb catalog. That enumeration is the primary
discoverability surface an agent caller sees: the list of named verbs is how an agent knows
what the server can do. This matters for the refute analysis below.

### What already exists that a fold would use

The KindHook trait (`crates/khive-runtime/src/pack/traits.rs:471-612`) is the shipped extension
seam for per-kind create behavior. It has two create lifecycle methods: `prepare_create` (`:477-481`) runs
before the storage write and can normalize or reject fields; `after_create` (`:491-498`) runs
after the write for side effects. The kg create handler resolves hooks at
`crates/khive-pack-kg/src/handlers/create.rs:916-950`, calls preparation at
`:1007-1018`, and calls `after_create` at `:1201-1208`. The latter is best-effort: a failure
is logged and swallowed because the storage write is already committed.

gtd already ships a production KindHook. `TaskHook`
(`crates/khive-pack-gtd/src/hook.rs:353-398`) implements both methods; its
`after_create` delegates dependency-edge work at `:380-398`.

This is decisive for the mechanism forks. The existing KindHook seam supports per-kind create behavior
(`crates/khive-runtime/src/pack/traits.rs:471-498`), and TaskHook implements it
(`crates/khive-pack-gtd/src/hook.rs:353-398`). Folding internal paths can therefore reuse an existing extension point.

At proposal time, `gtd.assign` and `create(kind="note", note_kind="task")` were separate normalization,
validation, and post-write edge paths. The shipped implementation keeps their public and
storage entry points distinct but removes the semantic fork: both construct
`TaskCreateInput`, call `prepare_task_create`, and use `link_depends_on_edges`. The
`gtd.assign` handler still calls `create_note_with_options` or
`create_note_with_embedding_content_and_report` (`crates/khive-pack-gtd/src/handlers.rs:1178-1196,1222-1238`) to
preserve its response shape; the generic path reaches the same shared pipeline through `TaskHook`. This helper-based
adaptation is the live meaning of internal-path unification in this ADR.

The gtd state machine must survive any change. `atomic_gtd_transition`
(`crates/khive-pack-gtd/src/handlers.rs:746-775`) is a
single atomic conditional UPDATE keyed on the properties status via json_extract, where
`rows_affected == 0` means the caller lost a concurrent transition race. `can_transition`
(`crates/khive-pack-gtd/src/schema.rs:103-105`) is the validated state-machine guard.
Neither has an equivalent in the kg generic verbs. Any fold that touched task lifecycle
would have to reproduce this exactly, and none should.

## Decision

The design answer is: half of this is done, a quarter should not be done, and here
is the quarter worth doing.

- The EDGE_RULES half (part b as literally stated) is shipped (#621). This ADR records that
  and does not reopen it.
- The verb-retirement quarter (fold named pack verbs into dispatch-by-kind and remove them
  from the surface) should not be done. It breaks a decision accepted 48 hours
  earlier (ADR-083), it degrades agent discoverability, and it buys a deduplication benefit
  that the existing KindHook seam already delivers without touching the wire.
- The quarter worth doing is internal, not wire-facing (superseded by Amendment 1, Decision 1): (1) make
  dispatch-by-kind plus KindHook the mandatory internal pattern for future packs and for collapsing existing
  duplicate write paths; (2) a governance rule that per-kind create-time field validation
  lives in KindHook, not in bespoke parallel handlers; (3) name, but do not build, an
  update-time validation seam as future work.

Per-fork decisions follow.

### F1 Consolidation scope: decision (d) plus a narrow internal (c), reject (a) and (b)

Decision: (d) keep the surface and codify dispatch-by-kind as the mandatory pattern for
future packs (superseded by Amendment 1, Decision 1), augmented by a narrow, non-breaking internal fold that
routes existing duplicate write paths through the shared create-plus-KindHook seam. Reject (a) session-only
retirement and (b) all-crud-dup retirement.

Argument for retirement (the case against this decision): named pack verbs that only wrap
entity or note substrate operations are redundant with `create`, `search`, `list`, `get`,
`update`, `delete` dispatched by kind. The session pack's 4 verbs are substrate-native with
near-zero domain logic, so folding them removes 4 verbs at near-zero semantic cost, and a
smaller catalog is easier to learn.

Argument against retirement (why it loses): three independent reasons.

1. ADR-083 (accepted 2026-07-02, GitHub #342) deliberately made the session verbs
   agent-facing and specialized their names. It renamed `session.get` to `session.resume`,
   promoted `session.export` to a dispatchable `Visibility::Verb`, and set params to
   provider and provider_session_id, all 4 verbs agent-facing. Its stated rationale is that
   "resume names the continuity use case directly: fetching a session in order to continue
   it," where "get is a generic accessor name shared with unrelated substrate operations,"
   and that continuity requires "the verbs to be callable from the agent-facing MCP request
   surface." Folding `session.resume` back into `search(kind="session")` is the exact inverse
   of that decision, proposed 48 hours after it was accepted, and ADR-083 names session continuity as the reason
   for its agent-facing verbs (`docs/adr/ADR-083-session-pack-t1-verbs.md:29-34`). Reopening an accepted decision
   within two days needs a stronger reason than surface tidiness, and there is none.

2. Named verbs are self-documenting for agent callers; dispatch-by-kind pushes semantics into
   a `kind` parameter the agent must know to pass. The `request` tool description enumerates
   the named catalog, so each retirement removes a line from the discoverability surface and
   replaces it with knowledge the agent has to hold out of band. For a product whose callers
   are LLM agents, the named surface is a feature, not clutter.

3. The one concrete benefit retirement would buy, less duplicated CRUD code, is available
   without retiring anything. The duplicate is `gtd.assign`'s direct note-write path versus
   `create(kind="note", note_kind="task")`+TaskHook (see Context). Routing the verb's internal implementation
   through the shared seam removes the duplication while keeping the named verb as the public
   entry point. That is the narrow internal (c): fold the code path, not the wire surface.

Alternatives considered for F1:

- (a) session-only retirement: rejected, directly reverses ADR-083 (see above).
- (b) all-crud-dup retirement: rejected for the same discoverability and ADR-083 reasons,
  at larger blast radius.
- (c) as wire-facing crud-plus fold (retire pack verbs, re-express via kg-verb kind
  profiles): rejected as a wire change for the same reasons as (a) and (b); accepted only in
  its internal form, where the named verb stays and its implementation is unified.

### F2 Mechanism: decision (c) realized as internal-path unification, reject (a) and (b)

Decision: where any consolidation happens it is (c) pack-declared kind behavior via the
existing KindHook seam, realized as internal-path unification. The named verb remains the
public entry point. Its handler and the kind hook may remain separate storage entry points
when their wire responses differ, but they MUST share the same normalization, validation,
and side-effect helpers, leaving one semantic create pipeline per kind. Reject (a) hard
removal and (b) alias-and-deprecate as a durable state.

Argument for (a) hard removal: it is the only mechanism that actually shrinks the surface, so
if surface size were the goal it would be the honest choice. It loses because F1 already
rejected shrinking the surface: hard removal is a breaking wire change against existing callers, the
marketplace plugin users, and ADR-083, for a goal this ADR does not adopt.

Argument for (b) alias-and-deprecate (pack verbs become thin aliases over kg handlers, count
unchanged short term): it preserves the wire while promising eventual simplification. It loses
on the refute mandate's exact point: an alias that is never removed is a permanent double
surface, two names for one operation, which is strictly worse than either committing to the
named verb or committing to dispatch-by-kind. It doubles the discoverability catalog with
synonyms an agent must disambiguate, and it adds a redirection layer with no endpoint. A
double surface with no scheduled collapse is the worst of both worlds.

Argument for (c) via KindHook (the decision): the seam exists and is proven in production.
The delivered adaptation extracts the task-specific behavior below the two entry points
rather than making `gtd.assign` invoke the generic handler. It has zero wire change and
zero count change, while centralizing task field logic in one shared module. It is the
FindExisting answer (PI_AEP): the framework seam is adopted and its domain logic is shared.

Hard constraint honored: `prepare_task_create` resolves every `depends_on` target through
`resolve_primary` before either entry point writes, so neither path can leave an orphaned
task after a validation failure. `TaskHook::prepare_create` and `handle_assign` call that
same function; neither retains a parallel guard. The `atomic_gtd_transition` state machine
is lifecycle logic outside create; the transition path is not folded and its atomic guard
is untouched.

**Implementation evidence.** The shared input and normalization/validation pipeline are
in `crates/khive-pack-gtd/src/task_create.rs:272-370,452-485`;
`TaskHook` delegates to it at `crates/khive-pack-gtd/src/hook.rs:353-398`, while
`gtd.assign` delegates at `crates/khive-pack-gtd/src/handlers.rs:1128-1158,1243-1258`. Equivalence
regressions covering normalized properties, `depends_on`/`annotates` edges, and malformed
`context_entity_id` rejection are in
`crates/khive-pack-gtd/tests/dependencies.rs:461-462,664-689`.

Alternatives considered for F2 are the three fork options themselves; (a) and (b) are rejected
above.

### F3 Validation layer: decision (b) status quo plus a governance rule, reject (a) and (c)

Decision: (b) keep per-handler field validation as the enforcement mechanism, but add a
governance rule that new per-kind create-time field validation MUST live in
`KindHook::prepare_create`, not in a bespoke parallel handler. Do not build a composed
field-validation registry (a) and do not build per-kind JSON-schema validation (c).

Argument for (a) a composed field-validation registry analogous to EDGE_RULES (packs declare
per-kind field rules, runtime enforces at one seam): it is symmetric with the edge-rule design
that just shipped, and symmetry is appealing. It loses on PI_AEP. The total field validation
across the entire default surface is roughly three packs' worth of enum-and-format checks:
gtd status and priority enums, memory memory_type enum, schedule RFC-3339 datetimes. Building a
declarative rule engine, a runtime enforcement seam, and a per-pack declaration format to
govern that volume is Create where FindExisting suffices. KindHook already is the composed
create-time seam. The gap is not a missing registry; it is that not every pack routes through
the seam it already has.

Argument for (c) per-kind JSON-schema in pack vocab, enforced generically: it would give
declarative, introspectable field contracts. It loses for the same volume reason as (a), plus
it introduces a schema language and a generic validator as new dependencies to express checks
that are small domain predicates in current code
(`crates/khive-pack-gtd/src/task_create.rs:281-325; crates/khive-pack-memory/src/handlers/common.rs:326-334;
crates/khive-pack-schedule/src/handlers.rs:55-68`), and it overlaps ADR-084's `schema` introspection verb
without coordinating with it.

Argument for (b) plus governance (the decision): the enum checks are cheapest expressed as
what they are, small guards in the pack's shared domain pipeline. The original divergence
risk is now closed by construction for task creation: both the named handler and KindHook
delegate to `prepare_task_create`. A governance rule requiring create-time field validation
to be shared through the kind hook path prevents the same fork from returning without new
registry machinery.

Future work, named not built (superseded by Amendment 1, Decision 2, which records the update seam as built):
there is no update-time equivalent of KindHook. Update field
validation and the gtd transition guard live in their own handlers. If a second pack ever needs
validated update-time field rules, a `KindHook::prepare_update` (or `validate_update`) seam is
the natural extension. This ADR names it as future work and explicitly does not build it now,
because one consumer (gtd transition) is not enough signal to design a shared seam, and its
atomic guard must not be perturbed speculatively.

**Update-time state.** The update-time KindHook seam is implemented: `normalize_note_update`, `validate_note_update`,
and `validate_entity_update` (`crates/khive-runtime/src/pack/traits.rs:526-612`). Runtime update preparation calls note
normalization and validation (`crates/khive-runtime/src/pack/catalog.rs:146-160`). GTD, memory, and git provide
update-hook implementations (`crates/khive-pack-gtd/src/hook.rs:400-420;
crates/khive-pack-memory/src/hook.rs:147-155; crates/khive-pack-git/src/hook.rs:396-420`). The GTD transition guard
remains handler-specific
(`crates/khive-pack-gtd/src/handlers.rs:746-775; crates/khive-pack-gtd/src/schema.rs:103-105`), and no GTD
transition guard is added to KindHook (corrected by Amendment 1, Decision 2).

Coordination: the ADR-084 base record is Proposed; Amendment 1 is Accepted and records that it is signed and merged
(`docs/adr/ADR-084-verb-surface-consistency.md:3,497-502`). Together, they specify the verb-surface-consistency
contract, including the ID-resolution ladder, no-silent-enum-coercion, help-schema fidelity, param-naming discipline,
substrate-symmetric field names, declared-vocabulary completeness, and the `schema` introspection verb. The `schema`
verb is shipped, but neither the base ADR nor Amendment 1 ships the complete contract. F3's governance rule is
consistent with ADR-084 Rule 2 (no silent enum coercion): validation that rejects rather than coerces belongs in one
declared place per kind. This ADR references ADR-084 for the surface contract and does not duplicate it (corrected by
Amendment 1, Decision 3).

### F4 Migration and wire-compat: clean no-op on the wire

Decision: because nothing is retired or added, there is no wire-facing migration. No
deprecation window is needed because no breaking change is made.

Argument for a deprecation window (the case for treating this as a migration): if we were
retiring verbs, pre-1.0 status and a known consumer set (operators plus marketplace plugin
users) would still argue for a clean break over a deprecation window, since the consumer set is
small and reachable and a permanent alias surface is the F2-rejected outcome. But this decision
retires nothing, so the question is moot for the wire.

The delivered code change under this ADR is the internal-path unification (F2); F3 is a
governance rule. The equivalence regressions below cover F2. Pack handler declarations and
the generated request-tool surface do not move under this ADR.

Alternatives considered for F4: deprecation window (moot, nothing deprecated) and clean break
(moot, nothing broken). The decision is the third option the fork did not enumerate: no wire
change at all.

## Consequences

Verb-count deltas:

- Net wire delta under this ADR: 0. No verb is added, aliased, or removed.
- The refuted retirement alternatives would have reduced the proposal-time surface; the
  alias alternative would have preserved the public count while permanently doubling an
  internal surface. This ADR takes none of them.

Surface touchpoints (none move under this ADR, listed to make the no-op explicit):

- Pack `HANDLERS` declarations: unchanged.
- Introspection's exact returned-list/total relationship: unchanged.
- The generated `request` tool description: unchanged.
- ADR-023 (pack verb surface, visibility, and composition) and AGENTS.md amendment lane:
  engaged only for the
  governance rule wording (F1/F3 codification of dispatch-by-kind plus KindHook as the
  mandatory pattern for future packs), which is a documentation amendment, not a surface change
  (superseded by Amendment 1, Decision 1).

Positive consequences:

- `gtd.assign` and the task KindHook share the task-create pipeline, removing the divergence
  risk between handlers.rs and hook.rs task field logic while preserving their public
  response shapes.
- Future packs have a written, mandatory internal pattern (dispatch-by-kind plus KindHook), reducing the
  chance new packs add crud-dup verbs that later invite exactly this consolidation question (superseded by
  Amendment 1, Decision 1).
- ADR-083's session decision, session continuity, and the named discoverability
  surface are all preserved.
- No breaking change ships to operators or plugin users.

Negative consequences and costs:

- The surface does not shrink. Anyone who wanted a smaller catalog does not get it. This is the
  deliberate trade: discoverability and stability over count reduction.
- The internal-path unification is real refactor work on live gtd code paths, gated by the
  atomic-transition and orphan-guard constraints, so it is careful work despite being a no-op
  on the wire.
- The update-time KindHook seam is implemented. GTD transition validation remains in the GTD handler
  (`crates/khive-pack-gtd/src/handlers.rs:746-775; crates/khive-pack-gtd/src/schema.rs:102-105`), as corrected by
  Amendment 1, Decision 2.

## Non-goals

- brain (14 verbs) and knowledge (19 verbs) are out of scope. They write to pack-private SQL
  tables and register no entity or note kinds, so dispatch-by-kind cannot express them without
  a schema migration that introduces new storage substrates. No generic substrate access verb is adopted here; the
  event-read portion is settled by ADR-201 Amendment 1
  (`docs/adr/ADR-201-brain-module-boundaries.md:61-115,262-321;
  [#1503](https://github.com/ohdearquant/khive/issues/1503)`).
- Closed taxonomies stay closed. This ADR adds no entity kind (ADR-001), no edge relation
  (ADR-002), and no note kind (ADR-013), and it introduces no new storage substrate.
- The EDGE_RULES centralization is not reopened. It shipped in #621. This ADR records it as
  done and builds on it rather than restating it.
- ADR-084's surface-consistency contract and `schema` verb are not duplicated. This ADR
  references them.

## Implementation and regression record (informative)

The wire is unchanged. The internal refactor and its regression coverage have shipped:

1. `handle_assign` and `TaskHook::prepare_create` now feed the same
   `prepare_task_create` pipeline. It owns status/priority normalization, pre-write
   `depends_on` validation, `context_entity_id` resolution and persistence, and the
   `annotates` target derived from that context.
2. Both entry points use `link_depends_on_edges` for post-write dependency edges. They
   retain their separate storage calls and response rendering; parity is below those
   boundaries, where the domain semantics live.
3. Equivalence regressions cover corresponding normalized fields, the required
   `depends_on`/`annotates` edges, and malformed-context rejection in both paths. The GTD
   transition path is not involved.
4. No verb was added, aliased, deprecated, or removed. This remains a wire no-op.

## Implementation and residual ledger

- **Shipped:** task-create path unification and equivalence coverage (F2).
- **Shipped before this ADR:** composed EDGE_RULES validation (#621); this ADR does not
  reopen it.
- **Governance:** new CRUD-shaped pack operations use dispatch-by-kind and share
  create-time specialization through KindHook unless they contain genuine domain logic.
  (Scope restated by Amendment 1, Decision 1: the rule governs internal paths, not the wire surface.)
- **Residual, intentionally unbuilt:** an update-time `KindHook` seam. Do not infer
  update parity from the create implementation or bypass GTD's atomic transition guard.
  (Corrected by Amendment 1, Decision 2: the update-time seam is built; the GTD transition guard stays in its
  handler.)
- **Non-goal:** brain/knowledge substrate exposure still requires a separate schema and
  storage design; it is not implementation work authorized by this ADR.

## Amendment 1 (2026-10-23) -- Verb governance scope and implementation state

**Status**: Proposed 2026-10-23. This draft requires specification approval before dependent implementation lands. It
does not change the status of the base ADR or of earlier amendments.

**Addresses**: [#1730](https://github.com/ohdearquant/khive/issues/1730).

**Trigger.** The base record calls the one-verb-per-capability rule an internal pattern, but ADR-023 and AGENTS.md
apply it to new packs and verbs. The base record also says update-time KindHook support is unbuilt, while the runtime
trait exposes update methods used by GTD, memory, and git. ADR-084's base is Proposed and Amendment 1 is Accepted
(`docs/adr/ADR-084-verb-surface-consistency.md:3,497-502;
docs/adr/ADR-095-verb-surface-consolidation.md:105-109,236-250;
docs/adr/ADR-023-declarative-pack-format.md:626-639; AGENTS.md:829-843;
crates/khive-runtime/src/pack/traits.rs:526-612; crates/khive-runtime/src/pack/catalog.rs:146-160;
crates/khive-pack-gtd/src/hook.rs:400-420; crates/khive-pack-memory/src/hook.rs:147-155;
crates/khive-pack-git/src/hook.rs:396-430`).

### Decision 1 -- Keep wire-facing verbs outside the consolidation mandate

The one-verb-per-capability rule governs internal implementation paths, not the wire-facing verb surface. A
CRUD-shaped operation of a new pack (create, read, update, delete, or search over an entity or note kind) uses the
shared dispatch-by-kind path with its KindHook, not a parallel handler. A pack may still expose a named verb for the
operation because named verbs are the discoverability surface. The ADR cites the generated `request` tool
description and calls the named surface a feature (`docs/adr/ADR-095-verb-surface-consolidation.md:54-56,139-144`).
No existing verb is retired, renamed, or aliased.
A separate implementation path is justified only when the operation carries genuine non-CRUD domain
logic (a state machine, an atomic multi-step guard, or side effects beyond storing a record) that the shared
path cannot express.

### Decision 2 -- Record the implemented update seam and remaining GTD guard

The update-time KindHook seam is built. It exposes `normalize_note_update`, `validate_note_update`, and
`validate_entity_update` (`crates/khive-runtime/src/pack/traits.rs:526-612`). Runtime update preparation calls note
normalization and validation (`crates/khive-runtime/src/pack/catalog.rs:146-160`). Entity update preparation calls
`validate_entity_update` (`crates/khive-runtime/src/curation/entity_curation.rs:68-74,279-290`). GTD, memory, and
git provide update-hook implementations (`crates/khive-pack-gtd/src/hook.rs:400-420;
crates/khive-pack-memory/src/hook.rs:147-155; crates/khive-pack-git/src/hook.rs:396-430`). The GTD lifecycle
state machine remains in its handler: `atomic_gtd_transition` is an atomic conditional update, and `can_transition`
validates allowed transitions (`crates/khive-pack-gtd/src/handlers.rs:735-775;
crates/khive-pack-gtd/src/schema.rs:102-105`). No GTD transition guard is added to KindHook.

### Decision 3 -- State ADR-084 status precisely

The ADR-084 base record is Proposed, and Amendment 1 is Accepted
(`docs/adr/ADR-084-verb-surface-consistency.md:3,497-502`). The `schema` verb is shipped, but neither the base ADR
nor Amendment 1 ships the complete ADR-084 contract (`docs/adr/ADR-084-verb-surface-consistency.md:14-19,561-570`).
This corrects the base record's statement that ADR-084 already establishes its complete contract.

### Decision 4 -- Resolve the session and substrate questions

Issue [#1494](https://github.com/ohdearquant/khive/issues/1494) is resolved by keeping mirrored session records in
the session pack's own `sessions`, `session_messages`, and `session_mirror_cursor` tables. The mirror writes session,
message, and cursor rows. Session notes remain graph records: `session.store` creates a `session` note, and
`session.resume` resolves and fetches it through `core()` (`crates/khive-pack-session/src/vocab.rs:19-76,82-87;
crates/khive-pack-session/src/mirror/ingest.rs:1461-1483,1554-1581,1762-1802;
crates/khive-pack-session/src/handlers/store.rs:31-47; crates/khive-pack-session/src/handlers/resume.rs:20-33`).
No generic substrate access verb is adopted. ADR-201 Amendment 1 settles the event-read portion, retaining brain
verbs and adding runtime compatibility readers without registering new verbs
(`docs/adr/ADR-201-brain-module-boundaries.md:49-51,61-115,262-321;
[#1503](https://github.com/ohdearquant/khive/issues/1503)). Issue #1730 can close when this amendment merges.

### Decision 5 -- Correct the snapshot and use current evidence

The 2026-07-04 snapshot counted eight packs; the ninth pack arrived later
([#624](https://github.com/ohdearquant/khive/pull/624), [#692](https://github.com/ohdearquant/khive/pull/692)). This
count is historical evidence, not current surface authority.

### Acceptance

| ID  | Statement                                                                                                          | Evidence                                                                                                                             |
| --- | ------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------ |
| A1  | The exception clause has the same wording in ADR-095 and AGENTS.md; ADR-023 Rule 1 carries a supersession pointer. | `docs/adr/ADR-095-verb-surface-consolidation.md:395-397`; `AGENTS.md:840-842`; `docs/adr/ADR-023-declarative-pack-format.md:626-634` |
| A2  | Runtime declares update methods.                                                                                   | `crates/khive-runtime/src/pack/traits.rs:526-612`                                                                                    |
| A3  | Runtime invokes note update hooks.                                                                                 | `crates/khive-runtime/src/pack/catalog.rs:146-160`                                                                                   |
| A4  | Runtime invokes entity update hooks.                                                                               | `crates/khive-runtime/src/curation/entity_curation.rs:68-74,279-290`                                                                 |
| A5  | GTD implements note update hooks.                                                                                  | `crates/khive-pack-gtd/src/hook.rs:400-420`                                                                                          |
| A6  | Memory implements note update normalization.                                                                       | `crates/khive-pack-memory/src/hook.rs:147-155`                                                                                       |
| A7  | Git implements note update hooks.                                                                                  | `crates/khive-pack-git/src/hook.rs:396-430`                                                                                          |
| A8  | GTD's atomic transition remains in its handler.                                                                    | `crates/khive-pack-gtd/src/handlers.rs:746-775`                                                                                      |
| A9  | GTD validates allowed status transitions.                                                                          | `crates/khive-pack-gtd/src/schema.rs:102-105`                                                                                        |
| A10 | The ADR-084 base is Proposed and Amendment 1 is Accepted.                                                          | `docs/adr/ADR-084-verb-surface-consistency.md:3,497-502`                                                                             |
| A11 | The `schema` verb is shipped, but the full contract is not.                                                        | `docs/adr/ADR-084-verb-surface-consistency.md:14-19,561-570`                                                                         |
| A12 | The session pack declares its mirror tables.                                                                       | `crates/khive-pack-session/src/vocab.rs:19-76`                                                                                       |
| A13 | The mirror writes session rows.                                                                                    | `crates/khive-pack-session/src/mirror/ingest.rs:1461-1483`                                                                           |
| A14 | The mirror writes message/cursor rows.                                                                             | `crates/khive-pack-session/src/mirror/ingest.rs:1554-1581,1762-1802`                                                                 |
| A15 | `session.store` creates a graph note.                                                                              | `crates/khive-pack-session/src/handlers/store.rs:31-47`                                                                              |
| A16 | `session.resume` resolves and fetches a graph note.                                                                | `crates/khive-pack-session/src/handlers/resume.rs:20-33`                                                                             |
| A17 | The 2026-07-04 pack count is eight.                                                                                | PR #624; PR #692                                                                                                                     |
