# ADR-069: The Subject Model -- Domain-Ontology Ingestion and Map Pipeline

**Status**: Proposed\
**Date**: 2026-06-23\
**Authors**: khive maintainers
**Depends on**: ADR-001 (Entity Kind Taxonomy), ADR-002 (Edge Ontology), ADR-013 (Note Kind
Taxonomy), ADR-017 (Pack Standard -- extends `EndpointKind` with `EntityOfType` variant)

## Context

khive stores typed entities and typed edges whose semantics are governed by closed, auditable
taxonomies. The current ingestion pattern -- an external corpus feeds `concept` entities and
`enables` edges because `concept->concept depends_on` is not in the base endpoint contract
(operations.rs:253-260) -- demonstrates the limitation: the entity kind is too coarse to
distinguish a theorem from a definition, and the relation is an inversion approximation rather
than the correct consumer-to-dependency direction.

The broader motivation is a paradigm shift in how knowledge maps are built. Natural-language
embedding applied to formal or technical entities (Lean signatures, drug names, gene identifiers,
legal citations) does not recover their structure: the neighborhood in 768-dimensional embedding
space reflects surface text, not the declared ontological relationships. The result is an
unauditable blob -- a mathematician cannot verify that FlashAttention's nearest neighbors in
embedding space are actually structurally adjacent.

The alternative: the domain itself declares its taxonomy and its typed relations. The ingestion
pipeline reads that declared structure and maps it onto khive's closed ontology. The embedder is
demoted to texture and fuzzy nearest-neighbor lookup only. Maps produced this way are
expert-auditable (a clinician can verify a drug-class adjacency as right or wrong; a logician
can verify a proof dependency) and pipeline-farmable across domains that satisfy the
qualification test below.

This ADR specifies the **Subject model** -- the upstream ingestion-and-ontology pipeline that
turns a domain corpus into typed entities, typed edges, and a 2D layout ready to load into the
KG -- and records the decisions that shape it.

### Why the paradigm inverts the "atlas" approach

The prior atlas approach used an embedder to discover structure from text. This yields:

- Unauditable placement: no expert can confirm or refute a position without re-running the
  embedder.
- No novelty axis: "sparse region of the map" has no declared meaning because no declared
  taxonomy exists to define density.
- No farmability: each new domain requires human judgment to decide what counts as structure.

Subjects invert this: the declared ontology IS the structure; embedding discovers texture
within that structure. Maps become auditable and the production pipeline becomes repeatable
without re-authoring the ontology.

### Source-code grounding

**`concept->concept depends_on` is not in the base endpoint contract.**
`base_entity_rule_allows` in `crates/khive-runtime/src/operations.rs` (lines 253-260) lists
the allowed `DependsOn` triples. `("concept", EdgeRelation::DependsOn, "concept")` does not
appear. Pack `EDGE_RULES` are additive (ADR-017 §"Pack-extensible edge endpoints", lines
451-480); a formal-math pack can add new-kind-to-new-kind endpoint triples without touching
the base contract. This is the mechanism D4 relies on.

**`entity_type` subtypes are pack-registered, not enum-extension.**
ADR-001 §"Pack extensibility rule" (lines 142-149) specifies: `Pack::ENTITY_KINDS` entries
must resolve to a closed base `EntityKind` plus a registered `entity_type`. The shared
`EntityKind` enum stays at 8 base variants; pack-declared kind tokens are subtype
registrations. Formal-math entity kinds (`theorem`, `definition`, etc.) resolve to a base
`EntityKind` at load time.

**EDGE_RULES are declared as a const on the `Pack` trait; endpoint matching requires
`EndpointKind::EntityOfType`, not `EntityOfKind`.**
`Pack::EDGE_RULES: &'static [EdgeEndpointRule] = &[]` (`crates/khive-types/src/pack.rs`,
`Pack` trait definition). The two current `EndpointKind` variants are `NoteOfKind(&'static str)`
and `EntityOfKind(&'static str)` (pack.rs lines 163-168). `NoteOfKind("task")` works because
`Note.kind` is set to the granular kind string. `EntityOfKind("theorem")` does NOT work for
entity subtypes: `endpoint_matches` in `operations.rs` (lines 186-192) compares the `k`
argument against the base kind string returned by `resolved_pair` (lines 175-184), which for
an entity returns `e.kind.as_str()` -- the base `EntityKind` string (`"concept"`), never the
subtype. The comparison `"theorem" == "concept"` is always false; the rule can never fire.

The correct mechanism requires a new variant `EndpointKind::EntityOfType(&'static str)` that
matches against `Entity.entity_type` (entity.rs line 127, `Option<String>`), which IS the
pack-registered subtype string. This requires:

1. A new `EntityOfType` variant in `crates/khive-types/src/pack.rs`
2. `resolved_pair` in `operations.rs` surfacing `entity_type` alongside the base kind
3. A new arm in `endpoint_matches` for `EntityOfType`

This ADR extends ADR-017's `EndpointKind` by introducing this variant. A formal-math pack
can then declare `EdgeEndpointRule { relation: DependsOn, source: EntityOfType("theorem"),
target: EntityOfType("definition") }` in its `EDGE_RULES`, making that triple legal without
modifying the base contract or the `EdgeRelation` enum.

**The `EdgeRelation` enum is closed at compile time.**
`EdgeRelation` is defined in `crates/khive-types/src/edge.rs`. It is a Rust enum. Subjects
cannot add enum variants; they can only add endpoint rules for existing relations.

## Decision

### Component specification

A Subject is a knowledge vertical's ingestion-and-ontology pipeline. It sits above the pack
layer. It is corpus-blind and reusable; the specific corpus is a per-run input.

A Subject consists of four reusable components and two per-run inputs.

**Reusable components (corpus-blind):**

1. **OntologySpec** -- the declared contract for a vertical. Three parts:
   - The entity kind tokens this vertical registers (subtype declarations against a base
     `EntityKind`, per ADR-001 §"Pack extensibility rule").
   - The relation mappings: how this vertical's structural relations map onto khive's 17
     closed edge relations as additive `EDGE_RULES` (per ADR-017 §"Pack-extensible edge
     endpoints"). A vertical cannot add new relations -- only new legal endpoint triples for
     existing ones.
   - The taxonomy configuration that drives Layout: how to derive the discipline and
     subdiscipline labels from the corpus's own declared structure (module path, namespace
     hierarchy, or published classification scheme).

   The OntologySpec is the single source of truth for the downstream pack's vocabulary and
   `EDGE_RULES`. The pack is generated from or constrained by the OntologySpec -- there is no
   separate place to declare vocabulary.

2. **Scanner** -- the only source-format-specific component. Signature: `scan(source) ->
   RawDecls`. It normalizes a corpus into uniform records of the form:
   ```
   { name, kind, type_expr, refs, doc, module }
   ```
   where `refs` is the corpus-provided reference list. The Scanner is a pluggable adapter.
   Lean (lake/AST extract) is the first adapter; Coq, Isabelle, or any formal-math toolchain
   that emits structured declarations can plug into the same downstream pipeline by
   implementing the same `RawDecls` shape. Drawing this boundary at the Scanner -- rather
   than treating the Lean-specific extraction as the pipeline itself -- is a deliberate
   decision (see D1 rationale).

3. **Extractor** -- ontology-driven and source-blind. Operates on `RawDecls` only. Uses the
   OntologySpec's relation mappings to assign `EdgeRelation` values to structural
   relationships extracted from the record fields. Produces typed entities and typed edges.

4. **Layout** -- taxonomy-driven 2D placement, source-blind. Assigns each entity a 2D
   position based on the field's OWN declared taxonomy. Uses no embeddings. The pipeline:
   discipline/subdiscipline labels from the OntologySpec taxonomy config -> inter-discipline
   affinity matrix from cross-reference counts -> MDS on the affinity matrix -> circle-packing
   repulsion -> phyllotaxis sub-centroids -> in-degree-weighted micro-scatter -> normalization
   to [-100, 100].

**Per-run inputs (not components -- specified at run time):**

- **Source** -- the specific corpus artifact (e.g., a built mathlib4 lake project, a Lean
  `.olean` extract file). The same FormalMathSubject runs on any formal-math corpus that the
  Scanner can process. The source is not baked into the Subject.
- **Target store and namespace** -- the SQLite database file and namespace to write into. In
  development this is `unsorry-proofs.db` with namespace `mathlib`. The production KG
  (`khive.db`) is never a valid target for Subject ingest (see D5 hard constraint).

### D1: Subject and Pack are separate layers

A Subject is an upstream ingestion-and-ontology pipeline. A Pack is a downstream runtime
schema-and-verb surface. They are different abstraction layers and must not be merged.

A Subject owns: scanner, extractor, layout, and the OntologySpec that constrains the pack.

A Pack owns: verb handlers, note/entity kind registrations, `EDGE_RULES`, and the `PackRuntime`
trait implementation (ADR-017). Packs have no concept of a corpus, a scanner, or a 2D layout.

The OntologySpec drives the pack (single source of truth). The formal-math pack's
`ENTITY_KINDS` and `EDGE_RULES` are read from or constrained by the OntologySpec, so ingestion
ontology and runtime schema cannot drift.

### D2: Domain relations map onto the 17 closed edge relations, additively

The 17 `EdgeRelation` enum variants (ADR-002, amended by ADR-055) are a closed compile-time
Rust enum (`crates/khive-types/src/edge.rs`). A Subject cannot add enum variants.

A Subject's OntologySpec maps its structural relations onto the existing 17 by declaring
additive `EDGE_RULES` on its pack -- new legal `(source_kind, relation, target_kind)` endpoint
triples for existing relations. The base endpoint contract (operations.rs:215-292) is
unchanged; the rules broaden it for the pack's registered kinds.

For formal mathematics: a Lean declaration referencing another is a consumer-to-dependency
relationship. This maps to `depends_on`. A typeclass/structure inheritance relationship maps
to `extends`. Both use existing `EdgeRelation` variants; the pack's `EDGE_RULES` make the
new-kind endpoint pairs legal.

### D3: Formal-math entity kinds are pack-registered subtypes

The formal-math Subject registers five entity kind tokens:

| Token        | Base `EntityKind` | Meaning in Lean/formal-math                                   |
| ------------ | ----------------- | ------------------------------------------------------------- |
| `theorem`    | `Concept`         | A proved proposition (`theorem`, `lemma`, `corollary`)        |
| `definition` | `Concept`         | A named defined object (`def`, `noncomputable def`, `abbrev`) |
| `structure`  | `Concept`         | A record type or typeclass declaration (`structure`, `class`) |
| `instance`   | `Concept`         | A typeclass instance (`instance`)                             |
| `axiom`      | `Concept`         | An unproved postulate (`axiom`, `opaque`)                     |

These resolve to `EntityKind::Concept` plus the registered `entity_type`. The shared
`EntityKind` enum is not changed (ADR-001 §"Pack extensibility rule" prohibits new enum
variants without an ADR). The `Pack::ENTITY_KINDS` declaration is a subtype registration,
not an enum extension.

For endpoint rules to fire against these subtypes, `EDGE_RULES` must use
`EndpointKind::EntityOfType("theorem")`, not `EntityOfKind("theorem")`. The distinction is
mechanically critical: `resolved_pair` (operations.rs lines 175-184) returns the base kind
string (`"concept"`) for all entities; the subtype is in `Entity.entity_type`. The
`EntityOfType` variant (introduced by this ADR) matches against `entity_type` rather than
the base kind string.

`Concept` (the coarse base kind) is excluded from direct use in formal-math ingest because
it is too coarse to be auditable: a query for `kind=theorem` must return only theorems, not
every `Concept` entity in the store.

`formula` is not a separate kind. In Lean, a formula is the `type` field of a theorem or
definition -- it is not an independent declaration. Modeling it as a separate entity kind
would split information that belongs to a single record and create dangling references.

### D4: `depends_on` over `enables` for reference dependencies

A Lean declaration's `refs` field lists the declarations it uses -- the transitive
dependencies needed to type-check and compile it. The consuming declaration cannot complete
without those dependencies. This is the exact semantic of `depends_on` (consumer ->
dependency, hard requirement) as defined in ADR-002 §Category 5.

With the `EndpointKind::EntityOfType` variant (introduced by this ADR), the endpoint triple
`theorem depends_on definition` can be declared in `EDGE_RULES` as:

```
EdgeEndpointRule {
    relation: DependsOn,
    source: EntityOfType("theorem"),
    target: EntityOfType("definition"),
}
```

The runtime matches `EntityOfType("theorem")` against `entity_type == Some("theorem")` in the
entity record, never against the base kind string. This dissolves the earlier
`concept->concept depends_on` problem: the base contract forbids `concept depends_on concept`
(operations.rs:253-260 does not include that triple), but the `EntityOfType` rule fires on
the subtype field, so it is a distinct triple that never broadens or conflicts with
the base `concept->concept` contract.

`enables` (prerequisite enables outcome) is the inverted approximation: it reverses the
direction and implies "makes possible" rather than "hard requirement." The Extractor used
`enables` in its first pass (visible in `gen_edge_ops.py`, which explicitly notes "depends_on
is NOT c->c in base, only under draft ADR-065 which isn't loaded") precisely because
`concept->concept depends_on` was not available. With pack-registered kinds, `depends_on` is
both semantically correct and mechanically available.

`depends_on` also carries a `dependency_kind` qualifier (ADR-002 §"`depends_on` governed
metadata"). For formal-math reference dependencies, `dependency_kind="runtime"` (the compiled
proof requires the referenced declaration to be present) is appropriate; the Extractor can
default or infer this.

### D5: Transcribe, do not invent

The OntologySpec reads the domain's own declared structure. It does not author a taxonomy
from the Subject team's judgment.

Formal evidence of this principle in the existing pipeline:

- `phase2_select.py` lines 58-61: `discipline = parts[1]` and `subdiscipline = parts[2]`,
  reading directly off mathlib's module path (`Mathlib.Algebra.Group.Basic` ->
  discipline=`Algebra`, subdiscipline=`Group`). The taxonomy is mathlib's own namespace
  hierarchy.
- `build_taxonomy_layout.py`: the affinity matrix is built from cross-discipline reference
  counts read from `refs_in_corpus` -- no external semantic judgment is applied. The
  silhouette score is computed at runtime in step 6 (~line 294) as a **legibility metric
  only** -- the script notes: "layout is label-driven; high is expected. The structural
  evidence is the affinity matrix below." The value is not persisted; it is expected to be
  high precisely because the layout is label-driven, and it carries no structural weight.

Permitted curation at the margin (not origination):

- Selecting or reconciling among multiple declared ontologies when a domain has more than one.
- Light curation of an incomplete declared structure: dropping tooling namespaces
  (`TOOLING_DISCIPLINES = {"Tactic", "Util", "Deprecated", ...}` in `phase2_select.py`
  line 24), applying per-discipline caps to oversized buckets. Both are curation at the margin
  of a transcription.

**Three-ingredient qualification test** for whether a domain is a valid Subject:

1. A declared machine-readable vocabulary -- the domain's own type system, taxonomy, or
   ontology provides entity names and kinds without NL interpretation.
2. Typed relations recoverable from source structure -- not from reading prose, but from
   structural artifacts: AST, citation graph, statute hierarchy, gene-disease association
   databases.
3. A declared taxonomy for layout -- a hierarchy that the domain itself has published or
   encoded (module paths, MeSH hierarchy, SNOMED tree, statute chapter structure).

All three present -> valid Subject. Examples:

| Domain               | Vocabulary source                | Relation source                      | Taxonomy source           | Valid?                                                                                  |
| -------------------- | -------------------------------- | ------------------------------------ | ------------------------- | --------------------------------------------------------------------------------------- |
| Formal proofs (Lean) | AST declaration kinds            | `refs` field (typed in v2 target)    | Module path hierarchy     | YES                                                                                     |
| AMR / antimicrobials | CARD antibiotic resistance genes | Gene-drug-organism association links | CARD drug class hierarchy | YES                                                                                     |
| Medicine             | SNOMED / MeSH                    | NCBI gene-disease, DrugBank          | ICD-10 / SNOMED hierarchy | YES                                                                                     |
| Legal (statutes)     | Statute/section identifiers      | Citation graph + cross-references    | Chapter/part hierarchy    | PARTIAL -- transcribe citation graph + statute hierarchy; omit fuzzy doctrinal grouping |
| General research     | None canonical                   | None structural                      | None declared             | NO -- falls back to embedding                                                           |

The temptation to classify a domain by modeling it ourselves is the signal that the domain
does not meet the three-ingredient test and should be excluded from Subject treatment.

## Rationale

### Why Subject and Pack are separate layers (D1)

A Pack is specified entirely by ADR-017: it implements the `Pack` and `PackRuntime` traits,
declares `HANDLERS`, `NOTE_KINDS`, `ENTITY_KINDS`, and `EDGE_RULES` as const items, and
exposes verb dispatch through the VerbRegistry. Packs have no corpus concept, no scanner, no
layout pipeline.

Forcing a Subject's scanner and layout into a pack would conflate two responsibilities with
different lifetimes: packs run continuously as the runtime schema during every MCP session;
ingestion pipelines run once per corpus import. The abstraction boundary keeps the runtime
lean and the ingestion pipeline testable outside the MCP server.

The "scanner as pluggable adapter" boundary is chosen for extensibility -- not anticipated
extensibility (the anti-pattern) but demonstrated need: the formal-math domain already has
multiple proof assistants (Lean, Coq, Isabelle, Agda) with different AST formats but the
same downstream ontology. One OntologySpec, one Extractor, one Layout, multiple Scanners is
the minimal structure that avoids re-implementing the ontology for each proof assistant.

### Why additive EDGE_RULES, not relations-as-data (D2)

The closed `EdgeRelation` enum is the system's auditability mechanism. Every edge in the store
has one of 17 semantically defined relation types. An agent traversing the graph knows exactly
what `depends_on` means, regardless of which domain the entities belong to. A query for all
`depends_on` edges returns all dependency relationships across all subjects in the same store.

"Relations-as-data" -- allowing each Subject to define its own relation strings -- destroys
this property. The graph becomes a heterogeneous bag of domain-specific strings with no shared
semantics. Query authoring requires knowing per-domain relation vocabulary. Cross-domain
traversal ("find everything that any entity in domain A depends on") becomes impossible without
prior knowledge of all relation strings each domain uses.

The `EDGE_RULES` mechanism (ADR-017:451-480) already handles this correctly. The GTD pack uses
exactly this pattern to make `depends_on: task -> task` legal without modifying the base
contract. The formal-math pack follows the same pattern.

### Why pack-registered subtypes, not coarse `concept` (D3)

A query for theorems should return only theorems. With coarse `concept`, every formal-math
entity is a `concept` and "show me all theorems" requires a property filter on an ungoverned
field -- exactly the pathology ADR-001 was written to fix (§"Why governed entity_type instead
of open properties.type"). Pack-registered subtypes give `kind=theorem` as a first-class filter
predicate.

`formula` is excluded because it is not a first-class Lean declaration kind -- it is the
`type` field (the proposition or return type) of a theorem or definition. Modeling it as a
separate entity creates a many-to-one explosion (every declaration would spawn a formula
entity) with no independent identity: a formula has no standalone lifecycle, no `refs` list of
its own at the declaration level, and cannot be referenced directly by other declarations.

### Why `depends_on` is correct and `enables` was an approximation (D4)

The design brief for this ADR records the actual runtime situation: `gen_edge_ops.py` in the
current mathlib workspace explicitly uses `enables` and documents the reason ("depends_on is
NOT c->c in base, only under draft ADR-065 which isn't loaded"). With pack-registered kinds,
the workaround is no longer needed -- `theorem depends_on definition` is a valid triple under
the pack's `EDGE_RULES` -- and the correct relation can be used.

`enables` direction (prerequisite -> outcome) is inverted relative to the actual dependency:
the theorem that USES a lemma is the consumer; the lemma is the dependency. A consumer-to-
dependency direction (`depends_on`) is semantically precise. Traversing "what does this theorem
depend on?" is a natural `depends_on` traversal; with `enables` it requires inverting the
direction, which is non-obvious and error-prone for agent callers.

### Why transcription is the auditability moat (D5)

Expert auditability requires that the map can be verified by someone who knows the domain. A
map derived from the domain's own declared structure can be verified: a logician can check
whether `Nat.add_comm` is adjacent to `Nat.add_assoc` in the map and whether their structural
relationship justifies it. A map derived from embedding similarity cannot be verified by a
human expert -- the position depends on 768 numbers that encode distributional co-occurrence,
not structural meaning.

Transcription also enables farmability: the pipeline from declared ontology to KG entities and
edges does not require domain expertise at construction time (an expert need not author the
Subject, only audit the map). This is how the same pipeline can produce maps for AMR antibiotic
resistance genes, SNOMED disease hierarchies, and formal proofs without re-architecting.

## Alternatives Considered

### A1: Embed the ingestion pipeline inside a Pack

**Rejected.** A pack is a runtime schema-and-verb component. Its lifetime is the MCP server
session. Ingestion is a batch pipeline that runs once or periodically per corpus import.
Conflating them overloads the pack abstraction and makes the ingestion pipeline impossible to
test outside the running MCP server. ADR-017 §"Pack Standard" is clear that packs contribute
kinds, verbs, and edge rules -- not corpus-scanning or layout logic.

### A2: Per-Subject relation sets (relations-as-data)

**Rejected.** Allowing each Subject to define its own relation strings destroys the
closed-ontology value of the graph (see Rationale for D2). Cross-domain traversal loses shared
semantics. Existing packs (GTD, memory, brain) demonstrate that additive `EDGE_RULES` suffice
for any domain-specific endpoint extension without polluting the shared relation vocabulary.

### A3: Extend the `EntityKind` enum for formal-math entity kinds

**Rejected.** ADR-001 §"Pack extensibility rule" explicitly prohibits new `EntityKind` variants
without an ADR and demonstrates that the correct mechanism is `entity_type` subtype
registration. The practical cost of enum extension is high: every `match` arm on `EntityKind`
in the runtime, storage, and pack layers must be updated. Pack-registered subtypes achieve the
same query precision (`kind=theorem`) through the existing `EntityTypeRegistry` path without
touching the enum.

### A4: Use `enables` permanently for reference dependencies

**Rejected.** `enables` (prerequisite -> outcome) inverts the consumer-to-dependency direction
of a Lean declaration reference. The `depends_on` relation carries the correct semantics and
the correct `dependency_kind` metadata slot. The pack `EDGE_RULES` mechanism makes
`theorem depends_on definition` legal. The only reason `enables` was used in the initial
pipeline pass was the unavailability of `concept->concept depends_on` -- a constraint that
dissolves once pack-registered kinds are in place.

### A5: Use a general-purpose embedding approach for all domains

**Rejected.** Embedding-based maps are not auditable by domain experts, cannot provide a
novelty axis grounded in declared structure, and require per-domain prompt engineering to
extract structure from prose. The three-ingredient qualification test defines the scope:
domains that satisfy it get Subject treatment; domains that do not fall back to embedding.
The two approaches are not in competition -- they cover different domain classes.

### A6: Add formal-math entity kinds as new base `EntityKind` enum variants

**Rejected.** Adding `Theorem`, `Definition`, `Structure`, `Instance`, `Axiom` as new
`EntityKind` variants would require updating every `match` arm on `EntityKind` across the
runtime, storage, and pack layers. ADR-001 §"Pack extensibility rule" explicitly defines the
correct mechanism as `entity_type` subtype registration -- `Pack::ENTITY_KINDS` entries map
domain-specific kind tokens to a closed base `EntityKind` plus a registered `entity_type`.
The pack extensibility path achieves equivalent query precision (`kind=theorem`) through the
`EntityTypeRegistry` without touching the shared enum.

### A7: Use `EntityOfKind("theorem")` in `EDGE_RULES` without runtime changes

**Rejected.** `endpoint_matches` (operations.rs lines 186-192) compares the `EndpointKind`
argument against the base kind string from `resolved_pair` (lines 175-184), which for entities
returns `e.kind.as_str()` -- always the base `EntityKind` string (`"concept"`). A rule
keyed on `EntityOfKind("theorem")` evaluates `"theorem" == "concept"`, which is always false.
The rule would be silently inert at runtime. This is not a configuration choice; it is a
mechanical consequence of how `resolved_pair` works. A new `EntityOfType` variant that reads
`Entity.entity_type` is the minimal correct fix.

### A8: Use a broad `concept depends_on concept` pack rule

**Rejected.** Declaring an endpoint rule permitting `concept depends_on concept` for all
concepts would broaden the contract beyond the Subject's intent. Every `concept` entity in any
pack -- not just formal-math entities -- would become a valid endpoint for `depends_on`. This
defeats the purpose of the closed endpoint contract, which exists to prevent semantically
incorrect traversals. The `EntityOfType("theorem")`/`EntityOfType("definition")` approach
scopes the rule to formal-math subtypes only, preserving the integrity of the base contract.

## Consequences

### Positive

- Formal-math maps become expert-auditable: a mathematician can verify any edge by inspecting
  the Lean source. The map is a faithful transcription of the declared structure.
- The ingestion pipeline is domain-farmable: any formal-math corpus (mathlib, standard library,
  user project) feeds the same FormalMathSubject without re-authoring the ontology.
- Entity kind granularity (`theorem`, `definition`, `structure`, `instance`, `axiom`) enables
  precise query filters that `concept` cannot support.
- `depends_on` with `dependency_kind` metadata provides a semantically correct dependency
  graph that supports traversal in the natural consumer-to-dependency direction.
- The OntologySpec is the single source of truth for both ingestion and runtime schema --
  no drift between the two.
- The Scanner boundary means a second proof-assistant adapter (Coq, Isabelle) adds one new
  Scanner implementation, not a full pipeline rewrite.

### Negative

- Subjects add a new architectural layer above packs with no existing Rust trait or crate for
  it. Initial implementation requires bootstrapping this layer.
- The Extractor's relation extraction is currently limited by the `refs` field: the Lean
  scanner's `decls.jsonl` emits a flat, untyped reference list that mixes structural
  dependencies, statement-type dependencies, and proof dependencies. Typed extraction in v1
  requires heuristics (parsing the return-type head symbol to detect `extends` edges, for
  example); authoritative typed refs require a second Lean-level extraction pass (v2 target).
- Not all domains qualify as Subjects. Domains without a machine-readable declared vocabulary,
  typed structural relations, or a declared taxonomy are excluded. This is a feature (it
  prevents fabricated ontologies) but limits the lighthouse demos to a narrower set of domains.

### Neutral

- The `unsorry-proofs.db` / `khive.db` boundary (dev vs. production) is a process constraint,
  not a system constraint. Nothing in the Subject model prevents a misconfigured run from
  targeting the wrong database -- the separation must be enforced operationally.
- The silhouette score on the taxonomy layout (`build_taxonomy_layout.py` step 6, ~line 294)
  is a legibility metric, not a structural quality metric. It is computed at runtime and not
  persisted. The legibility is expected to be high because the layout is label-driven.
  Structural quality is evidenced by the inter-discipline affinity matrix, not the silhouette.

## Implementation

### Formal proofs as the first Subject

The FormalMathSubject instantiates all four components for the Lean proof-assistant ecosystem.

**OntologySpec** declares: five entity kind tokens (`theorem`, `definition`, `structure`,
`instance`, `axiom`) each resolving to `EntityKind::Concept`; relation mappings for
`depends_on` (declaration reference, consumer->dependency) and `extends` (typeclass/structure
inheritance, child->parent), both as additive `EDGE_RULES` using `EntityOfType` endpoint
specifiers (e.g. `EntityOfType("theorem")`, `EntityOfType("definition")`) -- not
`EntityOfKind` -- because endpoint matching for entity subtypes must use `Entity.entity_type`,
not the base kind string (see grounding section and D3); these `EDGE_RULES` require the
formal-math pack to be loaded;
taxonomy configuration reading discipline from `module.split(".")[1]` and subdiscipline from
`module.split(".")[2]` (mathlib's module path, verified in `phase2_select.py` lines 58-61).

**Scanner** (v1 state): a Lean lake/AST extract producing `decls.jsonl` (316,040 records from
the full mathlib4 Phase-1 extract, as recorded in the PHASE2-SELECTION.md filter funnel). Each
record has fields `name`, `kind`, `type`, `doc`, `module`, and `refs`. The `refs` field is a
flat, untyped list: it includes structural dependencies, statement-type dependencies, and proof
dependencies without distinguishing them. This is an honest v1 limitation. Two consequences:

- **`depends_on` edges in v1** are derived from `refs_in_corpus` (refs filtered to known
  corpus members) without type tagging. The Extractor uses all refs as `depends_on` edges with
  `dependency_kind="runtime"`.
- **`extends` edges in v1** are extracted by a return-type-head heuristic: declarations whose
  return type begins with an `X.toY` or structure-projection pattern, intersected with
  `refs_in_corpus`, are candidate `extends` edges. This is a heuristic approximation. The
  authoritative path (v2 target) is a Lean metaprogram that emits typed refs, distinguishing
  `extends` from `depends_on` at the AST level.

**Extractor** (v1 state): reads `selected.jsonl` (62,038 records after Phase-2 selection,
verified in PHASE2-SELECTION.md), emits `create` ops for entities and `link` ops for edges.
The current `gen_create_ops.py` creates entities as `kind="concept"` (pre-ADR-069 state, before
pack-registered subtypes); the v1 Extractor target after this ADR is accepted is to use the
pack-registered `lean_kind` field (`theorem`, `definition`, etc.) as the entity kind token.
The current `gen_edge_ops.py` uses `enables`; after this ADR the relation becomes `depends_on`.

**Layout** (shipped state): `build_taxonomy_layout.py` implements the full pipeline. Taxonomy
is read from module paths. The 26x26 inter-discipline affinity matrix is built from cross-
reference counts. MDS places discipline centroids. Circle-packing prevents territory overlap.
Phyllotaxis places subdiscipline sub-centroids. In-degree-weighted scatter places individual
nodes. Normalization to [-100, 100]. No embeddings at any stage. The silhouette score is
computed at runtime (step 6, ~line 294) as a legibility check; high is expected because
layout is label-driven; structural quality is evidenced by the affinity matrix.

**Phase-2 selection rule** (implemented, `phase2_select.py`): `has_doc OR in_degree >= 3`
(T=3 chosen to land the selected count in [80K, 120K]; landed at 62,038 after per-discipline
caps). Tooling disciplines dropped (`Tactic`, `Util`, `Deprecated`, `Lean`, `Mathport`,
`Testing`, `Std`). CategoryTheory cap = 3,000 (tighter; highly abstract infrastructure).
NumberTheory, Combinatorics, Order: no cap (all notable records kept).

**Two-graph convergence design**: the Subject produces two persistent graphs that serve
different functions:

1. **Semantic map** (`mathlib` namespace in `unsorry-proofs.db`): 62,038 entities drawn from
   the Phase-2 selection. This is the territory -- the comprehensive declared-structure map of
   formal mathematics as recorded in mathlib4.
2. **Proof frontier** (`unsorry-proofs.db`, separate namespace): 768 goal-concepts representing
   the open proof obligations in the unsorry corpus. This is the frontier.

Convergence: overlay proof-frontier goals onto the territory. A goal landing in a sparse region
of the semantic map signals a novel mathematical area. A goal with dense neighbors signals
proximity to established results. This spatial relationship is only meaningful because the
layout is structure-driven and expert-auditable -- a sparse region in an embedding-based layout
carries no such guarantee.

### Hard constraints

- The 17 `EdgeRelation` variants are a closed compile-time enum. A Subject cannot add relation
  kinds -- only additive endpoint rules for existing relations via `Pack::EDGE_RULES`.
- `EDGE_RULES` for entity subtypes MUST use `EndpointKind::EntityOfType`, not
  `EndpointKind::EntityOfKind`. The latter compares against the base kind string (`"concept"`);
  the former compares against `Entity.entity_type`. A rule keyed on `EntityOfKind("theorem")`
  can never fire for formal-math entities (see grounding section).
- Ingest target must be `unsorry-proofs.db` in development. The production `khive.db` is never
  a valid target for Subject ingest. This is a process constraint enforced by configuration.
- No synthesized docstrings or fabricated descriptions may be written to the store. The `doc`
  field from the scanner is used as-is or omitted; provenance must be clean.
- Entity creation uses the pack-registered kind tokens (`theorem`, `definition`, etc.), not the
  bare `concept` kind, once the formal-math pack is loaded. Using bare `concept` for
  formal-math entities after this ADR is a regression.

## References

- ADR-001: Entity Kind Taxonomy -- pack extensibility rule, `entity_type` subtype registration
- ADR-002: Closed Edge Ontology -- `depends_on` and `enables` semantics; endpoint contract;
  `dependency_kind` metadata for `depends_on`
- ADR-013: Note Kind Taxonomy -- note kinds referenced for completeness
- ADR-017: Pack Standard -- `Pack::EDGE_RULES` const, additive endpoint rules, `ENTITY_KINDS`
  as subtype registration, `VerbRegistry::all_edge_rules()` aggregation
- `crates/khive-runtime/src/operations.rs` lines 215-292: `base_entity_rule_allows` --
  implemented base endpoint contract; confirms `concept->concept DependsOn` absent
- Local proof-gate `mathlib/phase2_select.py` lines 58-61: taxonomy
  derivation from module path; lines 24: `TOOLING_DISCIPLINES` drop
- Local proof-gate `mathlib/build_taxonomy_layout.py` steps 2-6:
  Layout pipeline and silhouette annotation
- Local proof-gate `mathlib/gen_edge_ops.py`: records the `enables`
  workaround and its reason ("depends_on is NOT c->c in base, only under draft ADR-065")
- Local proof-gate `mathlib/PHASE2-SELECTION.md`: verified selection
  counts (316,040 raw; 62,038 final selected; T=3 threshold)
