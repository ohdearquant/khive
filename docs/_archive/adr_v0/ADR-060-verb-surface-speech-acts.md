# ADR-060: Verb Surface as Illocutionary Act Taxonomy

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-023 (Verb Consolidated MCP Surface), ADR-025 (Pack Standard)

## Context

ADR-023 consolidated the verb surface into a single MCP tool. ADR-025 established packs as the
owners of verb vocabulary. Together they define the current 15+ product verbs as a closed,
pack-owned interface. ADR-023 consolidated them
into a single MCP surface. The verbs were designed pragmatically: each verb does one thing, the
set covers all user-facing operations, and the closed discipline prevents verb sprawl.

What was never formalized: the verbs are not arbitrary API names. They are **illocutionary acts**
in the Speech Act Theory sense (Austin 1962, Searle 1969). Each verb constitutes an institutional
fact in the namespace — `remember` doesn't just store bytes, it _commits_ the caller to a memory;
`assign` doesn't just create a row, it _directs_ an actor to do work; `complete` doesn't just
update a status, it _declares_ a task done.

This structural correspondence was identified during KG analysis: the same closed-vocabulary
principle governs both deontic compliance vocabularies (27 prefixes in compliance frameworks) and
khive's 15 product verbs. Both are insert-only closed taxonomies where the vocabulary IS the
regulation — adding a verb changes what the system can do, not just how it's called.

This observation has significant prior art. The Knowledge Query and Manipulation Language (KQML,
1990s) and FIPA Agent Communication Language (FIPA-ACL, 1997) explicitly used Searle's categories
to classify agent communication performatives: `ask-all` (assertive), `achieve` (directive),
`tell`/`insert` (commissive). khive's application to a modern MCP verb surface is fresh, but the
general principle — classifying API verbs by illocutionary force — is well-established in
multi-agent systems literature.

Formalizing this connection gives the verb surface a principled extension criterion: new verbs
are admissible only if they introduce a new illocutionary force not covered by the existing set.
Without this, the "closed" discipline in ADR-015 relies on taste rather than theory.

## Decision

### 1. Classify the 15 product verbs by illocutionary force

Following Searle's five categories (1976):

| Category        | Illocutionary force                            | Verbs                                       | What the verb DOES                              |
| --------------- | ---------------------------------------------- | ------------------------------------------- | ----------------------------------------------- |
| **Assertive**   | Speaker represents a state of affairs          | `recall`, `search`, `list`, `inbox`, `next` | Retrieves and presents facts from the substrate |
| **Directive**   | Speaker attempts to get hearer to do something | `assign`                                    | Directs an actor to perform work                |
| **Commissive**  | Speaker commits to a future course of action   | `remember`, `send`, `link`, `create`        | Commits the caller to a persistent change       |
| **Declaration** | Speaker brings about a state of affairs        | `complete`, `delete`, `update`              | Changes institutional status by fiat            |
| **Expressive**  | Speaker expresses psychological state          | _(none)_                                    | No verb currently — and this is correct         |

The `suggest` and `compose` verbs are internal-only (lore service) and not part of the product
surface. They would classify as assertives (they retrieve and present domain knowledge).

### 2. Extension criterion

A new verb is admissible if and only if:

1. **It introduces a force not redundant with an existing verb in the same category.** Adding a
   second directive (`order`) alongside `assign` requires justification for why `assign` is
   insufficient. Adding a first expressive would require justification for why expressives belong
   in the product surface.

2. **It constitutes an institutional fact.** A verb that merely retrieves data without committing
   to anything is assertive; verify it isn't a synonym for `recall`/`search`/`list`. A verb that
   changes state is either commissive (caller-initiated commitment) or declarative (status change
   by authority); verify it isn't a synonym for an existing verb in that category.

3. **The 15-verb cap is a guideline, not a law.** The ceiling exists because agent comprehension
   degrades with surface size. But if a genuinely new illocutionary force appears (e.g., a
   _permissive_ — granting rights, which is neither directive nor declaration), the cap yields
   to the taxonomy. The taxonomy is the invariant, not the count.

### 3. Batch `request` verb classification

The `request` verb (ADR-020/ADR-027) is a meta-verb: it composes other verbs. Its illocutionary
force is _inherited_ from the verbs it contains. `request("[assign(...), complete(...)]")`
performs a directive followed by a declaration. The request verb itself is a _speech-act
combinator_, analogous to a conjunction of illocutionary acts.

### 4. Documentation convention

Each verb's doc comment in the MCP surface definition should include its illocutionary
classification:

```rust
/// remember — Commissive: commits a memory to the namespace.
/// assign  — Directive: directs an actor to perform work.
/// complete — Declaration: declares a task done.
```

This is a documentation convention, not a runtime check.

## Alternatives Considered

### A. Don't formalize; keep the pragmatic "15 verbs, closed" rule

Pros: simpler, no theory needed. Cons: "closed" without an extension criterion is just
"we haven't needed to add one yet." The first verb-addition pressure will re-open the debate
from scratch. A principled taxonomy gives the debate a frame.

Rejected. The taxonomy costs one ADR and pays back on every future verb discussion.

### B. Use a different speech-act taxonomy (Bach & Harnish 1979, or custom)

Pros: could be more fine-grained. Cons: Searle's five categories are the most widely cited and
map cleanly to the existing 15 verbs. A custom taxonomy would require justification that Searle's
doesn't fit, and none has been found.

Rejected. Searle's taxonomy fits; use it.

### C. Add "expressive" verbs (e.g., `react`, `like`, `flag`)

Considered as a consequence of the taxonomy: the expressive category is empty. Should we fill it?
No. Expressives communicate psychological state without changing the substrate. In a research
runtime, the relevant signal is always an assertive ("I observe X"), a commissive ("I note X"),
or a declaration ("I dispute X"). Expressives would be social-media primitives, not research
primitives. If the cloud product later adds social reactions (stars, emoji), those belong in the
cloud layer, not the substrate verb surface.

Deferred to cloud product layer.

## Consequences

### Positive

- **Principled extension criterion**: verb-addition proposals can be evaluated against a 60-year-old
  taxonomy rather than ad-hoc debate
- **Agent comprehension**: agents can reason about verb semantics categorically ("all commissives
  commit state; all assertives are read-only") rather than memorizing 15 individual definitions
- **Cross-system coherence**: the same illocutionary taxonomy governs canonsys deontic prefixes and
  khive verbs, strengthening the architectural isomorphism (7 axes, 14 `instance_of` edges to
  Insert-Only Closed-Taxonomy Architecture)

### Negative

- **Theoretical overhead**: developers must know Searle's categories to evaluate verb proposals.
  Mitigated by the classification table above — the theory is captured in this ADR, not assumed
  as background knowledge.
- **Edge cases**: some verbs straddle categories. `send` is arguably both commissive (commits a
  message) and directive (requests the recipient's attention). The classification table represents
  primary force; secondary forces are noted but not formalized.

## Open Questions

1. **`orient` verb**: currently absent from the product surface (used in session-start protocol).
   If promoted, it would be assertive (presents namespace dashboard). Should it be formalized?
2. **`thread` verb**: assertive (retrieves a conversation thread). Currently in the surface but
   not in the 15-verb table of ADR-015. Classify and document.
3. **Cloud-only verbs**: the cloud ADR proposals (ADR-040 provenance, ADR-041 dispute, ADR-042
   dispute-PR) will introduce cloud-layer verbs. Should the illocutionary taxonomy apply to the
   cloud surface as well, or only to the substrate?

## References

- ADR-015: Verb-Based Interface Standard
- ADR-023: Verb Consolidated MCP Surface
- ADR-027: Single-Tool MCP Surface
- Austin, J.L., "How to Do Things with Words" (1962)
- Searle, J.R., "Speech Acts: An Essay in the Philosophy of Language" (1969)
- Searle, J.R., "A Classification of Illocutionary Acts" (1976)
- KQML: Finin et al., "KQML as an Agent Communication Language" (1994)
- FIPA-ACL: Foundation for Intelligent Physical Agents, "FIPA ACL Message Structure
  Specification" (1997) — direct prior art for illocutionary-force-classified agent APIs
