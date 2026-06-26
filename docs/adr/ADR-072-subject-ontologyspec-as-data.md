# ADR-072: Subject OntologySpec as Runtime Data -- Verbless Verticals and Pack Retirement

**Status**: Proposed\
**Date**: 2026-06-26\
**Authors**: Ocean, lambda:khive\
**Extends**: ADR-069 (Subject Model -- component specification and D1)\
**Depends on**: ADR-001 (Entity Kind Taxonomy), ADR-002 (Edge Ontology), ADR-017 (Pack
Standard -- `EDGE_RULES`, `ENTITY_KINDS`, `all_edge_rules()`), ADR-055 (Epistemic Edge
Relations -- `supports`)

## Context

ADR-069 established the Subject anatomy: four corpus-blind components (OntologySpec, Scanner,
Extractor, Layout) and two per-run inputs (Source, Target). It named the OntologySpec the
single source of truth for a vertical's vocabulary and stated that "the pack is generated from
or constrained by the OntologySpec -- there is no separate place to declare vocabulary."

ADR-069 left two points open:

1. **The concrete form of the OntologySpec.** It described the spec's three parts (entity
   subtype tokens, additive endpoint rules, taxonomy configuration) but not how they are
   expressed or loaded.
2. **What a vertical that contributes no runtime behavior looks like.** ADR-069 assumed a
   downstream pack always exists -- "the formal-math pack's `ENTITY_KINDS` and `EDGE_RULES` are
   read from or constrained by the OntologySpec." It did not address the case where the only
   thing the vertical contributes IS that vocabulary.

The formal-math vertical is exactly that case. Its `khive-pack-formal` crate declares five
entity-kind tokens and a set of additive endpoint rules. It registers no verb handlers, holds
no backend, and runs no logic. The crate is a compiled container for static declarations.
Expressing those declarations as Rust means a schema change to a knowledge vertical requires
editing a crate, recompiling, and reshipping the binary, even though nothing executable
changed. ADR-069 anticipated more verticals (literature, AMR); each verbless vertical would add
another declaration-only crate under the same constraint.

This ADR closes both open points: the OntologySpec is a runtime-loaded data artifact, and a
Subject that contributes only vocabulary needs no pack crate at all.

This ADR does not change the Pack abstraction, the closed taxonomies, or the per-run
constraints ADR-069 established. It refines how a verbless Subject's vocabulary is expressed and
loaded.

## Decision

### D1: The OntologySpec is a runtime-loaded data artifact

The OntologySpec is expressed as a data file, not Rust. The canonical on-disk format is TOML;
JSON is also accepted. It is deserialized and loaded at runtime by the runtime's ontology
loader; it is not compiled into a crate.

It declares the three parts ADR-069 specified:

- **Entity subtype tokens** -- each names the base `EntityKind` it refines, per ADR-001's pack
  extensibility rule. (For formal mathematics: `theorem`, `definition`, `structure`,
  `instance`, `axiom`, all refining `Concept`.)
- **Additive endpoint rules** -- each a `(source, relation, target)` triple whose `relation` is
  one of the closed 17 `EdgeRelation` variants (ADR-002, amended by ADR-055), expressed with
  the `EndpointKind` vocabulary including the `EntityOfType` variant ADR-069 introduced. Rules
  are additive only; they broaden the base endpoint contract for the vertical's registered
  kinds and never tighten it.
- **Taxonomy configuration** -- how Layout derives discipline and subdiscipline labels from the
  corpus's own declared structure (module path, namespace hierarchy, or published
  classification scheme).

To make the spec deserializable, the `EndpointKind` and `EdgeEndpointRule` types in
`khive-types` gain serde derives. The relation token in a rule is resolved against the existing
`EdgeRelation` name table; it is not a free string.

### D2: A verbless Subject has no pack crate

A Subject that contributes only vocabulary (entity subtypes, additive endpoint rules, taxonomy
configuration) and no verb handlers, no `PackRuntime` logic, and no backend is expressed
entirely as an OntologySpec data file. The runtime loads it through an ontology loader that
feeds the same two validation surfaces a compiled pack feeds: the registered entity-kind
tokens, and the aggregated endpoint rules (`VerbRegistry::all_edge_rules()`). At the validation
layer a loaded OntologySpec is indistinguishable from a pack's `ENTITY_KINDS` + `EDGE_RULES`
declarations.

This does not change the Pack abstraction. A vertical that contributes behavior (verbs, a
backend, runtime logic) remains a Pack. The capability packs (kg, gtd, memory, comm, brain,
schedule, knowledge) are unaffected. The split is:

- **Behavior is a Pack** -- compiled, implements the `Pack`/`PackRuntime` traits.
- **Pure vocabulary is a Subject OntologySpec** -- data, loaded at runtime.

ADR-069 D1 kept Subject and Pack as separate layers. This ADR removes the implicit requirement
that every Subject's vocabulary be carried by a downstream pack: a verbless Subject carries its
vocabulary as data, with no crate.

### D3: The ontology loader validates against the closed taxonomies and fails closed

At load time the loader rejects any OntologySpec that:

- names a relation token that does not resolve to one of the 17 closed `EdgeRelation` variants,
- declares an endpoint rule that would tighten or contradict the base endpoint contract (rules
  are additive only, per ADR-017),
- declares an entity subtype against a base kind that is not one of the closed `EntityKind`
  variants,
- is structurally malformed (missing required fields, unparseable taxonomy configuration).

A violation aborts the load with an error naming the offending token and the valid
alternatives. There is no silent coercion and no default substitution -- the same discipline
the runtime already applies to invalid kinds and relations (invalid input is an error listing
the valid set, never `unwrap_or_default()`).

Because the spec is data, this validation runs at load time rather than at compile time.
Fail-closed loading is what preserves the guarantee a `const EDGE_RULES` array gave for free: an
invalid ontology never reaches the store; it stops the load.

### D4: `khive-pack-formal` is retired; its vocabulary becomes the formal-math OntologySpec

The five entity-kind tokens and the additive endpoint rules currently compiled into
`khive-pack-formal` move verbatim into a formal-math OntologySpec data file. The crate is
removed.

`khive-pack-formal` is not in the default pack set and registers no verbs, so its removal
changes no runtime verb surface. The migration is a transcription: the same tokens, the same
`(source, relation, target)` triples, expressed as data instead of Rust. ADR-069's hard
constraint that formal-math entities use the registered subtype tokens (`theorem`,
`definition`, ...) rather than the bare `concept` kind is unchanged; the tokens now come from
the loaded OntologySpec instead of a compiled `ENTITY_KINDS` const.

This retirement is not the deletion of forward-deployed logic (ADR-043). `khive-pack-formal`
carries no logic -- only declarations. The declarations are preserved; they move from Rust to
data. Nothing executable is removed.

### D5: The territory and proof-frontier maps are separate Subjects

ADR-069 D3 registers five subtypes for the formal-math **territory** map (`theorem`,
`definition`, `structure`, `instance`, `axiom`). It does not register `goal` or `proof`, and
its Implementation section already places the proof frontier in a namespace separate from the
territory map. This ADR makes that separation explicit at the Subject level: the territory and
the proof frontier are two Subjects, each with its own OntologySpec. Expressing ontologies as
data (D1) makes one-spec-per-concern cheap, which is what makes this split practical rather than
duplicative.

- The **territory Subject** maps a library's declared structure (the five subtypes above), with
  statement-level `depends_on` edges between declarations.
- The **proof-frontier Subject** is an application over that territory. It introduces `goal`
  entities (open proof obligations) and proof artifacts (closing attempts). A `goal` relates to
  the theorem or definition it restates via `variant_of`. A proof is an `artifact`; it relates
  to the goal it discharges via `supports`. ADR-055's epistemic rail already makes
  `artifact -> concept supports` legal, so this needs no new endpoint and no new relation.

A typed `proves` relation distinct from the epistemic `supports` would require amending the
closed 17 `EdgeRelation` enum and is out of scope for this ADR; it is tracked separately. Until
then, `supports` carries the proof-to-goal relationship. The detailed proof-frontier ontology
(the `goal`/`proof` entity modeling and the proof-term `depends_on` population) is specified by
the proof-frontier Subject's own OntologySpec together with the in-flight formal-entity work,
not fixed here. The two `depends_on` populations stay separate: statement-level dependencies
(declaration to declaration, in the territory) and proof-term dependencies (a proof to the
lemmas it invokes, in the proof frontier).

## Rationale

### Why data, not a generated crate

The OntologySpec already IS the single source of truth (ADR-069). A separate compiled crate
that merely re-expresses it adds a build step and a second copy to keep in sync, and buys
nothing for a vertical with no executable surface. Loading the spec as data at runtime removes
the crate, removes the recompile-and-reship cycle for a schema change, and leaves exactly one
copy of the vocabulary.

### Why a verbless Subject has no pack

The Pack abstraction earns its compile-time cost when a vertical contributes behavior: verb
handlers, a backend, runtime logic. A vertical that contributes only vocabulary pays that cost
for nothing. Separating "behavior is a Pack, vocabulary is data" lets each vertical pay only for
what it actually adds. It also makes the cost of a new verbless vertical (literature, AMR) a
data file rather than a crate.

### Why retire pack-formal rather than keep it as the spec

Keeping `khive-pack-formal` as the canonical declaration would mean the data spec and the crate
both exist and must agree -- the exact drift ADR-069 D1 set out to prevent. For a verbless
vertical the crate is pure redundancy. Removing it leaves the OntologySpec as the single,
authoritative declaration, loaded and validated at runtime.

### Why the loader must fail closed

A `const EDGE_RULES` array is validated by the compiler: a malformed rule does not build. Moving
to data forfeits that unless the loader enforces the same invariants. Fail-closed loading
restores the guarantee at a different point in the lifecycle (startup instead of build). A spec
that names an unknown relation, tightens the base contract, or refines an unknown base kind is
rejected with a specific error, never coerced.

## Alternatives Considered

**A1: Build-time codegen (OntologySpec generates a pack crate).** Rejected. This still requires
a recompile and reship for every schema change and still ships a crate per verbless vertical.
The generated code is a redundant artifact derived from the spec. Loading the spec directly at
runtime removes the build step entirely.

**A2: Keep `khive-pack-formal` as a hand-written crate constrained to match the spec.** This is
the "constrained by" reading ADR-069 left open. Rejected. Two sources (the crate and the spec)
must be kept in sync; drift is exactly the failure ADR-069 D1 names. For a verbless vertical the
crate contributes nothing the spec does not already carry.

**A3: A single all-formal Subject covering territory and proof frontier.** Rejected. The two
have different ontologies (five declaration subtypes vs `goal`/`proof`), different namespaces
(already separated in ADR-069's Implementation), and different lifecycles (the territory is a
library snapshot; the frontier changes as obligations are discharged). One OntologySpec per
concern is cleaner, and D1 makes it cheap.

**A4: Resolve the relation-tier question here.** Declined as out of scope. Whether packs or
Subjects may declare native relation labels beyond the closed 17 (a "Tier-2" escape) is a
separate, unsettled decision tracked independently. This ADR stays on the closed-17 path per
ADR-069 D2 and takes no position on it. The `supports`/`proves` treatment in D5 is the current,
no-amendment-needed path, not a resolution of that question.

## Consequences

### Positive

- A schema change to a verbless vertical is a data edit, not a recompile-and-reship. New
  verticals ship their ontology as data.
- One authoritative source per vertical, enforced at load. The crate-vs-spec drift ADR-069 D1
  warned about is structurally impossible because there is no second copy.
- The validation surface is unchanged. Loaded specs and compiled packs are validated through the
  same entity-kind registry and `all_edge_rules()` aggregation.

### Negative

- The compile-time guarantee that a `const EDGE_RULES` array is well-typed is replaced by
  load-time validation. A malformed spec is caught at startup, not at build. Mitigated by
  fail-closed loading (D3) and a spec linter in CI (analogous to `scripts/lint-sql.sh` for
  `.sql`).
- A runtime ontology loader is new surface area to build and maintain.

### Neutral

- Capability packs are unaffected; the `Pack` trait is unchanged. Only verbless verticals move
  to data.
- `khive-types` gains serde derives on `EndpointKind` and `EdgeEndpointRule` (additive, no
  behavior change).

## Implementation

1. Add serde derives to `EndpointKind` and `EdgeEndpointRule` in
   `crates/khive-types/src/pack.rs`. The relation field deserializes through the existing
   `EdgeRelation` name table.
2. Add an ontology loader in `khive-runtime` that reads a TOML or JSON OntologySpec, validates
   it per D3, and registers its entity-kind tokens and endpoint rules into the same surfaces
   packs feed (the entity-kind registry and `all_edge_rules()`).
3. Author the formal-math territory OntologySpec from `khive-pack-formal`'s current `vocab.rs`
   (the five subtypes and the additive endpoint rules). Remove the `khive-pack-formal` crate
   once the data file reproduces its declarations.
4. Add a spec linter to `make ci` (load every ontology spec file into the validator and assert
   it passes), analogous to `scripts/lint-sql.sh`.
5. The proof-frontier OntologySpec and the `goal`/`proof` entity modeling are specified with the
   in-flight formal-entity work, not in this ADR.

## References

- ADR-069: Subject Model -- component specification (OntologySpec as single source of truth),
  D1 (Subject and Pack as separate layers), D3 (five formal-math subtypes and the
  `EntityOfType` variant), Implementation (territory vs proof-frontier namespaces)
- ADR-001: Entity Kind Taxonomy -- pack extensibility rule, `entity_type` subtype registration
- ADR-002: Edge Ontology -- closed 17 `EdgeRelation` variants; additive endpoint contract
- ADR-017: Pack Standard -- `Pack::EDGE_RULES`, `ENTITY_KINDS`, `all_edge_rules()` aggregation,
  additive-only endpoint rules
- ADR-043: forward-deployed crates -- the logic-vs-declaration distinction invoked in D4
- ADR-055: Epistemic Edge Relations -- `supports`/`refutes`; the `artifact -> concept supports`
  endpoint relied on in D5
- `crates/khive-pack-formal/src/vocab.rs`: the declarations migrated to the formal-math
  OntologySpec
- `crates/khive-types/src/pack.rs`: `EndpointKind`, `EdgeEndpointRule` (serde derives added)
