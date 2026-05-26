# ADR-025: Verb Surface as Speech-Act Taxonomy

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: Ocean, lambda:khive

## Context

ADR-016 establishes the `request` DSL and ADR-017 establishes packs as verb-vocabulary
owners. Together they define the runtime verb surface as a closed, pack-owned interface.
ADR-027 (dynamic pack loading) introduces self-registration: any pack can claim verbs.
Without a principled criterion for what a verb _should_ be, "closed" devolves into "we
haven't added one lately."

The verbs are not arbitrary API names. They are **illocutionary acts** in the Speech Act
Theory sense (Austin 1962, Searle 1969). Each verb constitutes an institutional fact in
the namespace — `remember` does not just store bytes; it _commits_ the caller to a memory.
`assign` does not just create a row; it _directs_ an actor to do work. `complete` does not
just update a status; it _declares_ a task done.

This observation has significant prior art. The Knowledge Query and Manipulation Language
(KQML, 1990s) and FIPA Agent Communication Language (FIPA-ACL, 1997) explicitly used
Searle's categories to classify agent communication performatives: `ask-all` (assertive),
`achieve` (directive), `tell`/`insert` (commissive). khive's application to a modern MCP
verb surface is fresh, but the general principle — classifying API verbs by illocutionary
force — is well-established in multi-agent systems literature.

Formalizing this connection gives the verb surface a principled extension criterion: new
verbs are admissible only if they introduce a new illocutionary force not redundantly
covered by the existing set.

## Decision

### Classify the runtime verbs by illocutionary force

Following Searle's five categories (1976):

| Category        | Illocutionary force                            | Verbs                                                                                         | What the verb DOES                              |
| --------------- | ---------------------------------------------- | --------------------------------------------------------------------------------------------- | ----------------------------------------------- |
| **Assertive**   | Speaker represents a state of affairs          | `get`, `list`, `search`, `recall`, `neighbors`, `traverse`, `query`, `next`, `tasks`, `inbox` | Retrieves and presents facts from the substrate |
| **Directive**   | Speaker attempts to get hearer to do something | `assign`, `transition`                                                                        | Directs an actor or state machine to act        |
| **Commissive**  | Speaker commits to a future course of action   | `create`, `remember`, `link`, `send`, `propose`, `withdraw` (ADR-046)                         | Commits the caller to a persistent change       |
| **Declaration** | Speaker brings about a state of affairs        | `update`, `delete`, `merge`, `complete`, `review` (ADR-046)                                   | Changes institutional status by fiat            |
| **Expressive**  | Speaker expresses psychological state          | _(none)_                                                                                      | No verb currently — and this is correct         |

The `suggest` and `compose` verbs (internal lore service, not part of the standard
product surface) would classify as assertives. Pack-internal handlers prefixed with a
dotted notation (`recall.score`, `brain.state` — ADR-033, ADR-032) inherit the
illocutionary force of their parent verb.

### Extension criterion

A new verb is admissible if and only if:

1. **It introduces a force not redundant with an existing verb in the same category.**
   Adding a second directive (`order`) alongside `assign` requires justification for why
   `assign` is insufficient. Adding a first expressive would require justification for why
   expressives belong in the substrate surface at all.

2. **It constitutes an institutional fact.** A verb that merely retrieves data without
   committing to anything is assertive; verify it is not a synonym for
   `recall`/`search`/`list`. A verb that changes state is either commissive (caller-
   initiated commitment) or declarative (status change by authority); verify it is not a
   synonym for an existing verb in that category.

3. **The verb count is a guideline, not a law.** A ceiling exists because agent
   comprehension degrades with surface size. But if a genuinely new illocutionary force
   appears (e.g., a _permissive_ — granting rights, which is neither directive nor
   declaration), the count yields to the taxonomy. The taxonomy is the invariant, not
   the count.

### Batch `request` is a speech-act combinator

The `request` verb (ADR-016) is a meta-verb: it composes other verbs. Its illocutionary
force is _inherited_ from the verbs it contains. `request("[assign(...), complete(...)]")`
performs a directive followed by a declaration. The request verb itself is a speech-act
combinator, analogous to a conjunction of illocutionary acts. Speech-act classification
does not apply to `request` as a primitive — it applies to the verbs it dispatches.

### Documentation convention

Every handler's doc comment in its pack's `HANDLERS` registration MUST include its
illocutionary classification. The wire form follows ADR-023 §4 (kg pack bare;
all others pack-prefixed):

```rust
const HANDLERS: &[HandlerDef] = &[
    HandlerDef {
        name:        "remember",                       // wire: memory.remember
        category:    VerbCategory::Commissive,
        visibility:  Visibility::Verb,
        description: "Commits a memory to the namespace.",
        // ...
    },
    HandlerDef {
        name:        "assign",                         // wire: gtd.assign
        category:    VerbCategory::Directive,
        visibility:  Visibility::Verb,
        description: "Directs an actor to perform work.",
        // ...
    },
    // ...
];

pub enum VerbCategory {
    Assertive,
    Directive,
    Commissive,
    Declaration,
    // Expressive — reserved; no verb currently uses it.
}
```

The category is a runtime tag (`HandlerDef::category`) consumed by introspection
(`kkernel call <pack> <handler> --help` per ADR-003) and not enforced at compile time.
Mis-categorization is
caught in code review, not by the type system. The point is naming the classification so
verb-addition discussions can reference it.

### What the taxonomy is not used for

- **Not for permission checking.** ADR-018 (gate enforcement) defines which verbs are
  agent-safe; the gate uses verb names directly, not speech-act categories.
- **Not for transport routing.** All verbs go through the same `request` DSL (ADR-016).
- **Not for return-shape selection.** Return shapes are per-verb, not per-category.

The taxonomy is **only** for the verb-addition decision and for documentation/introspection.

## Rationale

### Why formalize at all

"N verbs, closed" without an extension criterion is just "we have not needed to add one
yet." (The product surface has grown: the kg pack now carries 15 verbs including `propose`,
`review`, `withdraw` from ADR-046 and `verbs` discovery from Wave 4; the full classified surface is in the table above.) The first verb-addition pressure (e.g., for a future audit/compliance pack) will
re-open the debate from scratch. A principled taxonomy gives the debate a frame: "is this
a new illocutionary force, or a synonym?" The answer is usually decisive.

The taxonomy costs one ADR and pays back on every future verb discussion. It also
strengthens the cross-system coherence with deontic vocabularies (compliance frameworks
use the same closed-vocabulary discipline, and the same illocutionary structure).

### Why Searle's five and not another taxonomy

Bach & Harnish (1979) propose a finer-grained taxonomy; FIPA-ACL has 22 performatives.
Searle's five categories are the most widely cited, map cleanly onto khive's existing
verbs, and are at the right level of abstraction for an API surface. A finer-grained
taxonomy would create false distinctions ("is `link` an assertive or a commissive?" —
unprincipled granularity).

### Why expressive stays empty

Expressives communicate psychological state without changing the substrate. In a research
runtime, the relevant signal is always an assertive ("I observe X"), a commissive ("I
note X"), or a declaration ("I dispute X"). Expressives would be social-media primitives
(`react`, `like`, `flag`), not research primitives. If a future deployment-layer pack
adds social reactions, they belong there, not in the v1 product surface.

The empty Expressive slot is deliberate — it documents what khive _is not_, as much as
what it is.

### Why batch request inherits, not classifies

`request` is the only verb that does not perform a substrate operation by itself; it
dispatches. Classifying it would either (a) pick one category and be wrong half the time
or (b) introduce a sixth category for "combinators" that has no other members. Inheritance
is the natural model: `request([X, Y])` has the speech-act force of X followed by Y.

## Alternatives Considered

### A. Skip the taxonomy; keep the pragmatic "verbs are verbs" rule

Pros: simpler, no theoretical commitment. Cons: every verb-addition proposal re-opens the
debate. A principled taxonomy converges the debate quickly. The cost of one ADR is
negligible compared to repeated re-litigation.

Rejected.

### B. Use FIPA-ACL's 22 performatives

Pros: more granular; established in multi-agent systems. Cons: 22 categories is the wrong
level — most categories would map to zero khive verbs. Searle's five hits the right
abstraction level for an API surface.

Rejected.

### C. Add expressive verbs (`react`, `like`, `flag`)

Considered as a consequence of the taxonomy: the expressive slot is empty. Should we fill
it? No. Expressives are social primitives (`react`, `like`, `flag`). If a future product
layer adds them, they belong in a deployment-layer pack, not in the v1 product surface.

Deferred to deployment-layer packs (out of v1 product surface).

### D. Compile-time category enforcement

Make `VerbCategory` an enum that gates which return shapes / parameter shapes are legal
per category. Pros: catches mis-categorization. Cons: speech-act categories do not map
cleanly onto return-shape constraints. Mixing them creates spurious type errors and
discourages correct categorization.

Rejected. Documentation convention is enough.

## Consequences

### Positive

- **Principled extension criterion**: verb-addition proposals can be evaluated against a
  60-year-old taxonomy rather than ad-hoc debate.
- **Agent comprehension**: agents can reason about verb semantics categorically ("all
  commissives commit state; all assertives are read-only") rather than memorizing
  individual definitions.
- **Cross-system coherence**: the same illocutionary taxonomy governs khive verbs and the
  KQML/FIPA-ACL agent communication tradition.
- **Empty Expressive documents the system's scope**: research substrate, not social
  network.

### Negative

- **Theoretical overhead**: contributors must know Searle's categories to evaluate verb
  proposals. Mitigated by the classification table in this ADR — the theory is captured
  here, not assumed as background.
- **Edge cases**: some verbs straddle categories. `send` is arguably both commissive
  (commits a message) and directive (requests the recipient's attention). The
  classification table represents primary force; secondary forces are noted but not
  formalized.

### Neutral

- **No runtime behavior change.** The taxonomy is a doc / introspection tag.
- **No wire format change.** MCP clients see verbs by name; categories are not on the wire.
- **Existing verbs unchanged.** This ADR ratifies the current classification, does not
  reshape it.

## Open Questions

1. **`orient` verb.** Currently absent from the standard product surface (used in
   session-start protocols). If promoted to a top-level verb, it would be assertive
   (presents namespace dashboard). Decision deferred to when `orient` is formalized.
2. **`thread` verb.** Assertive (retrieves a conversation thread). Currently in the
   surface but not in the standard table. Classify and document in the relevant pack.
3. **Deployment-layer verbs.** Future deployment-layer ADRs (provenance, dispute,
   dispute-PR) may introduce verbs outside the v1 product surface. The illocutionary
   taxonomy should apply to any such surface; those ADRs will categorize their verbs
   at proposal time.

## References

- [ADR-016](ADR-016-request-dsl.md) — `request` DSL and verb-dispatch
- [ADR-017](ADR-017-pack-standard.md) — packs own verb vocabulary; `HandlerDef` carries
  classification (`VerbDef` was the predecessor type — see ADR-023)
- [ADR-027](ADR-027-dynamic-pack-loading.md) — self-registering packs; this ADR governs
  what verbs they may introduce
- Austin, J.L., "How to Do Things with Words" (1962)
- Searle, J.R., "Speech Acts: An Essay in the Philosophy of Language" (1969)
- Searle, J.R., "A Classification of Illocutionary Acts" (1976)
- Finin et al., "KQML as an Agent Communication Language" (1994)
- FIPA, "FIPA ACL Message Structure Specification" (1997) — direct prior art for
  illocutionary-force-classified agent APIs
