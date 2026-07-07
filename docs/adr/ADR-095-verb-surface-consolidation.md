# ADR-095: Verb-Surface Consolidation and Field-Validation Governance

**Status**: Proposed
**Date**: 2026-07-04
**Authors**: khive maintainers
**Depends on**: ADR-016 (request DSL, single-tool contract), ADR-017 (pack standard),
ADR-023 (pack verb surface, visibility, and composition), ADR-083 (session pack T1 verbs),
ADR-084 (verb-surface
consistency contract)
**Scope**: the 74-verb MCP surface across the 9 default packs; the KindHook create-time
extension seam; per-verb field validation (status enums, memory_type enums, datetime
checks). Out of scope: brain and knowledge pack-private storage, closed taxonomies.

## Context

The design request asked for two things: (a) consolidate specialized-pack
CRUD-shaped verbs (the example given was `session.resume`) into the kg pack's
dispatch-by-kind pattern (`search(kind="session", ...)`), and (b) centralize EDGE_RULES
validation so endpoint rules are "bundled and compiled together, instead of per verb
enforcement."

Two facts reframe the directive before any design begins.

First, part (b) is already shipped. PR #621 (merged 2026-07-04) left exactly one composed
edge-rule set. Endpoint acceptance flows through a single validator,
`validate_edge_relation_endpoints` (crates/khive-runtime/src/operations.rs:1130-1317), which
consults the runtime-composed rules (base entity-endpoint rules plus each loaded pack's
declared additions per ADR-017). The recon in VERB_SURFACE_INVENTORY.md (cited to c8c16f49
plus #621) found no remaining duplicate rule site. So the EDGE_RULES half of the directive
is done. Restating it would produce an ADR that ratifies work already merged.

Second, the still-per-verb validation layer that the centralization goal actually
lands on is not edge endpoints. It is KindHook-shaped field validation: the status-enum
checks in gtd, the memory_type enum in memory, the RFC-3339 datetime checks in schedule,
each living inside its own pack handler with no composed-registry equivalent. That is the
real target of part (b), and F3 below decides it.

The verb/single-tool contract is ADR-016 (Request DSL), not ADR-015 (schema migrations in
this repo).

### The surface as it stands

The 74 user-facing verbs across 9 default packs (kg 17, gtd 5, memory 5, brain 14, comm 6,
schedule 4, knowledge 19, session 4, git 0) are asserted as a tripwire: `tests/smoke_test.py:209`
carries `assert verbs_result["total"] == 74`, with a comment stating the assertion exists to
catch silent drift. Any change to the count moves this assertion, the AGENTS.md catalog, and
the `request` tool description in lockstep.

The `request` tool description enumerates the verb catalog. That enumeration is the primary
discoverability surface an agent caller sees: the list of named verbs is how an agent knows
what the server can do. This matters for the refute analysis below.

### What already exists that a fold would use

The KindHook trait (crates/khive-runtime/src/pack.rs:247-267) is the shipped extension seam
for per-kind create behavior. It has two methods: `prepare_create` (pack.rs:253) runs before
the storage write and can normalize or reject fields; `after_create` (pack.rs:267) runs after
the write for side effects. The kg create handler invokes both: it looks up the hook
(create.rs:228, create.rs:244), calls `prepare_create` (create.rs:283), and calls
`after_create` (create.rs:363). The after_create call is best-effort: a failure is logged and
swallowed because "storage write already committed" (create.rs:363-368).

gtd already ships a production KindHook. `TaskHook` (crates/khive-pack-gtd/src/hook.rs:25)
implements the trait: `prepare_create` (hook.rs:26) normalizes task fields, and
`after_create` (hook.rs:207) creates `depends_on` edges best-effort.

This is decisive for the mechanism forks. The inventory claimed (gtd.assign row, section 5
blocker 3) that folding crud-plus verbs "needs a parameter/hook extension, not just a rename"
because create's kind resolution has "no hook seam." That claim is wrong against source. The
hook seam exists, ships, and is used. Folding crud-plus does not require building new
infrastructure; it requires routing through infrastructure that is already there.

There is a related finding the fold must respect. `gtd.assign` (handlers.rs:497) does not go
through the kg create handler plus TaskHook path. It calls `create_note` directly
(handlers.rs:633) after inline validation, salience derivation, and a
resolve-depends_on-before-write orphan guard. Its status normalization helpers are shared
with the hook (comment "Shared with hook.rs" at handlers.rs:372). So `gtd.assign`-the-verb
and `create(kind="task")`+TaskHook are two write paths that produce the same kind of record
from partly shared helpers. That is a genuine duplicate-path, and it is the one place in the
current surface where the "compiled together instead of per-verb" direction is correct at
the code level, independent of any wire change.

The gtd state machine must survive any change. `atomic_gtd_transition` (handlers.rs:444) is a
single atomic conditional UPDATE keyed on the properties status via json_extract, where
`rows_affected == 0` means the caller lost a concurrent transition race. `can_transition`
(handlers.rs:1012) is the validated state-machine guard. Neither has an equivalent in the kg
generic verbs. Any fold that touched task lifecycle would have to reproduce this exactly, and
none should.

## Decision

The design answer is: half of this is done, a quarter should not be done, and here
is the quarter worth doing.

- The EDGE_RULES half (part b as literally stated) is shipped (#621). This ADR records that
  and does not reopen it.
- The verb-retirement quarter (fold named pack verbs into dispatch-by-kind and remove them
  from the surface) should not be done. It breaks a decision accepted 48 hours
  earlier (ADR-083), it degrades agent discoverability, and it buys a deduplication benefit
  that the existing KindHook seam already delivers without touching the wire.
- The quarter worth doing is internal, not wire-facing: (1) make dispatch-by-kind plus
  KindHook the mandatory internal pattern for future packs and for collapsing existing
  duplicate write paths; (2) a governance rule that per-kind create-time field validation
  lives in KindHook, not in bespoke parallel handlers; (3) name, but do not build, an
  update-time validation seam as future work.

Per-fork decisions follow.

### F1 Consolidation scope: decision (d) plus a narrow internal (c), reject (a) and (b)

Decision: (d) keep the surface and codify dispatch-by-kind as the mandatory pattern for
future packs, augmented by a narrow, non-breaking internal fold that routes existing
duplicate write paths through the shared create-plus-KindHook seam. Reject (a) session-only
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
   of that decision, proposed 48 hours after it was accepted, and the packet names the session
   pack as load-bearing for the commercial session-continuity pillar. Reopening an accepted,
   commercially-tied decision within two days needs a stronger reason than surface tidiness,
   and there is none.

2. Named verbs are self-documenting for agent callers; dispatch-by-kind pushes semantics into
   a `kind` parameter the agent must know to pass. The `request` tool description enumerates
   the named catalog, so each retirement removes a line from the discoverability surface and
   replaces it with knowledge the agent has to hold out of band. For a product whose callers
   are LLM agents, the named surface is a feature, not clutter.

3. The one concrete benefit retirement would buy, less duplicated CRUD code, is available
   without retiring anything. The duplicate is `gtd.assign`'s direct `create_note` path versus
   `create(kind="task")`+TaskHook (see Context). Routing the verb's internal implementation
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
public entry point; its handler routes through the same create-plus-KindHook path that
`create(kind=...)` uses, so there is one code path and one validation site per kind. Reject
(a) hard removal and (b) alias-and-deprecate as a durable state.

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

Argument for (c) via KindHook (the decision): the seam exists and is proven in production
(TaskHook, pack.rs:247, create.rs:283/363). Unifying `gtd.assign`'s path onto it is a code
refactor with zero wire change, zero count change, and it centralizes the task field logic
that is currently forked between handlers.rs and hook.rs into one site. It is the
FindExisting answer (PI_AEP): the mechanism is not built, it is adopted.

Hard constraint honored: the resolve-depends_on-before-write orphan guard is already hosted
in `TaskHook::prepare_create` (hook.rs:103), which pre-validates every depends_on target
through the same `resolve_primary` path `gtd.assign` uses, before the storage write.
Unification therefore removes `handle_assign`'s duplicate copy of the guard and leaves
`prepare_create` as the single create-time validation site; the guard is not moved, weakened,
or carved out as a second site. The `atomic_gtd_transition` state machine (handlers.rs:444)
is lifecycle logic outside create; the transition path is not folded and its atomic guard is
untouched.

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
that are three-line enum matches today, and it overlaps ADR-084's `schema` introspection verb
without coordinating with it.

Argument for (b) plus governance (the decision): the enum checks are cheapest expressed as
what they are, small guards next to the handler. The real defect is divergence risk, the same
check implemented twice (handlers.rs and hook.rs sharing helpers by comment, not by
construction). A governance rule that create-time field validation lives in KindHook converges
those without new machinery. This is the minimal change that fixes the actual problem.

Future work, named not built: there is no update-time equivalent of KindHook. Update field
validation and the gtd transition guard live in their own handlers. If a second pack ever needs
validated update-time field rules, a `KindHook::prepare_update` (or `validate_update`) seam is
the natural extension. This ADR names it as future work and explicitly does not build it now,
because one consumer (gtd transition) is not enough signal to design a shared seam, and its
atomic guard must not be perturbed speculatively.

Coordination: ADR-084 (proposed) already establishes the verb-surface-consistency contract
(the ID-resolution ladder, no-silent-enum-coercion, help-schema fidelity, param-naming
discipline, substrate-symmetric field names, declared-vocabulary completeness) and the
`schema` introspection verb. F3's governance rule is consistent with ADR-084 Rule 2
(no silent enum coercion): validation that rejects rather than coerces belongs in one declared
place per kind. This ADR references ADR-084 for the surface contract and does not duplicate it.

### F4 Migration and wire-compat: clean no-op on the wire

Decision: because nothing is retired, there is no wire-facing migration. The verb count stays
74 (plus whatever ADR-084's `schema` verb adds when it lands, which is ADR-084's tripwire to
move). No deprecation window is needed because no breaking change is made.

Argument for a deprecation window (the case for treating this as a migration): if we were
retiring verbs, pre-1.0 status and a known consumer set (operators plus marketplace plugin
users) would still argue for a clean break over a deprecation window, since the consumer set is
small and reachable and a permanent alias surface is the F2-rejected outcome. But this decision
retires nothing, so the question is moot for the wire.

The only change under this ADR is the internal-path unification (F2) and the governance rule
(F3). Both are covered by existing integration tests. The migration sketch below adds one
equivalence test. The verb-count tripwire (smoke_test.py:209), the AGENTS.md catalog, and the
`request` tool description do not move under this ADR.

Alternatives considered for F4: deprecation window (moot, nothing deprecated) and clean break
(moot, nothing broken). The decision is the third option the fork did not enumerate: no wire
change at all.

## Consequences

Verb-count deltas:

- Net wire delta under this ADR: 0. Count stays 74. No verb is added, aliased, or removed.
- The refuted alternatives would have been: F1(a) session-only retirement -4 (to 70);
  F1(b) all-crud-dup -N (N is the crud-dup count in the inventory, larger blast radius);
  F2(b) alias +0 short term but a permanent doubled internal surface. This ADR takes none of
  these.

Doc and tripwire touchpoints (none move under this ADR, listed so the reviewer can confirm the
no-op):

- tests/smoke_test.py:209 (`assert verbs_result["total"] == 74`): unchanged.
- AGENTS.md verb catalog: unchanged.
- The `request` tool description enumeration: unchanged.
- ADR-023 (pack verb surface, visibility, and composition) and AGENTS.md amendment lane:
  engaged only for the
  governance rule wording (F1/F3 codification of dispatch-by-kind plus KindHook as the
  mandatory pattern for future packs), which is a documentation amendment, not a surface change.

Positive consequences:

- The `gtd.assign` duplicate write path is unified onto the shared create-plus-KindHook seam,
  removing the divergence risk between handlers.rs and hook.rs task field logic.
- Future packs have a written, mandatory pattern (dispatch-by-kind plus KindHook), reducing the
  chance new packs add crud-dup verbs that later invite exactly this consolidation question.
- ADR-083's session decision, the commercial continuity pillar, and the named discoverability
  surface are all preserved.
- No breaking change ships to operators or plugin users.

Negative consequences and costs:

- The surface does not shrink. Anyone who wanted a smaller catalog does not get it. This is the
  deliberate trade: discoverability and stability over count reduction.
- The internal-path unification is real refactor work on live gtd code paths, gated by the
  atomic-transition and orphan-guard constraints, so it is careful work despite being a no-op
  on the wire.
- The update-time validation gap is named but left open, so a future pack needing it will have
  to design the seam then.

## Non-goals

- brain (14 verbs) and knowledge (19 verbs) are out of scope. They write to pack-private SQL
  tables and register no entity or note kinds, so dispatch-by-kind cannot express them without
  a schema migration that introduces new storage substrates. The packet forbids that and this
  ADR names it as future work only, not a decision here.
- Closed taxonomies stay closed. This ADR adds no entity kind (ADR-001), no edge relation
  (ADR-002), and no note kind (ADR-013), and it introduces no new storage substrate.
- The EDGE_RULES centralization is not reopened. It shipped in #621. This ADR records it as
  done and builds on it rather than restating it.
- ADR-084's surface-consistency contract and `schema` verb are not duplicated. This ADR
  references them.

## Migration and test sketch (informative)

The wire is unchanged, so the "migration" is an internal refactor plus one new test.

1. Internal-path unification (F2): change `handle_assign` (handlers.rs:497) so the task-note
   creation routes through the same create-plus-KindHook path that `create(kind="task")` uses,
   so `TaskHook::prepare_create` (hook.rs:26) is the single field-normalization and pre-write
   depends_on-validation site (it already hosts the orphan guard, hook.rs:103;
   `handle_assign`'s duplicate copy is removed) and `after_create` (hook.rs:207) is the single
   depends_on-edge site. The `context_entity_id` parameter must translate to the shared path
   with identical semantics: full-UUID validation resolving to a KG entity (handlers.rs:264),
   persistence into `properties.context_entity_id` (handlers.rs:620), and the `annotates`
   edge currently passed to `create_note` (handlers.rs:630). The transition path
   (handlers.rs:444) is not touched.
2. Governance amendment (F1/F3): amend ADR-023 (and AGENTS.md) to state that (i) new packs
   express CRUD-shaped operations through kg dispatch-by-kind rather than bespoke verbs unless
   they carry genuine non-CRUD domain logic, and (ii) per-kind create-time field validation
   lives in `KindHook::prepare_create`, not in a parallel handler.
3. New test: an equivalence test asserting that `gtd.assign(...)` and the corresponding
   `create(kind="task", ...)` produce records with identical normalized fields, identical
   depends_on edges, and identical `context_entity_id` handling (rejection of non-UUID input,
   `properties.context_entity_id`, and the `annotates` edge), so the two paths cannot
   silently diverge again.
4. Unchanged guards: the smoke tripwire (smoke_test.py:209 == 74) must still pass untouched,
   which is the positive proof that this ADR is a wire no-op.

## GitHub issues to file

- "Unify gtd.assign onto the create+KindHook seam (remove the duplicate task write path)":
  route handle_assign's note creation through TaskHook prepare_create/after_create; preserve the
  pre-write orphan guard; do not touch the transition state machine.
- "Add gtd.assign vs create(kind=task) equivalence test": assert identical normalized fields
  and depends_on edges from both paths, guarding against future divergence.
- "Amend ADR-023 + AGENTS.md: dispatch-by-kind + KindHook is the mandatory pattern for future
  packs": document the rule that new CRUD-shaped operations use kg dispatch-by-kind and that
  per-kind create-time field validation lives in KindHook.
- "Record EDGE_RULES centralization as complete (ADR cross-reference to #621)": a doc-only
  note pointing ADR readers at the single composed validator so the centralization intent is
  not re-litigated.
- "Future work: KindHook update-time validation seam (prepare_update)": placeholder issue,
  do-not-build-yet, to capture the update-time field-validation gap for when a second consumer
  appears.
- "Future work: brain/knowledge substrate exposure (requires schema migration)": placeholder
  capturing why these 33 verbs are out of the dispatch-by-kind scope today.
