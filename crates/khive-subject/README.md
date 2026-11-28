# khive-subject

A library for turning declaration corpora into typed graph operation plans. It implements
[ADR-069](../../docs/adr/ADR-069-subject-model.md)'s Subject contract, a Lean JSONL scanner,
and a source-independent formal-math extractor. It does not start a server or write a database.

`OntologySpec` supplies three required parts: declaration vocabulary, relation mappings and
taxonomy configuration. Construction refuses missing parts by name. The formal-math spec has
five ingestion tokens on base kind `concept`: `theorem`, `definition`, `structure`, `instance`
and `axiom`. Its compatibility rules preserve all 21 rules from FormalPack, including the
existing runtime `goal` subtype. `goal` is not an ingestion token. The module separator is `.`,
with discipline at zero-based segment 1 and subdiscipline at segment 2.

`Scanner::scan` accepts a reader on every call and returns `RawDecl` records containing
`name`, `kind`, `type_expr`, `refs`, `doc` and `module`. `LeanJsonlScanner` maps JSON `type` to
`type_expr` and accepts either `refs` or `refs_in_corpus`. Supplying both reference fields is
an error, even when their values match. Name, kind, module and reference strings retain their
input spelling and order. Optional `doc` is copied as decoded UTF-8 content: whitespace and
Unicode are preserved, an empty string remains present, and a missing or null value is absent.
Selection metadata is ignored. Blank or malformed lines and read errors refuse the whole scan
with a one-based physical line number; no successful prefix corpus is returned.

`FormalExtractor` consumes those records and an `OntologySpec`. The same interface accepts
records constructed directly in Rust. Corpus membership is rebuilt per call, so
`refs_in_corpus` is not trusted as proof that a referenced declaration survived selection.
Empty or duplicate declaration names and unknown kinds refuse the whole extraction. The
scanner keeps raw kind names; ontology resolution maps existing aliases such as `def` to
`definition` and `inductive`, `struct` or `class` to `structure`.

An `OperationPlan` contains one `EntityIntent` per declaration and `LinkIntent` values with
corpus-local endpoint names. Entity intents retain the declaration's `type_expr`, optional doc
and module; formulas are not separate entities. Every in-corpus reference becomes a
consumer-to-dependency `depends_on` intent with `DependencyKind::Runtime`. Repeated identical
edges are emitted once in first-occurrence order. References outside the supplied corpus are
omitted. Entity order follows input order; each declaration's reference edges precede its
optional parent-projection edge.

The v1 `extends` heuristic emits `P → H` only when all these conditions hold:

1. The declaration name is `P.to<S>` with a nonempty suffix.
2. The final codomain head uniquely resolves through the declaration's references to an
   in-corpus `H`.
3. `P` is also in the corpus.
4. `S` equals the last name component of `H`.
5. Both endpoint records have raw kind `inductive`.
6. The projection declaration has raw kind `def`.
7. Its type has an immediate `self` binder whose type head resolves to `P`.

For example, `Point3.toPoint2 : (self : Point3) → Point2` can emit
`Point3 extends Point2`. Balanced arrows and leading forall binders are handled textually;
a universe-annotated head such as `C.{v,u}` resolves as `C`. Unsupported or ambiguous syntax
produces no `extends` candidate and retains ordinary reference intents. A self-less conversion
between inductives also produces no `extends`. This bounded heuristic does not elaborate Lean;
a generated projection without the extracted `self` binder remains a v1 false negative.
Typed references are a separate v2 extension.

To consume a plan with KG, create entities using each intent's canonical `entity_type` as the
public `kind`, copy `doc` into `description`, and put `type_expr` and `module` in properties.
Resolve endpoint names through the returned create IDs, then dispatch links with the intent's
relation and explicit runtime dependency kind. Load KG and FormalPack and install the composed
registry's edge rules. FormalPack is needed for the formal `depends_on` pairs; a base
concept-to-concept `extends` edge alone does not establish that it is loaded.

The extractor preserves reference intents whose endpoint pairing the runtime forbids. Such a
link receives the runtime's normal refusal and stores no edge; it is not silently omitted or
used to widen the endpoint rules. The pure operation plan does not promise an atomic database
transaction across a caller's later create/link requests.

Production dependencies are shared types, serde, serde_json and thiserror. Runtime, KG,
FormalPack and Tokio are development dependencies for the integration tests, which inspect
stored entities and edges after real dispatch. This crate adds no pack, verb, schema, CLI,
layout, model inference or corpus producer.
