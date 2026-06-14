# ADR-055: Epistemic Edge Relations — `supports` and `refutes`

**Status**: accepted\
**Date**: 2026-06-14\
**Authors**: Ocean, lambda:khive\
**Amends**: [ADR-002](ADR-002-edge-ontology.md) — expands the closed edge set from 15 → 17
relations and adds a 9th category (Epistemic / Evidential).

## Context

ADR-002 fixes a closed set of 15 edge relations in 8 categories. The set deliberately answers a
bounded list of query classes — structure, intellectual lineage, material provenance, temporal
order, dependency, implementation, peer relationships, and cross-substrate annotation.

There is one query class none of the 15 cover: **the evidence for and against a claim, with
polarity and strength.** Concretely:

> "What is the evidence supporting claim X? What refutes it? How strong is each piece of
> evidence?"

This is the defining query of any research / experiment-tracking workload: a hypothesis or
claim accumulates findings, papers, datasets, and runs that either corroborate or contradict it.
Confidence in the claim is a function of that weighted, signed evidence.

The existing relations approximate this badly and lose information:

| Candidate       | Why it fails for evidence ↔ claim                                                                                                                                                    |
| --------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `introduced_by` | Records **origin** ("first described in"), not ongoing evidential weight. Polarity-blind.                                                                                            |
| `derived_from`  | Records **material/generative provenance** (this artifact was made from that input). Not "is evidence about".                                                                        |
| `enables`       | Records a **causal/prerequisite** relation, not an epistemic one.                                                                                                                    |
| `annotates`     | A note **comments on** a target. It is polarity-blind: it cannot distinguish "this finding supports the claim" from "this finding refutes it", and it does not connect two entities. |

`annotates` is the closest, and it is what the research-pack design
(`.khive/workspaces/.../research-pack-design/DESIGN.md`) currently leans on for findings. But
`annotates` answers "is there commentary here?", not "is this evidence **for** or **against**,
and how strongly?". The polarity and the strength are exactly the signal a confidence model
needs, and `annotates` discards both.

Ocean has decided to add the minimal pair of relations that carry this signal. This ADR
formalizes that decision and specifies the contract.

## Decision

Add two relations forming a new **Category 9: Epistemic / Evidential**, expanding the closed
set from 15 → 17:

| Relation   | Direction        | When                                                                     |
| ---------- | ---------------- | ------------------------------------------------------------------------ |
| `supports` | evidence → claim | This evidence is **for** the claim (corroborates, confirms, replicates). |
| `refutes`  | evidence → claim | This evidence is **against** the claim (contradicts, falsifies).         |

The relation choice carries the **polarity** (for vs. against). The edge **weight** carries the
**strength** of the evidential link, on ADR-002's existing scale.

### Direction and symmetry

`supports` and `refutes` are **directional**: source is the evidence, target is the claim. They
are **NOT symmetric** (unlike `competes_with` / `composed_with`). The asymmetry is semantic — a
paper is evidence about a claim; a claim is not symmetrically "evidence about" the paper. No
write-time canonicalization applies. Query the inverse ("what evidence bears on this claim") with
`direction=in`, exactly as for every other directional relation.

### Weight semantics

Edge weight = **strength of the evidential link**, reusing ADR-002's existing scale verbatim:

```text
1.0      definitional / direct replication
0.7-0.9  strong evidence
0.4-0.6  plausible / suggestive
<0.4     weak / speculative
```

Polarity is **not** overloaded onto the weight. There is no negative weight; `refutes` with
weight 0.9 means "strong evidence against", not "weight -0.9". Sign lives in the relation choice,
magnitude lives in the weight. This keeps weight monotone (higher = more confident in the link)
across all 17 relations and avoids a special-case sign convention that downstream scorers would
have to know about.

### Substrate rule — Note→Note as the primary rail (mirrors `supersedes`)

`supports` and `refutes` are **same-substrate** relations: `Note → Note` **or** `Entity →
Entity`, never crossing substrates. They join `supersedes` as same-substrate relations.

> **Invariant preserved**: `annotates` remains the _only_ relation that crosses substrate kinds.
> `supports`/`refutes` do not cross.

#### Primary rail: Note→Note

**The primary documented path is `Note → Note`.** A finding note carries the evidential
result; the claim it bears on is also a note (for example, a question, decision, or insight
note holding the hypothesis text). The typed evidential link upgrades the polarity-blind
`annotates` that the research pack uses today — no substrate crossing required, and no
promotion to an entity required.

This is the path the research pack is designed to consume. Findings and claims both live as
notes, and the relation supplies polarity and weight that `annotates` cannot carry. The
`Note → Note` form is enforced at the **substrate level** (any note kind to any note kind),
exactly like `supersedes`. It is not part of the kind-level entity allowlist.

#### Secondary rail: Entity→Entity (kind-restricted)

When both the evidence and the claim have been promoted to first-class entities, the
kind-restricted entity form applies. Evidence may be a concept, document, dataset, or
artifact. The claim must be a `concept` entity — the kind used for "a named idea /
hypothesis / assertion." This yields the base entity allowlist:

| Source     | Relation               | Target    | Meaning                                            |
| ---------- | ---------------------- | --------- | -------------------------------------------------- |
| `Concept`  | `supports` / `refutes` | `Concept` | One claim/finding is evidence for/against another  |
| `Document` | `supports` / `refutes` | `Concept` | A paper or report is evidence for/against a claim  |
| `Dataset`  | `supports` / `refutes` | `Concept` | A benchmark/corpus result is evidence for/against  |
| `Artifact` | `supports` / `refutes` | `Concept` | An experiment run / output is evidence for/against |

The target is **always `Concept`** in the entity-form base contract. Other entity target kinds
(e.g. `supports` a `project` or a `service`) are not in the base contract; a future pack adds
them via `EDGE_RULES` (additive only, per ADR-017), or a follow-up ADR amends the base.

Event and edge endpoints are invalid for `supports`/`refutes`, identical to `supersedes`.

### Endpoint contract — sub-decision on claim representation

A claim may be represented in one of two substrate forms:

- **Note form**: the claim is a note (question, decision, insight, or observation). This is the
  natural form for hypothesis text that does not yet warrant a named entity. The `Note → Note`
  rail serves this case directly.
- **Entity form**: the claim is a `concept` entity. `concept` is the only legal entity-form
  target in the base contract. It is already the kind for "a named idea/hypothesis/assertion."

ADR-055 constrains only that evidence and claim share substrate. **Which rail the research pack
uses — claims-as-notes or claims-as-promoted-concepts — is the research pack's own design
decision** (to be specified in its own ADR). ADR-055 provides both same-substrate rails and
does not mandate one over the other for consumers.

### Cascade behavior

`supports` and `refutes` are evidential-lineage-sensitive. On **hard-delete** they cascade the
incident edge (no dangling references) and emit a warning event, mirroring `derived_from` /
`supersedes` / `precedes`:

| Relation               | Cascade behavior                                |
| ---------------------- | ----------------------------------------------- |
| `supports` / `refutes` | cascade edge; emit evidential-link-loss warning |

Deleting evidence or a claim silently discards the recorded confidence signal otherwise; the
warning makes the loss observable to anyone watching the substrate event log. No hard block on
delete (same posture as all other relations).

## Brain-pack wiring (contract here; implementation is Phase 6)

The intended long-term consumer is confidence scoring. The intended wiring:

- `supports` edges into a claim → **positive (α)** evidence on that claim's Beta posterior.
- `refutes` edges into a claim → **negative (β)** evidence.
- Edge weight scales the magnitude of the α/β increment (strong evidence moves the posterior
  more than weak evidence).

**This ADR specifies the contract; it does NOT implement the wiring.** Brain-pack consumption of
`supports`/`refutes` is a **named Phase 6 task** (see IMPL_PLAN, Phase 6). The near-term
consumer is the research pack, which uses the `Note → Note` rail. Phase 1 ships the relations as
first-class edges — creatable, queryable, validated, cascade-handled — with no brain-pack
behavior change. This keeps the type/ontology change small, reviewable, and independently
mergeable, and lets the brain wiring be designed against real evidential edges rather than
speculatively.

## Limitations / explicitly out of scope (v1)

**No neutral / inconclusive relation.** A null or ambiguous result — strong evidence of no
effect, a high-powered replication that returns a flat distribution — is not modeled by the
pair. This is a genuine gap: a Beta posterior tracks trials, not just successes and failures,
and the current representation cannot distinguish "one null result" from "no evidence at all."
This limitation is revisited with the Phase 6 brain/Beta design, because the right shape (a
`tests` / `evaluates` relation, or trial-count metadata on the weight) depends on that scorer's
interface. Adding a third relation before the scorer exists would speculate on the wrong shape.

**No `person` or `org` evidence sources in the base contract.** Expert testimony ("Yann LeCun
supports the claim") is a real epistemic category, but it is distinct from paper, benchmark, and
artifact evidence. It is addable later via pack `EDGE_RULES` or an ADR-002 amendment without
breaking v1 consumers.

**No finer-grained methodological relations.** `replicates`, `cites`, `consistent_with`, and
`contradicts` are real distinctions that affect Bayesian update magnitude. v1 carries
methodological strength in the weight; the system cannot itself distinguish "one gold-standard
replication" from "a dozen weak correlational studies" beyond weight magnitude. Typed refinements
are additive future amendments, deferred until the workload demonstrates the need.

**Note-evidence → concept-claim crossing is disallowed (same-substrate).** When you hold a
finding note and the claim is a concept entity, the resolution is to keep the claim as a note
and use the `Note → Note` rail, or promote the finding to an entity and use the `Entity →
Entity` rail. `annotates` remains the only relation that crosses substrates, and that invariant
is not changed here.

**The relations ship ahead of the brain-pack confidence consumer.** The near-term consumer is
the research pack (active design) via the `Note → Note` rail. The brain Beta-posterior wiring is
Phase 6. This is deliberate sequencing: shipping the ontology first lets the research pack
design against real typed edges rather than a polarity-blind `annotates` proxy. It is not
speculative growth — it is staged delivery with a named next consumer.

## Sub-decisions — review record

The core decision — _add `supports`/`refutes` as the minimal epistemic pair_ — is Ocean's
explicit call and is **not** open for relitigation. The following are **lambda:khive's design
choices** in service of that decision. The adversarial review verdict and resolution are recorded
for each.

1. **Claim representation** (REFRAMED). A claim may be a `concept` entity (entity form) OR a
   note — question, decision, insight (note form). `concept` is only the legal entity-form
   target; the Note form permits note-claims directly without any entity promotion. ADR-055
   provides both same-substrate rails; which the research pack uses is the research pack's ADR
   to specify. The mirror's objection (conflating nouns with propositions) is dissolved: the
   Note→Note rail treats claims as notes, where propositional content lives naturally.

2. **Four evidence source kinds** (deliberate v1 minimalism; see Limitations). `concept`,
   `document`, `dataset`, `artifact` are the kinds that produce citable research artifacts. The
   mirror's point about `person` / `org` exclusion is acknowledged: expert testimony is a real
   epistemic category, excluded from the base contract because it is a distinct epistemic mode
   from experimental/document evidence. It is addable later without breaking v1.

3. **Same-substrate rule** (RESOLVED — mirror misread). The mirror assumed claims MUST be
   concept entities and concluded same-substrate "breaks the research pack" because finding notes
   cannot target concept entities. That reading is incorrect. The `Note → Note` rail serves the
   research pack's findings-as-notes / claims-as-notes pattern directly. Same-substrate stands;
   the design is unchanged. Cross-substrate is explicitly not needed.

4. **Polarity in relation, strength in non-negative weight** (partial concession documented as
   Limitation). The mirror's point on neutral/null results is genuine: a null result does not
   fit cleanly into `supports` or `refutes`, and a true Beta model tracks trials. This is
   recorded as a v1 Limitation, revisited with Phase 6. The polarity-in-relation +
   non-negative-weight convention stays for v1; it is the simplest correct encoding for the
   binary case.

5. **No finer-grained relations** (deliberate v1 minimalism; see Limitations). The mirror's
   point that `supports` conflates methodological strength distinctions is acknowledged and
   recorded in Limitations. The decision to carry methodological nuance in the weight rather than
   in relation type is a v1 trade-off, not an oversight. Typed refinements remain additive future
   amendments.

6. **Brain wiring deferred to Phase 6** (deliberate sequencing). The near-term consumer is the
   research pack via `Note → Note`. The mirror's framing that there is "no demonstrated workload"
   is rejected: the research pack is in active design and consumes the `Note → Note` rail
   immediately. Phase 6 brain/Beta wiring is the named second consumer; intentional staging is
   not speculative growth.

## Rationale

### Why a new category rather than overloading an existing relation?

ADR-002's "Why 17 specifically?" rationale is explicit: a category is justified when "the
relation within each answers a question no other category covers." Evidence-for / evidence-against
is precisely such a question — it is the first relation pair whose primary signal is **epistemic
polarity**, and it directly feeds confidence scoring. Folding it into `annotates` would discard
the polarity and the directed entity-to-entity link, which is the whole point.

### Why the minimal pair, and not `cites` / `replicates` / `consistent_with` / `contradicts`?

`supports`/`refutes` is the smallest set that closes the evidence-for/against query class.
Finer-grained relations (`replicates` as a strong `supports`, `cites` as attribution distinct
from endorsement, `consistent_with` as weak symmetric agreement) are real but **separable**
concerns. Adding them now would over-fit the ontology before the workload demands them. They are
explicitly recorded in Limitations as additive future amendments. The closed-set discipline
(ADR-002) is to expand by demonstrated query class, one minimal increment at a time — this ADR
follows the same 13→15 precedent that added `derived_from` and `precedes`.

### Why directional, not symmetric?

Evidence bears on a claim asymmetrically. A symmetric relation would imply the claim is equally
"evidence about" the paper, which is false and would corrupt traversal ("find evidence for claim
X" must not return claims that X is evidence for). The two symmetric relations
(`competes_with`, `composed_with`) describe genuinely peer relationships; this is not one.

## Migration

**No database migration is required.** Edge relations are stored as a SQL `TEXT` column
([ADR-002](ADR-002-edge-ontology.md) Implementation §; `Edge.relation` serialized via `Display`).
Adding `Supports` / `Refutes` enum variants changes no schema — existing databases accept the
new relation strings the moment the binary recognizes them. No `VersionedMigration`, no new
`sql/NNN-*.sql` file. (Confirmed against `crates/khive-db/src/migrations.rs`: the edge table
stores `relation` as TEXT with no CHECK constraint enumerating relations; validation is
runtime-layer, not schema-layer.)

## Implementation

`EdgeCategory` gains one variant; `EdgeRelation` gains two. In `khive-types/src/edge.rs`:

```rust
pub enum EdgeCategory {
    Structure,
    Derivation,
    Provenance,
    Temporal,
    Dependency,
    Implementation,
    Lateral,
    Annotation,
    Epistemic, // supports, refutes
}

pub enum EdgeRelation {
    // ... existing 15 ...
    // Epistemic
    Supports,
    Refutes,
}
```

`ALL` becomes `[Self; 17]`; `VALID_NAMES` gains `"supports"`, `"refutes"`; `category()`,
`as_str()`, and `FromStr` gain the two arms. `is_symmetric()` is unchanged (both return false via
the existing fallthrough). Endpoint validation in `khive-runtime` routes `supports`/`refutes`
into the same-substrate branch alongside `supersedes`, and the base entity allowlist gains the
four `* → concept` rows. No DSL parser, SQL compiler, or merge-layer change is required — those
layers treat relations as opaque strings validated downstream. Full ordered breakdown:
`.khive/workspaces/20260614/supports-refutes/IMPL_PLAN.md`.
