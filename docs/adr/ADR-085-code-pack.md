# ADR-085: Code Pack — Source-Code Ontology and Audit-Finding Vocabulary

**Status**: Accepted\
**Date**: 2026-07-03\
**Authors**: khive maintainers
**Depends on**: ADR-001 (Entity Kind Taxonomy — `entity_type` subtype registration), ADR-002
(Edge Ontology — closed 17 relations, base endpoint contract), ADR-013 (Note Kind Taxonomy —
pack-declared note kinds), ADR-017 (Pack Standard — `EDGE_RULES`, `EntityOfType`,
`NOTE_KINDS`), ADR-019 (GTD Pack — `NoteKindSpec` lifecycle precedent), ADR-055 (Epistemic
Edge Relations), ADR-069 (Subject Model — domain-ontology paradigm, `EntityOfType` mechanism)\
**Related**: ADR-072 (Proposed — OntologySpec as data; see D6), ADR-084 (ontology
introspection), issue #373 (verbs vs services — explicitly out of scope, see D1)

## Context

khive's research graph compounds: an agent reads a paper once, records concepts and edges, and
every later session queries the graph instead of re-reading the paper. Source code — the
corpus maintainers work in daily — has no equivalent. Every session re-derives the same
structural facts (what contains what, what depends on what, what implements which idea) by
re-reading files. The recurring defect-audit pipeline (`.khive/scripts/audit_crate.py`)
produces structured findings that live only as flat GitHub issues and local
`findings.json` files — invisible to graph queries, so "which crates have unresolved
high-severity findings" and "what is the fix-recurrence rate per category" cannot be asked
of the store.

Source code passes ADR-069's three-ingredient Subject qualification test cleanly:

1. **Declared machine-readable vocabulary** — the language's own declaration kinds (fn,
   struct, trait, mod in Rust; def, class in Python), extractable from ASTs without NL
   interpretation.
2. **Typed relations recoverable from structure** — imports, calls, type references, trait
   implementations; from compiler/AST tooling, not prose.
3. **A declared taxonomy** — module/package path hierarchy, the same namespacing mathlib's
   map was built from.

This ADR specifies the runtime vocabulary for that vertical: the **code pack**
(`khive-pack-code`) plus the concept subtypes it relies on. It is the downstream schema
surface in ADR-069's terms. The upstream code _Subject_ (Scanner over rust-analyzer /
tree-sitter, Extractor, Layout) is separate ADR-069-layer work and is not specified here.

### Source-code grounding

**The triples a code ontology needs are absent from the base endpoint contract.**
`BASE_ENTITY_ENDPOINT_RULES` (`crates/khive-runtime/src/operations.rs:290-355`, the table
`base_entity_rule_allows` consults) contains no `concept -> concept` row for `depends_on`
or `implements`, and no `project -> concept` row for `contains`. A symbol-level dependency
edge (`function depends_on function`), a type-to-trait edge (`datatype implements
interface`), and a crate-to-module containment edge (`project contains module`) are all
rejected today. This is the same gap ADR-069 documented for formal math, and the same
sanctioned fix applies: additive pack `EDGE_RULES` scoped to subtypes via
`EndpointKind::EntityOfType` (ADR-017 §"Pack-extensible edge endpoints").

**Broadening the base contract instead is prohibited by ADR-069's own reasoning.** A broad
`concept depends_on concept` base row would legalize the triple for every concept in every
deployment (ADR-069 A8). Subtype-scoped pack rules are the only additive mechanism that
does not destroy the closed contract's precision.

**Additivity is verified.** Pack rules union with the base contract — an edge is legal if
either accepts it (ADR-017; `pack_rule_allows` at `operations.rs:262` is consulted before
the base-kind fallback, with regression coverage around `operations.rs:9688-9756`). Base
`concept -> concept` rows (`contains`, `extends`, `variant_of`) continue to fire for
subtyped concepts, because the base matcher compares base kinds only.

**Subtype tokens are governed by the kg pack's `EntityTypeRegistry`.** The shipped
formal-math precedent placed its six tokens (`theorem`, `definition`, ...) in
`BUILTIN_DEFS` (`crates/khive-pack-kg/src/entity_type_registry.rs:96-125`), validated at
create time via `validate_entity_type` (`crates/khive-pack-kg/src/handlers/create.rs`),
while the endpoint rules live in the separate `khive-pack-formal` crate — pure ontology, no
verbs, `REQUIRES = ["kg"]`, self-registered via `inventory::submit!` with an anchor import
in `crates/kkernel/src/lib.rs:29`. This ADR follows that exact split.

**A pre-existing alias hazard constrains token naming.** The formal-math `structure` token
already claims aliases `"struct"` and `"class"` (`entity_type_registry.rs:109`). A code
ingester that writes `entity_type="struct"` or `"class"` will silently resolve to the
formal-math `structure` subtype. The code vocabulary therefore uses distinct canonical
tokens and never claims those aliases; ingesters must write the canonical code tokens.

**Issue #373 does not gate this ADR.** The verbs-vs-services tension applies to
capabilities that watch, stream, or execute. This pack is pure vocabulary plus one note
kind riding shared CRUD — nothing here needs a background service, a subscription, or an
execution surface. An executable code capability (run/snapshot/sandbox), if ever wanted, is
a different risk class and belongs to #373's interface-taxonomy resolution, not to this
pack.

## Decision

### D1: Scope — the code pack is a domain-ontology pack, not a capability pack

`khive-pack-code` models the structure and provenance-adjacent quality observations of
source code as typed graph vocabulary. It registers **zero verbs**. Agents use the existing
shared surface: `create` / `link` / `neighbors` / `traverse` / `query` / `search`. The pack
contributes:

- four concept subtypes for code declarations (D2, tokens in the `EntityTypeRegistry`),
- additive `EDGE_RULES` making the code triples legal (D3),
- one note kind, `finding`, for audit/defect observations (D4 — severable),
- nothing else: no schema plan, no background services, no execution surface.

Repositories and crates remain base `project` entities (optionally subtyped with the
existing `repository` / `library` / `tool` project subtypes). Binaries and build outputs
remain base `artifact` entities. No new base entity kind, no new note kind beyond
`finding`, and no new edge relation is introduced.

### D2: Four concept subtypes, registered in the `EntityTypeRegistry`

The code vertical registers four subtype tokens, all resolving to `EntityKind::Concept` —
the same base kind and mechanism as the formal-math subtypes (ADR-069 D3). They are
language-agnostic; the language is a property (`properties.language`), never a kind.

| Token       | Base kind | Aliases                        | Meaning                                                      |
| ----------- | --------- | ------------------------------ | ------------------------------------------------------------ |
| `module`    | `Concept` | `mod`, `namespace`             | A namespace/module/package-internal unit within a project    |
| `function`  | `Concept` | `fn`, `func`, `method`         | A callable: free function, method, procedure                 |
| `datatype`  | `Concept` | `enum`, `record`, `type_alias` | A data-shape declaration: struct, enum, class, record, alias |
| `interface` | `Concept` | `trait`, `protocol`            | A behavioral contract: trait, interface, protocol, typeclass |

Deliberately excluded from v1 (amend this ADR with evidence before adding): `macro`,
`constant`, `test`, `file` (files are storage layout; `module` is the semantic container),
and any commit/PR provenance subtypes (deferred — see Alternatives A7). The aliases
`struct` and `class` are NOT claimed (they belong to the formal-math `structure` token);
ingesters MUST write the canonical tokens above.

Tokens are added to `BUILTIN_DEFS` in `crates/khive-pack-kg/src/entity_type_registry.rs`,
following the formal-math precedent. Consequence inherited from that precedent: subtype
tokens validate at create time in every deployment, while the edge rules below apply only
where the code pack is loaded. This asymmetry is the current architecture's shape (formal
behaves identically); unifying token and rule loading is ADR-072-era cleanup, not this ADR.

### D3: Additive edge rules

All rules bind the base kind (`EndpointKind::EntityOfType { kind: "concept", entity_type:
... }` — never bare `EntityOfKind` for subtypes, per ADR-069's grounding and the PR #231
review lesson that subtype matching must be scoped to the registry-validated
`(EntityKind, entity_type)` pair).

**Additive rules (not legal under the base contract today):**

| #  | Relation     | Source              | Target              | Reading                               |
| -- | ------------ | ------------------- | ------------------- | ------------------------------------- |
| 1  | `depends_on` | concept/`function`  | concept/`function`  | call / use                            |
| 2  | `depends_on` | concept/`function`  | concept/`datatype`  | uses type                             |
| 3  | `depends_on` | concept/`function`  | concept/`interface` | bound / dyn use                       |
| 4  | `depends_on` | concept/`datatype`  | concept/`datatype`  | field / composition                   |
| 5  | `depends_on` | concept/`datatype`  | concept/`interface` | bound                                 |
| 6  | `depends_on` | concept/`interface` | concept/`interface` | supertrait / bound                    |
| 7  | `depends_on` | concept/`interface` | concept/`datatype`  | signature type reference              |
| 8  | `depends_on` | concept/`module`    | concept/`module`    | import                                |
| 9  | `contains`   | project (base kind) | concept/`module`    | crate/repo contains module            |
| 10 | `contains`   | project (base kind) | concept/`function`  | flat project contains declaration     |
| 11 | `contains`   | project (base kind) | concept/`datatype`  | flat project contains declaration     |
| 12 | `contains`   | project (base kind) | concept/`interface` | flat project contains declaration     |
| 13 | `implements` | concept/`datatype`  | concept/`interface` | impl Trait for Type                   |
| 14 | `implements` | concept/`function`  | concept (any)       | function implements an algorithm/idea |
| 15 | `implements` | concept/`datatype`  | concept (any)       | type implements a design/idea         |
| 16 | `implements` | concept/`module`    | concept (any)       | module implements a design/idea       |

Rules 14-16 are the **research bridge**: they connect code entities to the untyped
research-KG concepts (algorithms, techniques, design patterns) that the rest of the graph
already speaks. Their target is `EntityOfKind("concept")`, which also matches subtyped
concepts; that mild over-breadth is accepted and documented — `EndpointKind` has no
negative matcher, and inventing one is a mechanism change out of this ADR's additive scope.

**Base-covered rules, declared for subtype-granular ontology documentation** (the base
`concept -> concept` rows at `operations.rs:292,303` already legalize these; declaring them
keeps the pack's `EDGE_RULES` a complete, introspectable statement of the code ontology —
the same practice `khive-pack-formal` follows for its `extends`/`variant_of` rules, and a
defensive guarantee if subtype-row matching policy ever tightens):

| #     | Relation   | Source              | Target                                                |
| ----- | ---------- | ------------------- | ----------------------------------------------------- |
| 17-20 | `contains` | concept/`module`    | concept/`module`, `function`, `datatype`, `interface` |
| 21    | `extends`  | concept/`interface` | concept/`interface` (inheritance)                     |
| 22    | `extends`  | concept/`datatype`  | concept/`datatype` (inheritance)                      |

**Relied-on base triples (no declaration needed, listed for implementers):**
`* instance_of concept` (a function is an instance_of an algorithm), `concept variant_of
concept` (ports/forks), `concept supersedes concept` (renamed/replaced declarations),
`project depends_on project` (crate dependencies), `project implements concept`,
`concept introduced_by document|person`, and the epistemic rails
`{concept,document,dataset,artifact} supports|refutes concept`.

`depends_on` metadata guidance: code-declaration references default to
`dependency_kind="build"` (a compile-time requirement); use `"runtime"` for dynamic/plugin
references. Set by the caller — the base inference table is not modified.

### D4: The `finding` note kind (severable — see Open Questions)

The audit lane rides one pack-declared note kind:

- `NOTE_KINDS = ["finding"]`, alias `defect`. A finding is an epistemic observation about a
  code entity, not an entity itself: numerous, time-bound, evidence-bearing.
- **Attachment needs zero edge rules**: `annotates` (note -> any entity) is the base
  cross-substrate rail; a finding annotates the `project` (crate) or code-subtype entity it
  concerns. `supports`/`refutes` note->note and `supersedes` note->note are likewise
  already legal for linking findings to decision notes or to superseding findings from a
  later sweep.
- **`NoteKindSpec` lifecycle** (declared for introspection now, enforced when the generic
  Phase-2 lifecycle layer lands — same posture as gtd's `task`): field `kind_status`,
  initial `open`, terminals `resolved`, `wontfix`, `invalid`; transitions `open ->` each
  terminal.
- **Properties contract** (governed values validated by a `prepare_create` `KindHook`, the
  pack's only code beyond declarations): `severity` in `{critical, high, medium, low,
  info}`; `confidence` in `{high, medium, low}`; free-form: `categories` (array),
  `source_run` (e.g. `audit-20260702`), `standard`, `evidence` (array), `refs` (object —
  `github_issue`, `pr`, `commit` as plain references; commits/PRs are properties, not
  entities, in v1). The hook defaults `kind_status="open"` and rejects unknown `severity`
  / `confidence` values (fail closed; no silent coercion).
- **No verbs.** Findings are created via `create(kind="finding", ...)` on shared CRUD;
  status changes via `update`. If usage proves a validated-transition verb is needed, that
  is a one-verb amendment with gtd's `transition` as the template.

This makes the existing audit pipeline's harvest step able to mirror `findings.json`
records into the graph (severity/confidence/categories map directly), turning audit history
into a queryable, compounding corpus instead of a per-sweep flat file.

### D5: Pack mechanics

Following `khive-pack-formal`'s shipped shape (`crates/khive-pack-formal/src/pack.rs`):

- Crate `crates/khive-pack-code`, `NAME = "code"`, `REQUIRES = ["kg"]`,
  `ENTITY_KINDS = []` (tokens live in the registry, per D2), `HANDLERS = []`,
  `NOTE_KINDS = ["finding"]`, `NOTE_KIND_SPECS` = the D4 spec, `EDGE_RULES` = the D3
  table, `SCHEMA_PLAN = None`.
- Self-registration via `inventory::submit!` (`PackFactory`) plus the anchor import
  `use khive_pack_code::CodePack as _;` in `crates/kkernel/src/lib.rs` — the live ADR-023
  pattern, superseding ADR-017's match-arm text.
- **Not in the default pack set** at v1. Loaded opt-in via `KHIVE_PACKS=...,code` /
  `--pack code`. Promotion to the default set is a follow-up decision gated on one
  validated real ingest (an audit-harvest mirror plus a hand-curated crate/module/symbol
  slice, with `neighbors`/`traverse`/`query` answering real questions).

### D6: Hard constraints

1. **Granularity fence.** The shared production graph (`khive.db`) receives _incremental,
   curated_ code entities — crates, modules, and the specific declarations agents actually
   reference in their work — plus findings. Exhaustive whole-repo symbol/call graphs
   (Subject-scale batch ingests; mathlib-scale is 10^5 entities / 10^6 edges) target
   **dedicated map databases** via the direct-build path, never `khive.db`. This mirrors
   ADR-069's hard constraint verbatim; it is a process constraint enforced operationally.
2. **Transcribe, do not invent** (ADR-069 D5). Entity names are the declared symbol names;
   descriptions are doc-comments as-is or omitted; no synthesized descriptions.
3. **Registry-valid tokens only.** Ingesters write canonical subtype tokens through the
   validated create path (or pre-validated bulk import). Rules match
   `(kind="concept", entity_type)`; unvalidated subtype strings on imported rows must not
   be relied on to fire rules (PR #231 lesson).
4. **No secrets.** Code snippets and finding evidence pass the runtime secret gate like all
   writes; ingesters must not embed credential material in content or properties.
5. **ADR-072 forward compatibility.** The D2 tokens and D3 rules are pure data and are
   authored transcription-ready: if ADR-072 is ratified and its ontology loader ships, the
   entity vocabulary migrates verbatim to a code OntologySpec, and `khive-pack-code`
   either shrinks to the `finding` note kind + hook (note kinds are outside ADR-072 D1's
   OntologySpec scope) or retires entirely if that scope is amended. Nothing in this ADR
   may make that transcription harder (no logic entangled with the vocabulary).

## Rationale

### Why a domain-ontology pack (and not an execution surface)

The pack roster names domains and capability surfaces that khive already owns; the closest
analogue to "code" is `formal` — a domain vocabulary pack. Code passes the ADR-069 Subject
test on all three ingredients, and the two concrete, recurring pains (comprehension
re-work; audit findings invisible to queries) are both graph-vocabulary problems. An
execution capability would collide head-on with the unresolved #373 interface taxonomy and
carry a sandboxing risk class nothing in the one-line ask implies. Vocabulary now is useful
now; execution later remains possible under #373's resolution without this ADR changing.

### Why a new pack crate at all (the null hypothesis, refuted specifically)

The null — "extend an existing pack, create nothing" — fails on placement, in two halves:

- The **subtype tokens** genuinely do go into an existing pack (`khive-pack-kg`'s
  `EntityTypeRegistry`) — a Modify, not a Create. This ADR takes that path.
- The **edge rules** cannot: placing them in `khive-pack-kg` makes them unconditional in
  every deployment (kg is always loaded), which is a de-facto base-contract broadening —
  exactly what ADR-069 A8 rejects and what operator opt-in exists to prevent. Placing them
  in `khive-pack-formal` couples two unrelated domain ontologies and entangles code with a
  crate ADR-072 already slates for retirement. A rules-carrying pack crate is the only
  additive, opt-in, sanctioned container — and at pack-formal's demonstrated cost (~300
  LOC of const data plus one anchor import), the ongoing maintenance surface is minimal.
- The **`finding` note kind** additionally requires pack machinery (`NOTE_KINDS`,
  `NoteKindSpec`, `KindHook`) that no data-spec mechanism, shipped or proposed, can carry.

### Why `concept` subtypes for declarations

The formal-math precedent is directly on point: Lean declarations ARE source-code
declarations, and ADR-069 D3 already settled that declaration-level code units are
`Concept` subtypes, with A3/A6 rejecting enum extension and A8 rejecting coarse `concept`.
`artifact` is wrong (artifacts are produced binaries/checkpoints, not authored
declarations); `project` is wrong (projects are codebases, and the subtype rules would then
collide with real project-to-project semantics). Query precision (`entity_type="function"`
as a first-class filter) is the same motivation ADR-069 records.

### Why `finding` is a note, not an entity

Findings are epistemic observations _about_ entities: numerous, time-bound, resolution-
bearing, evidence-carrying. That is the note substrate's definition, and `annotates` is the
purpose-built note->entity rail — zero new endpoint rules needed. An entity modeling would
bloat the entity space with thousands of records that have no independent identity beyond
the thing they annotate, and would need new endpoint rules for every attachment.

### Why ship as a crate despite ADR-072's direction

ADR-072 (verbless vocabulary should be a runtime-loaded OntologySpec) is Proposed, not
ratified, and its loader does not exist in `khive-runtime` today (verified by search). A
data-spec-only design would block a present need on unbuilt, unratified infrastructure.
The crate path ships this week under accepted mechanism (ADR-017 + the shipped ADR-069
`EntityOfType` machinery), and D6.5 guarantees the vocabulary transcribes to data verbatim
if/when ADR-072 lands — the same retirement path ADR-072 D4 already defines for
pack-formal. The `finding` note kind keeps a residual pack justified under ADR-072's own
"behavior is a Pack, pure vocabulary is data" split, unless ADR-072's scope is amended.

## Alternatives Considered

**A1: No new pack — subtype tokens plus edge rules all into `khive-pack-kg`.** Rejected.
Tokens yes (that half is adopted); rules no — kg is loaded in every deployment, so its
`EDGE_RULES` are effectively base contract. The code triples would become legal everywhere,
unconditionally, losing operator opt-in and broadening the closed contract by the back
door (ADR-069 A8 reasoning).

**A2: Extend `khive-pack-formal` into a general "technical declarations" pack.** Rejected.
Different domains, different Subjects, different lifecycles (ADR-072 D5 splits even
territory from proof-frontier within formal math). Coupling code to a crate already slated
for ADR-072 retirement entangles both migrations.

**A3: Wait for ADR-072 and ship the code ontology as an OntologySpec data file.** Rejected
for v1. The loader is unimplemented and the ADR unratified; this path converts a one-week
vocabulary addition into a runtime-infrastructure project. Mitigation adopted instead:
transcription-ready authoring plus an explicit migration clause (D6.5).

**A4: Model findings as plain `observation` notes — no `finding` kind.** Rejected as the
primary design, retained as the documented fallback if the audit lane is descoped. Plain
observations lose the kind-scoped filter (`list`/`search kind="finding"`), governed
severity/confidence values, lifecycle introspection, and the create-time validation hook —
the exact gaps between "notes with a tag convention" and a governed vocabulary.

**A5: Model findings as entities.** Rejected. Findings have no identity independent of the
entity they annotate; the note substrate plus `annotates` is purpose-built for this, needs
zero new rules, and keeps the entity space for durable named things.

**A6: An executable code surface (`code.exec`, `code.snapshot`, sandboxed evaluation).**
Rejected for this ADR. It is a capability, not vocabulary; it lands in issue #373's
unresolved verbs-vs-services taxonomy; and execution is a materially different security
risk class requiring its own design (sandboxing, resource limits, injection surface). If
wanted, it is a separate ADR gated on #373 — this pack neither needs it nor blocks it.

**A7: Commit/PR provenance entities in v1.** Deferred. Entity-per-commit is graph bloat at
exactly the granularity the D6.1 fence exists to prevent; findings carry `refs`
(pr/commit/issue) as properties, which serves the audit lane's resolution-tracking need.
If a real consumer needs commit-graph traversal, that is a v2 amendment with `artifact`
subtypes and `introduced_by`/`derived_from` rules.

**A8: File-level entities.** Rejected. Files are storage layout; `module` is the semantic
container and the taxonomy carrier (module paths drive ADR-069 Layout). File entities churn
on refactors without adding query value.

**A9: Language-specific subtype sets (`rust_fn`, `py_class`, ...).** Rejected. The
ontology must be farmable across languages (one vocabulary, many Scanners — ADR-069 D1);
language is a property. Four language-agnostic tokens cover the declaration kinds that
carry cross-language semantics.

## Consequences

### Positive

- Codebase comprehension compounds: structural facts extracted once become
  `neighbors`/`traverse`/`query`-able for every later session, joining the research graph
  through the `implements` bridge (code -> algorithm/technique concepts).
- Audit history becomes a queryable corpus: unresolved-by-severity-by-crate, recurrence per
  category, findings superseded across sweeps — all expressible as graph queries; the
  harvest step gains a graph mirror with a direct field mapping.
- Zero new verbs, zero new relations, zero base-contract changes: the entire surface is
  additive const data plus one note kind, at pack-formal's demonstrated cost.
- The vocabulary is Subject-ready: a future code Subject (Scanner/Extractor/Layout per
  ADR-069) targets these tokens and rules without re-design.

### Negative

- One more pack crate to maintain, and one more entry ADR-072's eventual migration must
  transcribe (mitigated: pure-data authoring, D6.5).
- The token/rule loading asymmetry (tokens validate everywhere; rules only where the pack
  loads) is inherited from the formal precedent and slightly widens the globally-valid
  subtype vocabulary even for deployments that never load the pack.
- Prod-graph bloat is possible if the D6.1 granularity fence is ignored; the fence is
  operational, not mechanical — same enforcement posture (and residual risk) as ADR-069's
  database constraint.
- The `struct`/`class` alias capture by formal-math `structure` remains a live ingestion
  trap; documented here, resolvable only by a future alias re-audit (out of additive
  scope).

### Neutral

- Existing prod entities that model modules as `project` (the `{crate}[-{module}]` naming
  convention) stay valid — data-vs-view; no migration is mandated. New curation should
  prefer the subtyped forms.
- `finding` lifecycle enforcement waits on the generic NoteKindSpec Phase-2 layer, exactly
  as gtd's `task` does today; until then `kind_status` lives in properties with
  hook-defaulted initial state.

## Open Questions

1. Is the audit-finding lane (D4) in scope for v1, or should this ADR ship code-ontology-only
   (D1-D3, D5, D6) and defer `finding` to a follow-up amendment? Recommendation:
   include it — it rides shared CRUD at near-zero marginal mechanism cost and immediately
   makes the existing `audit_crate.py` harvest step's output graph-queryable.
2. Default-pack-set promotion timing (D5's opt-in-at-v1 posture) — confirm gated on one
   validated real ingest, or set an explicit earlier promotion trigger.
3. Confirm this ADR's reading of the original one-line ask ("we need a pack-code") as
   domain-ontology scope (Fork A: code-as-graph vocabulary) rather than an execution surface
   (Fork C, rejected here as a different risk class colliding with issue #373).

## Implementation

This ADR authorizes design, not code; implementation follows design review approval.

1. `crates/khive-pack-kg/src/entity_type_registry.rs` — add the four `EntityTypeDef`
   entries (D2 tokens + aliases) to `BUILTIN_DEFS` under a `// ── Code ──` section.
2. `crates/khive-pack-code/` — new crate mirroring `khive-pack-formal`'s structure:
   `vocab.rs` (the 22 `EdgeEndpointRule` consts + `NoteKindSpec` for `finding`), `hook.rs`
   (the `finding` `prepare_create` hook: default `kind_status`, validate
   severity/confidence), `pack.rs` (`Pack` + `PackRuntime` + `PackFactory` +
   `inventory::submit!`), `lib.rs` (pure re-exports).
3. `crates/kkernel/src/lib.rs` — one anchor import: `use khive_pack_code::CodePack as _;`.
4. Tests: rule-presence tests (formal's pattern); an integration test proving (a) each
   additive triple links successfully with the pack loaded and is rejected without it,
   (b) base-covered triples remain legal in both configurations (additivity regression),
   (c) `create(kind="finding")` defaults `kind_status=open` and rejects an invalid
   `severity`.
5. Docs: README pack table row; AGENTS.md note-kind/subtype listing (per the
   surface-contract amendment lesson — the wire-visible vocabulary changes, so consumer
   docs are part of the PR).
6. Verify by: `make ci` green; the integration tests above; one manual end-to-end on a
   scratch DB — create a crate `project`, a `module`, two `function`s, link
   `contains`/`depends_on`/`implements`, create a `finding` annotating the crate, and
   confirm `traverse` + `query` answer "what does f depend on" and "open high-severity
   findings for crate X".

## References

- ADR-001: Entity Kind Taxonomy — pack extensibility rule; governed `entity_type`
- ADR-002: Edge Ontology — closed 17 relations; base endpoint contract
- ADR-013: Note Kind Taxonomy — pack-declared note kinds
- ADR-017: Pack Standard — `EDGE_RULES`, `EntityOfType`, additive-only endpoints
- ADR-019: GTD Pack — `NoteKindSpec` lifecycle precedent (`task`)
- ADR-055: Epistemic Edge Relations — `supports`/`refutes` rails findings reuse
- ADR-069: Subject Model — the domain-ontology paradigm; `EntityOfType`; hard constraints
  mirrored in D6
- ADR-072 (Proposed): OntologySpec as data — the D6.5 migration target
- ADR-084 (Proposed): ontology introspection — why declared rules double as documentation
- Issue #373: verbs vs resources vs subscriptions vs services — the boundary D1 respects
- `crates/khive-runtime/src/operations.rs:290-355` — `BASE_ENTITY_ENDPOINT_RULES` (the
  verified gaps: no `concept->concept` `depends_on`/`implements`, no `project->concept`
  `contains`)
- `crates/khive-pack-kg/src/entity_type_registry.rs` — token governance; the
  `struct`/`class` alias hazard (line 109)
- `crates/khive-pack-formal/src/{vocab.rs,pack.rs}` — the reference implementation shape
- Existing audit harvest script and findings JSON — the audit lane's record shape mapped by D4

---

## Amendment 1 (2026-07-07) — v0 implementation record

The v0 implementation (`crates/khive-pack-code`) surfaced three decisions the base
text left open. Resolved as follows; all three are normative.

### A1 — Ingest posture for free-form fields: tolerate

The fail-closed validation set is exactly the governed contract above:
`severity` and `confidence` values, evidence shape, and `failure_scenario` presence.
Fields the base text lists as free-form (`categories`, `standard`, `evidence`,
`source_run`, `refs`) and fields it does not govern at all (`priority`, `status`,
`impact`, `recommendation`, `verification`) are tolerated when absent or extra:
ingest neither rejects nor coerces them. A producer that wants stricter
required-field guarantees must bring that as a future amendment with its own
justification; it does not arrive as unilateral ingest hardening.

### A2 — `finding.status` to `kind_status` mapping is pack-owned

The pack owns the `finding` kind and its `kind_status` lifecycle, so normalization
of producer status vocabulary is ontology, not a consumer concern. v0 behavior:
`kind_status` defaults to `open` and the raw producer value is preserved under
`properties.audit_status`. The governed mapping (`fixed -> resolved`,
`false_positive -> invalid`) lands pack-side as a v0.1 change; consumers must not
implement their own mapping.

### A3 — Ingest path: internal mapper, no wire verb

Shared `create` assigns UUIDv4 per call and is therefore not idempotent for
re-ingested audit runs. v0 ships a pure internal mapper, `ingest_findings_json`
(`findings.json` to entities/notes/edges), honoring the "no verbs" decision: no
wire verb is added. Identity is content-derived UUIDv5 over the record's key
fields; `observed_at` is excluded from every key tuple, so re-ingesting the same
findings file — or the same findings observed at a different time — is a no-op.
Whole-document validation runs before any record construction (all-or-nothing).

### Colocated producer contract

The `findings.json` schema and producer contract are to be committed in-repo so the
ingest consumer and its input contract live in one place. Producer tooling itself
is out of scope for this repository.

## Amendment 2 (2026-07-09): code.ingest verb + acceptance

**Status (as of PR #1039, 2026-07-15): accepted and shipped — L1 (manifest
edges) + L1.5 (import-scan edges) only.** The `code.ingest` verb has a
handler in `crates/khive-pack-code` and is live on the default MCP surface,
which now reports 79 verbs. Amendment 3 below documented the interim
zero-verb production surface (2026-07-11 through PR #1039) — that window has
closed; the pack now contributes one verb, `code.ingest`, in addition to the
`finding` note kind. L2 (the full Scanner/Extractor pipeline over the D2-D3
vocabulary at declaration granularity) remains unimplemented and out of
scope for PR #1039; the design and acceptance recorded in the rest of this
section remain the plan for that future work.

The base text left the Scanner/Extractor pipeline over the D2-D3 vocabulary as
"separate ADR-069-layer work" and explicitly out of scope. That pipeline now has a
design. This amendment specifies it as a single new verb, `code.ingest`, and closes
the ADR to Accepted.

### B1: One new verb, `code.ingest(path, db?, languages?)`

The pack gains exactly one verb, following the precedent set by the git pack's
single `git.digest` verb for a comparable bulk-intake surface. Signature:
`code.ingest(path, db?, languages?=auto)`.

- `path` is a folder, not necessarily a repository root. Monorepo subtree ingest
  (a single crate, a single package directory) is first-class, not a special case
  of whole-repo ingest.
- `languages` defaults to automatic detection from manifest files
  (`Cargo.toml`, `pyproject.toml`, `package.json`, Lean project files) and file
  extensions under `path`; callers may pass an explicit language list to skip
  detection or restrict scope.
- `db` targets the destination database (see B7); it defaults to a workspace map
  database, not the shared production graph.

### B2: Pipeline shape

The pipeline follows the Scanner/Extractor split from ADR-069: one Scanner per
source language performs syntax-level parsing only, and a single Extractor,
shared across all languages, maps Scanner output into this ADR's D2 subtypes and
D3 edge rules. The Extractor is ontology-driven and source-blind: it has no
per-language branching, only per-Scanner-output-shape adapters feeding one
mapping table.

Scanners are syntax-only in v1: no type-checking and no compilation. A
declaration's doc comment is transcribed verbatim into its `description`
property when present (ADR-069 D5, "transcribe, do not invent"); nothing is
synthesized. Scanner sequencing, in delivery order: Rust (`syn`), Python
(`rustpython-parser`), TypeScript (`oxc`), then Lean. The Lean scanner performs
statement-text structural parsing; `structure`-level `extends` relationships
require environment metadata that a syntax-only parse does not have, so that
edge kind is an explicit boundary deferred past v1, consistent with this ADR's
D3 finding that some relations need more than AST structure to resolve.

### B3: Tiers

Ingest proceeds in three tiers of increasing cost and completeness, each usable
independently of the ones after it:

- **L1 (manifest edges).** Pure manifest parsing (`Cargo.toml`, `pyproject.toml`,
  `package.json`) yields `project depends_on project` edges with
  `dependency_kind`, using the base endpoint contract's already-legal triple.
  Requires no Scanner and covers every language with a package manifest.
- **L1.5 (import-scan edges).** A regex-based import scan produces
  module-to-module and project-to-project `depends_on` edges. This is the
  coverage floor for a language that has no Scanner yet, and doubles as the
  signal for which language to build a Scanner for next.
- **L2 (symbol tier).** The full Scanner/Extractor pipeline (B2) over the D2
  subtypes and D3 edge rules, at declaration granularity.

### B4: Identity and idempotency

Symbol identity is `uuid5` over
`(source_project, language, module_path, name, kind)`, where `language` is the
detected source language of the declaring file and `kind` is one of the four
canonical D2 tokens (never an alias). `language` is part of the identity tuple
because module paths are language-native rather than globally disjoint: a
polyglot or manifestless project can hold same-named declarations in two
languages whose native module paths coincide (single-segment paths
especially), and without the language component those declarations would
collapse to one entity, leaving B5 unable to attribute that entity to a
single `(source_project, language)` sweep clock. A
secondary `content_hash` property (a hash of the declaration body) detects
changed-versus-unchanged content independently of identity. Because identity is
derived from these fields rather than assigned per call, re-ingesting the same
path is idempotent: the same declarations produce the same entity and edge
identities, and repeated ingests do not accumulate duplicate rows. Ingesters
write the canonical D2 tokens only; the alias-capture hazard documented in D2
applies identically here.

Canonicalization of the two identity inputs is fixed, not left to per-caller
convention:

- `source_project` resolves per source file, not per ingested path: each
  file's `source_project` is the package name declared by the nearest
  governing manifest at or above that file (`Cargo.toml` `[package].name`,
  `pyproject.toml` `[project].name`, `package.json` `name`, the Lean project
  name for a Lean project file). A virtual or workspace-only manifest that
  declares no package name, such as a `Cargo.toml` with `[workspace]` but no
  `[package]`, or a `pyproject.toml` with no `[project]` table, is not
  governing and is skipped upward in favor of the next manifest above it. The
  basename fallback applies only when no governing manifest exists anywhere
  above a file, in which case that file's `source_project` falls back to the
  basename of the ingested folder. Consequently, ingesting a multi-package
  repository root naturally yields multiple per-package `source_project`
  values in one ingest run; there is no reject rule for multi-package roots.
- `module_path` is the language's own canonical module path, relative to the
  `source_project` root determined above: a Rust crate-relative `::` path, a
  Python dotted module path, a TypeScript path relative to the package root
  with the file extension stripped, and a Lean namespace path.
- The `uuid5` namespace seed is a fixed, pack-level constant, following the
  same pattern already used by the pack's existing findings-ingest identity
  namespace.

### B5: Staleness

Ingest performs no automatic deletion. Every entity present in a sweep has its
`properties.last_seen_at` stamped to that sweep's time, recording when it was
last observed. An entity absent from a sweep is left untouched: its
`last_seen_at` keeps the value from its last observed sweep. Sweep
timestamps are recorded per `(source_project, language)` pair. Staleness
filtering compares an entity's `last_seen_at` against the latest sweep time
of its own `(source_project, language)` pair, never against sweeps of other
projects or languages ingested into the same namespace; whether a query
surfaces an entity below that threshold is a view-layer filtering decision,
not a data-layer one (khive's data-versus-view principle: showing only
current state is always a query concern, never a reason to delete, mutate,
or transfer stored data).

### B6: Cross-repo resolution

An import specifier that names a project not yet ingested (a dependency on a
crate or package that has not itself been scanned) does not fail the ingest.
The specifier is recorded on the source entity as an unresolved reference, and
within-`source_project` edges land normally. Resolution runs as a re-resolve
pass: after any `source_project` is ingested, previously recorded unresolved
specifiers across the target database are replayed against the now-known
symbol keys. In v1 this pass runs synchronously as part of the same
`code.ingest` call and completes before a successful return; there is no
pending-resolution state exposed to callers in v1. A deferred-edge queue that
replays only the pending references relevant to a specific just-ingested
`source_project` is a documented, more scalable v2 alternative once the number
of interlinked projects grows large enough that repeated full-database replay
becomes costly.

Once materialized, a cross-project edge is an ordinary edge: source provenance
is carried as a `properties.source` value on the entity, not as a namespace
distinction, so cross-project and within-project edges share the same relation
semantics and query surface.

### B7: Target database posture

This amendment restates and does not relax the D6.1 granularity fence.
`code.ingest`'s `db` parameter selects among dedicated map databases only; the
verb rejects the shared production database as a target for an exhaustive L2
ingest. Exhaustive, whole-project symbol and call graphs are large by
construction (comparable in scale to other Subject-scale ingests this codebase
already supports) and target dedicated map databases via the existing
direct-build path, never the shared production graph, with no override
available on the ingest verb itself. Promoting a curated slice, such as the
symbols and modules an agent's work has actually referenced, into the shared
production database is a separate, explicit curation or import path, distinct
from and never performed by `code.ingest`.

### B8: Acceptance

An implementation of this amendment is acceptance-tested against three
properties, all expressible as ordinary queries against the shared query
surface (`neighbors` / `traverse` / `query`) with no additional tooling. The
acceptance fixture supplies the traversal bound (`max_depth`) and the expected
result for that bound; `max_depth=3` is the reference value used unless a
fixture states otherwise.

1. **Codeflow parity**, per ingested `source_project`:
   - blast radius: a reverse `depends_on` traversal from a named symbol,
     bounded by the fixture's `max_depth`, returns its callers;
   - circular dependencies: a self-returning `depends_on` pattern at module
     level, bounded by the fixture's `max_depth`, is detectable;
   - dead symbols: the set of functions, datatypes, and interfaces with zero
     incoming `depends_on` edges is listable (scoped to callers present in the
     ingested set).
2. **Cross-project order independence**: ingesting two related
   `source_project`s in either order converges to the identical final edge set
   once both ingests and their synchronous re-resolve passes have completed.
3. **Cross-language identity disjointness**: a fixture containing same-named
   declarations of the same `kind` in two languages, placed so their
   language-native module paths coincide, produces two distinct entities, and
   a subsequent single-language sweep advances only that language's
   `(source_project, language)` sweep clock, leaving the other language's
   entity and staleness threshold untouched.

### Explicitly deferred (unchanged from the base text's posture)

Rename detection, the deferred-edge queue (B6's v2 alternative), Lean
`structure`-level `extends` resolution, additional D2 subtypes beyond the four
shipped, commit/PR entities, and any type-checked or semantic (as opposed to
syntactic) extraction remain out of scope for this amendment, consistent with
D6 and the base text's Alternatives Considered.

## Amendment 3 (2026-07-11): admin ingest path + default load

Prior to this amendment, `ingest_findings_json` (Amendment 1 A3) was reachable
only by linking `khive-pack-code` into a caller's own binary, and the `code`
pack was not part of khive-mcp's default pack set, so the `finding` note kind
existed in source but was not live on the production surface. This gap was
identified while specifying a durable audit service (the "staged 3-pass crate
audits" work) that needs `findings.json` sweeps to land as queryable graph
records, not only as flat local files. This amendment closes that gap.

Distinct from Amendment 2's `code.ingest` verb (an unimplemented, larger
Scanner/Extractor design targeting dedicated map databases with symbol/call
graphs, B1-B8 above): this amendment covers only the existing
`ingest_findings_json` mapper and how it reaches storage. The two share the
"ingest" word but are otherwise unrelated in scope, surface, and target
database.

### C1: The `finding` note kind is now live on the production surface

This is a deliberate change, not an incidental side effect. `code` joins the
default pack set khive-mcp and `kkernel` load when no `--pack`/`KHIVE_PACKS`
override is given. Every default-configuration server and admin invocation
from this point on validates and stores `finding` notes and the pack's 22
additive `EDGE_RULES`; a caller no longer has to opt in with an explicit
`--pack code` to make audit findings queryable. At the time of this
amendment the pack contributed zero MCP verbs and zero new entity kinds;
only its note kind, edge rules, and entity-subtype registrations were
reachable by default. (This was a statement of current fact, not a standing
invariant: Amendment 2's `code.ingest` source-ingest verb shipped in PR
#1039, so the pack now contributes one verb — see Amendment 2's updated
status. The no-verb statements in this amendment were scoped to the
findings surface and predate that verb landing.)

### C2: Ingestion is an admin/runner-side path, not a verb

For the findings surface, D1's no-verb ruling stands unmodified: findings
ingestion is not, and does not become, a verb. (Amendment 2's accepted
`code.ingest` source-ingest verb is untouched by this; it targets dedicated
map databases and has nothing to do with `findings.json`.)
`ingest_findings_json` is exposed to
operators through a new `kkernel code-ingest <findings.json>` admin CLI
subcommand (`crates/kkernel/src/code_ingest.rs`), following the same shape as
`kkernel git-ingest`: it builds a runtime directly from the configured pack
set, validates the whole document before any write (Amendment 1 A3's
fail-closed, all-or-nothing contract, unchanged), and persists the resulting
entity/note/edge batch by content-derived id: a record whose id already
exists is reported as skipped, never overwritten, so re-running the same
sweep is a no-op and a `finding`'s curated lifecycle state (`kind_status`) is
never reset by re-ingestion. `--dry-run` runs the same validation and
existence checks and reports what would happen without writing.

No MCP verb calls this path, and none is added. Agents that participate in
an audit never hold a bulk-ingest verb; only the CLI, run by the audit
service (or an operator), writes findings into the graph. This is the
runner-writes rule: an agent's contract is to produce a validated
`findings.json`, not to write graph records itself.

### C3: Consuming service context

The immediate consumer is a staged, 3-pass crate audit service: pass 1
(logic/docs), pass 2 (architecture), and pass 3 (correctness/optimization)
run per crate or per dependency-layer bundle, each pass sequenced so later
passes see earlier passes' findings as context. Each pass's agent output is
a `findings.json` sweep on disk, validated exactly as it always has been;
the audit service then invokes `kkernel code-ingest` once per validated
sweep so the run's findings become queryable graph records, serving as
pass-context for that audit cycle and prior-findings context for the next
one. Filing
policy for GitHub issues is unchanged and orthogonal to this amendment: it
still runs after pass 3 against the verify-dedupe-rank policy, never a bulk
file-everything pass, and is not affected by whether a finding has also been
ingested into khive.

## Amendment 4 (2026-07-17): analysis verbs over the map database

D1 declared the code pack "verbless-by-design, domain-ontology only," and
Amendment 2 added exactly one verb, `code.ingest`, without changing that
posture: ingest writes a map database, it does not read one back. Amendment
2's L1 and L1.5 tiers shipped in PR #1039; L2 (declaration-granularity
Scanner/Extractor ingest) remains unimplemented. Across all three tiers,
nothing in the pack analyzes the structure it has ingested — the original
one-line ask that opened this ADR's Context ("we need a pack-code") included
analysis, and the pack has shipped none. This amendment adds the pack's
first analysis verbs: three read-only operations over the same map databases
`code.ingest` writes.

### E1: Scope — D1's verbless-by-design statement is superseded for reads only

This amendment supersedes D1's "registers zero verbs" scope statement for
read verbs. The write surface is unchanged: `code.ingest` remains the pack's
only mutating verb, and no new entity kind, note kind, or edge relation is
introduced here. The three verbs below are pure readers: each opens a map
database, computes over its stored entities and edges, and returns a
result. None of them writes to any database, production or map.

All three verbs additionally observe soft-delete state. Every query
underlying `code.coupling`, `code.health`, and `code.cycles` participates
only rows with `deleted_at IS NULL` — entities, `depends_on` edges, and
`contains` (containment) edges alike. This is khive's ordinary soft-delete
convention (a deleted row stays in storage; every reader filters it out at
query time), restated here because these three verbs compute aggregates and
graph structure rather than returning individual rows, and an aggregate has
no obvious place to apply a filter unless every underlying query does it
consistently. A soft-deleted module, or a soft-deleted edge between two
otherwise-live modules, contributes to no coupling number, no dead-module
count, no aggregation, and no cyclic component.

### E2: `code.coupling(db, level?, top_n?)`

Fan-in/fan-out over `depends_on` edges, per module by default
(`level="module"`), or per project when `level="project"` — aggregating the
same edges up to the `project contains module` containment rule (D3 #9).
`level` is a closed enum, `module` or `project`; see E6 for why
declaration-level coupling is out of scope for this amendment. `db` is
required (E7) — there is no `path`-derived default for analysis verbs the
way `code.ingest` has one for `path` itself (B1).

Each result row carries the entity id, its name, `fan_in` (incoming
`depends_on` edge count), and `fan_out` (outgoing `depends_on` edge count).
Rows are ordered by total degree (fan_in + fan_out) descending, then by
entity id ascending as a tiebreaker — a total order, so two identical calls
return rows in the same sequence (E8). The result is bounded by `top_n`
(default 50, max 500) rather than offset-paginated: degree ranking cannot be
sliced without first aggregating every row's total degree, so an
offset-based page over the map-database scale this pack targets would redo
that full aggregation on every page for no benefit over asking for a larger
`top_n` once. There is no `offset` parameter in v1. When fewer than `top_n`
rows exist — including an empty or newly-ingested database with no modules
at all — `code.coupling` returns every available row, or an empty array; it
never invents rows to reach `top_n` and never errors for having fewer rows
than requested. A materialized-cursor contract — one that lets a caller
resume a coupling listing without re-aggregating from scratch — is
deferred until a consumer demonstrates an actual need for it; nothing here
forecloses adding one later.

At `level="project"`, `fan_out` counts the number of _distinct_ neighbor
projects this project depends on, and `fan_in` counts the number of
distinct neighbor projects that depend on it — not edge counts. A
dependency from project A to project B counts as one neighbor relationship
if there is a direct project-to-project `depends_on` edge from A to B, or
at least one module-to-module `depends_on` edge whose source module
belongs (via `contains`) to A and whose target module belongs to B; A and
B count as neighbors once regardless of how many edges — direct or
module-mediated — witness the relationship.

Intra-project module dependencies do not contribute to project-level
`fan_in`/`fan_out`. A `depends_on` edge whose source and target modules
both belong (via `contains`) to the same project describes structure
inside that project, not a relationship between projects, so it is
excluded from the neighbor count entirely: it neither makes the project a
neighbor of itself nor inflates either total. Project-level coupling
counts only edges, direct project-to-project or module-mediated, whose
endpoint modules resolve to two different projects. A project whose
modules import only each other therefore reports zero fan_in and zero
fan_out at `level="project"`, even though the same modules carry nonzero
fan_in/fan_out at `level="module"`.

`code.coupling` returns an envelope, not a bare array: `level` echoes the
requested level, and `rows` carries the ordered result. Each row's
`entity_id` is the module's or project's entity UUID as a string; `name`
is its entity name as stored. Field names are fixed regardless of level:

```json
{
  "level": "module",
  "rows": [
    {
      "entity_id": "3f9a1c2e-8b7d-4e21-9c4a-6d1f2a8b5c30",
      "name": "khive_pack_code::vocab",
      "fan_in": 4,
      "fan_out": 2
    },
    {
      "entity_id": "9a2b7e10-4c5f-4d8a-8e2b-1f3c6a7d9e40",
      "name": "khive_pack_code::hook",
      "fan_in": 1,
      "fan_out": 3
    }
  ]
}
```

### E3: `code.health(db, top_n?)`

A single summary object over one map database, computed directly against
that database — not a call to the existing `stats()` verb, which reports
KG-substrate counts and has no target-database parameter:

- `entities_by_kind` — a map from entity-kind token to count, over the
  target database's own entities.
- `edges_by_relation` — a map from edge-relation token to count, over the
  target database's own edges.
- `coupling_outliers` — an array of coupling rows in E2's row shape
  (`entity_id`, `name`, `fan_in`, `fan_out`), reusing E2's computation at
  `level="module"` rather than a separate query; `code.health` takes no
  `level` parameter, so this array is always module-level. Its length is
  `min(top_n, available rows)`, `top_n` defaulting to 10 (max 100), the
  same availability-bounded behavior as `code.coupling` itself (E2).
- `dead_module_candidate_count` — the count of modules with zero incoming
  `depends_on` edges, scoped to modules actually present in the ingested
  set, the same scoping Amendment 2's B8 acceptance property 1 uses for dead
  symbols.
- `cyclic_component_count` — the number of cyclic components (E4), reusing
  E4's computation rather than a separate query.

`code.health` is a composition of `code.coupling` and `code.cycles` plus
counting; it introduces no computation beyond what those two verbs already
define, and its work is bounded the same way theirs is. `code.health`
opens one short read-only transaction and does two kinds of work inside
it, and nothing else: it runs `entities_by_kind` and `edges_by_relation` as
grouped SQL aggregates (`COUNT(*) ... GROUP BY` over every non-deleted
entity row and every non-deleted edge row respectively, filtered by E1's
soft-delete rule), and it loads a single compact module-graph projection,
module entity ids and names, module-to-module `depends_on` pairs, and
`project contains module` pairs, into memory. Neither step materializes
individual entity or edge rows beyond that projection: the aggregates
return only the small per-kind and per-relation count maps, never the rows
being counted, and the projection is small by construction (module-level
structure only, never declaration-level rows). The transaction commits as
soon as both the aggregates and the projection have been read. Every
subsequent step, the coupling-outlier ranking and the Tarjan pass behind
`cyclic_component_count`, runs entirely against that in-memory projection,
with no further database reads and no open transaction during the
CPU-heavy work. Because `entities_by_kind` and `edges_by_relation` are
grouped aggregates over every non-deleted row in the same reader, not
restricted to the relations `code.coupling` or `code.cycles` happen to
traverse, `edges_by_relation` counts every edge relation present in the
database, including ones neither of those two verbs ever visits (`extends`,
`implements`, or any relation added to the ontology after this amendment
ships), and does so from the same snapshot as every other field in the
response. Every field in the returned summary therefore derives from that
one transaction's snapshot: no field can reflect a write that landed after
the snapshot was taken, and the summary describes one coherent state,
never a mix, without holding a reader open across the aggregation and
cycle-detection work that follows. `db` is required, per E7.

At most one `code.health` or `code.cycles` call runs against a given `db`
at a time. Both verbs run the Tarjan pass that dominates their cost, so an
in-process guard, keyed on the resolved `db` path, admits one such call per
database and rejects a second with a busy error naming the database rather
than queuing it: the MCP `request` surface can dispatch up to 100 ops
concurrently in one batch, and a caller who fans out several `code.health`
or `code.cycles` calls at once against the same database needs to see that
contention immediately rather than have it silently absorbed by a queue.
`code.coupling`, which does no cycle detection, is not subject to this
guard. The guard releases as soon as the call it is holding for returns,
success or error alike.

`code.health`'s JSON response nests the two computed sub-results under
their own field names, matching each verb's own envelope shape where one
exists:

```json
{
  "entities_by_kind": { "module": 12, "function": 340, "datatype": 58, "interface": 9 },
  "edges_by_relation": { "depends_on": 512, "contains": 419, "implements": 61 },
  "coupling_outliers": [
    {
      "entity_id": "3f9a1c2e-8b7d-4e21-9c4a-6d1f2a8b5c30",
      "name": "khive_pack_code::vocab",
      "fan_in": 4,
      "fan_out": 2
    }
  ],
  "dead_module_candidate_count": 3,
  "cyclic_component_count": 1
}
```

### E4: `code.cycles(db, limit?, max_members?)`

`code.cycles` returns **cyclic components**, not enumerated simple cycles.
Each result item is one strongly connected component of size two or more
over the `concept/module -> depends_on -> concept/module` edge set (D3 #8),
or a self-loop component of size one (a module depending on itself).
Simple-cycle enumeration — every distinct cycle through a component's
members — is exponential in the worst case and is out of scope; a
component's presence proves at least one directed cycle runs through its
members, without claiming to enumerate all of them.

Each component is returned as an ordered list of module ids and names,
members ordered by entity id ascending. The top-level result list is
ordered by member_count descending, then by the smallest member id
ascending — a total order, for the same pagination-determinism reason as
E2.

Detection cost is linear in the size of the target database's module
graph — a single Tarjan pass over its `depends_on` edges, run once
regardless of how many components exist or how the caller bounds the
response. `limit` (default 20, max 100) and `max_members` (default 100,
max 1000) bound only the response's size, not detection: `limit` caps the
number of components serialized, and `max_members` caps how many members a
single component's member list serializes before truncating. A component
whose true member count exceeds `max_members` still reports its full
`member_count` and sets `truncated: true`; its serialized member list is
cut to the first `max_members` members in the component's own id-ascending
order.

`code.cycles`, called directly rather than through `code.health`, follows
the same transaction shape E3 describes: it opens one short read-only
transaction, loads the compact module-graph projection (module ids, names,
and `depends_on` pairs), commits, and runs Tarjan over the in-memory
projection with no open transaction during the pass. This is the same
projection E3 loads for its own `cyclic_component_count`, not a second
materialization strategy.

Each component in the response is an object, not a bare list, so
`truncated` and `member_count` can sit alongside the (possibly truncated)
member array:

```json
{
  "components": [
    {
      "member_count": 3,
      "truncated": false,
      "members": [
        { "entity_id": "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d", "name": "khive_pack_code::a" },
        { "entity_id": "2b3c4d5e-6f70-4b8c-9d0e-1f2a3b4c5d6e", "name": "khive_pack_code::b" },
        { "entity_id": "3c4d5e6f-7081-4c9d-0e1f-2a3b4c5d6e7f", "name": "khive_pack_code::c" }
      ]
    }
  ]
}
```

### E5: Cycle detection is in-process, not a query recipe

`code.cycles` is implemented as an in-process graph traversal (Tarjan's
algorithm or an equivalent strongly-connected-components computation) over
the map database's `depends_on` edges, not as a `query()` pattern the caller
composes themselves. This is a normative consequence of the query layer's
own design, not a convenience choice: `khive-query`'s validator
(`crates/khive-query/src/validate.rs`) rejects any GQL or SPARQL pattern
that repeats a node variable, with the rejection reason stated in the error
text itself as cycle/self-reachability detection, and the rejection is
locked by regression tests. A pattern that walks a module back to itself —
the shape a cyclic-component query needs — cannot be expressed in either
query language today. `code.cycles` exists because the gap is structural,
not because in-process computation was preferred over a documented query
recipe that could have been written instead.

### E6: Non-goals

- No declaration-level (L2) analysis semantics are defined here. `level` is
  a closed enum, `module` or `project`, on `code.coupling` only. `code.cycles`
  takes no `level` parameter at all: it is module-level only in v1 (E4), full
  stop. A `level` parameter for `code.cycles`, if ever wanted, is not this
  amendment's decision to make — it belongs to the future declaration-level
  amendment below, alongside the rest of that amendment's eligibility,
  aggregation, selectors, and health semantics for declaration entities.
  `code.ingest`'s L2 tier remains unimplemented; declaration-level analysis
  is deferred to that future amendment once L2 ships. Nothing here implies
  what that shape will be. Regardless of what else a target map database
  contains (a future L2's declaration-level entities, or ordinary `finding`
  notes), `code.coupling` filters to module-to-module `depends_on` edges
  only in v1 at `level="module"` — plus the project-to-project and
  module-mediated project handling defined in E2 for `level="project"` — and
  `code.cycles` filters to module-to-module `depends_on` edges only, with no
  project-level variant.
- No mutation. All three verbs are read-only, per E1.
- No scheduled or background analysis. Every call computes fresh over the
  database's current state at call time; there is no cached or
  incrementally-maintained analysis result.
- No default-pack-set change. The `code` pack's default-load status is
  unchanged from Amendment 3 (C1).
- No schema change. `SCHEMA_PLAN` remains `None`, as declared in D5.

### E7: Target database posture — the production-db fence is restated, hardened, not relaxed

`code.coupling`, `code.health`, and `code.cycles` resolve their `db`
parameter through the same db-target resolution `code.ingest` uses (B1,
B7), and each refuses the shared production database exactly as
`code.ingest` does: analysis over `khive.db` is rejected with no override
available on any of the three verbs. `db` is required on all three analysis
verbs — there is no `path`-derived default the way `code.ingest` has one
for `path` itself (B1), so an omitted `db` has nothing to derive a target
from and is rejected outright; resolution reuse with `code.ingest`
therefore applies to explicit caller-supplied values only.

The `db` parameter, on `code.ingest` and on all three analysis verbs
alike, must be an absolute, plain filesystem path. A value that begins
with the `file:` scheme prefix, or that contains a `?` character, is
rejected at parameter validation, before any filesystem probe and before
any identity comparison. This check exists because the storage layer's
backend constructors, general and read-only alike, open paths with
SQLite's URI interpretation available, so `file:/path/to/production.db`,
with or without a trailing `?mode=rw`-style query string, is a second,
equally valid way to name the production database that a plain-path
identity check would never see spelled that way. Rejecting the syntax
outright, rather than parsing or canonicalizing it, removes that alias
from consideration before the identity machinery below ever has to reason
about it, on every one of the four verbs that accept `db`.

The fence's boundary is the open call itself, not the checks that precede
it. Every file this fence governs, the target's main database file and any
`-journal`, `-wal`, or `-shm` companion, opens through a thin VFS wrapper
layered over the platform's default VFS, used for every one of these opens
without exception, on the analysis verbs and on `code.ingest` alike. Two
things happen at the moment that wrapper opens a file, both against the
open itself rather than against a path resolved earlier: the open uses
no-follow semantics, so a path that resolves through a symlink at open
time fails outright instead of transparently following it, and the
resulting file descriptor's (device, inode) identity is read directly off
that descriptor and compared against the production identity set (the
production database's own main file, plus whichever of its `-journal`,
`-wal`, and `-shm` companions presently exist). A match on any pairing
aborts the open before the caller's connection sees a byte of the file.
Because the check runs against the handle the connection is about to use,
not against a path string resolved at some earlier moment, there is no
window between validating a target and using it: no-follow closes the
symlink-swap variant of that window, and the descriptor identity check
closes the hard-link variant, since a hard link has no symlink for
no-follow to reject and would otherwise pass under a different name.

A read-only WAL-mode open can still create or update a `-shm` file, and
can create a `-wal` file, for the database being opened, even though no
logical write occurs; the identity check above does not depend on those
side effects being absent, only on the opened file's identity never
matching the production set. This is why the fence's byte-level
immutability guarantee, verified by E8's acceptance properties, applies to
the production database's own files, never to the target being analyzed:
the target's `-wal`/`-shm` sidecars may legitimately change as an ordinary
consequence of being opened for read, while the production database's main
file and sidecars must never change as a result of any analysis call,
because the open-time identity check guarantees the production files are
never the ones an analysis call actually opens.

A path-level identity probe, checking a resolved path's (device, inode)
identity before any open is attempted, remains in the sequence below, but
only as a fast-fail courtesy: rejecting
an obviously protected target before paying for a VFS-level open attempt
is cheap and surfaces the same error sooner in the common case. It is no
longer the mechanism the fence's correctness depends on. Opening a
resolved `db` target for analysis now follows a fixed sequence, and every
step is a rejection point:

1. The `db` value must be an absolute, plain filesystem path, not a URI,
   per the plain-path rule above.
2. The target file must already exist. A `db` path that resolves to
   nothing is rejected outright, an analysis call never creates the
   database it was asked to read.
3. As a fast-fail courtesy, the target's main file and each present
   `-wal`/`-shm` sidecar are checked by path, before any file is opened:
   each must be a regular file, a symlink is rejected outright, not
   resolved and compared, and each file's (device, inode) identity is
   probed directly off the filesystem path and compared against the
   production database's own main-file and sidecar identities. A match on
   any pairing, main-to-main, main-to-sidecar, or sidecar-to-sidecar,
   aborts with the fence error before any connection is attempted.
4. The file, and every `-journal`/`-wal`/`-shm` companion SQLite opens
   alongside it, is opened through the no-follow VFS wrapper described
   above, which re-runs the regular-file and (device, inode) checks
   against the opened descriptor regardless of what step 3 already
   concluded, catching a target swapped to a symlink or a hard link of a
   protected file after step 3 passed. The open goes through the storage
   layer's read-only backend constructor class, the one that opens with
   `SQLITE_OPEN_READ_ONLY` plus `query_only`, refuses to create a missing
   file, and runs no migrations, never through the general runtime
   constructor. That general constructor is create-capable (a missing
   path is created, not rejected) and unconditionally runs migrations
   against whatever it opens; migrations are schema writes, and a
   read-only analysis call has no business mutating a map database's
   schema. Migrations are prohibited on the analysis path for exactly this
   reason: the read-only backend constructor neither creates a missing
   file nor runs migrations, which is the posture steps 2 and 4 both need,
   and the general constructor provides neither guarantee.

Because path resolution, the plain-path rule, and the no-follow VFS
wrapper are shared with `code.ingest` (B1, B7), `code.ingest` gains the
same hardening: its `db` parameter is rejected at the same URI-syntax
check, and every file it opens for its own writes goes through the same
no-follow, identity-checked wrapper. This amendment does not introduce a
second resolution path, it strengthens the one B7 already established. The
existence check (step 2) is analysis-only: `code.ingest` remains
create-capable and never requires its target to pre-exist, and it still
opens its target through the general, create-capable constructor class for
its own writes, only the three analysis verbs are constrained to the
read-only backend constructor class.

The D6.1 granularity fence exists to keep exhaustive symbol/call graphs out
of the shared production graph; letting analysis verbs read the production
database would not itself violate that fence's storage rule, but it would
create a second path that depends on the fence never being violated
elsewhere, which is exactly the posture B7 already rejected once for
writes. Restating the same fence for reads, with the plain-path rule and
the open-time VFS identity check as the boundary, keeps the rule uniform
across the pack's entire verb surface: `db` always means a dedicated map
database, spelled as a plain path and verified by identity at the moment
it is actually opened, not just inferred from the path the caller happened
to supply.

### E8: Acceptance

An implementation of this amendment is acceptance-tested against fifteen
properties:

1. **Coupling correctness.** `code.coupling` run against a small fixture
   database with hand-counted `depends_on` edges returns fan_in/fan_out
   values matching the hand count, at both `level="module"` and
   `level="project"`.
2. **Project self-coupling exclusion.** A fixture project whose modules
   depend only on each other, with no `depends_on` edge crossing to
   another project's modules, reports `fan_in=0` and `fan_out=0` for that
   project at `level="project"`, even though the same modules report
   nonzero fan_in/fan_out at `level="module"` (E2).
3. **Cycle detection, positive and negative.** `code.cycles` run against a
   synthetic fixture containing a 3-module cyclic component (`A depends_on
   B depends_on C depends_on A`) returns exactly that component; run
   against a synthetic fixture whose module graph is a DAG, it returns an
   empty result.
4. **Production-db fence.** All three verbs, called with `db` explicitly
   pointed at the shared production database, are rejected with the same
   error class `code.ingest` uses for the same condition (B7). An omitted
   `db` is a separate, ordinary missing-required-parameter rejection (see
   E7), not this fence error.
5. **Plain-path rejection.** All four verbs that accept `db`
   (`code.ingest` included), called with a `db` value that begins with the
   `file:` scheme prefix or contains a `?` character and names the
   production database under that spelling, are rejected at parameter
   validation, before any filesystem probe, with the same fence error as
   property 4 (E7).
6. **Hard-link rejection.** A hard link to the production database file,
   opened as an explicit `db` target under a different path, is rejected
   with the same fence error as property 4, the opened-file (device,
   inode) identity check (E7) catches what the path-based courtesy check
   alone would miss. The same rejection applies when only a sidecar is
   linked: a fixture whose target's `-shm` file is hard-linked or
   symlinked to the production database's `-shm` file is rejected before
   any connection is made, even though the target's own main `.db` file is
   a distinct, unlinked file (E7 step 3).
7. **Open-time closure of the preflight window.** A fixture whose `db`
   target passes the path-level courtesy check (E7 step 3) cleanly, and
   whose `-wal` or `-shm` sidecar is then replaced with a symlink or a
   hard link to the corresponding production sidecar before the file is
   actually opened, is still rejected with the fence error: the no-follow
   VFS wrapper's open-time identity check (E7 step 4) runs against the
   opened descriptor regardless of what the courtesy check already
   concluded, so a target that mutates between the two steps does not slip
   through.
8. **Missing-file rejection.** All three analysis verbs, called with `db`
   pointed at a path that does not exist, are rejected outright, and no
   file is created at that path as a side effect of the call (E7 step 2).
9. **Byte identity of the production database across an analysis call.**
   For the production database plus its `-wal` and `-shm` sidecars, a
   byte-for-byte snapshot taken immediately before and immediately after a
   `code.coupling`, `code.health`, or `code.cycles` call against an
   unrelated target database is identical across all three production
   files. This property is scoped to the production database's own files,
   not the target's: E7 permits a read-only WAL-mode open to create or
   update the _target's own_ `-wal`/`-shm` sidecars as an ordinary side
   effect of opening it, so byte-for-byte identity cannot be demanded of
   the target across a call. What the fence guarantees, and what this
   property verifies, is that the production database's files never
   change, regardless of which map database a given call analyzes.
10. **Tombstone exclusion.** A fixture containing one soft-deleted edge and
    one soft-deleted entity, each of which would otherwise participate in
    a coupling number, a dead-module count, an aggregation, or a cyclic
    component, is confirmed to influence none of them, across all three
    verbs (E1).
11. **Health snapshot coherence under concurrent ingest.** `code.health` run
    against a database that a concurrent `code.ingest` call is actively
    mutating returns a summary whose fields all agree with one database
    state: none of `entities_by_kind`, `edges_by_relation`,
    `coupling_outliers`, `dead_module_candidate_count`, or
    `cyclic_component_count` reflects a partial mix of pre- and mid-ingest
    state (E3).
12. **Busy error on concurrent analysis.** A `code.health` or `code.cycles`
    call issued against a `db` while another `code.health` or
    `code.cycles` call against that same `db` is still in flight fails
    immediately with a busy error naming the database, rather than
    blocking until the first call finishes. A concurrent call against a
    _different_ `db`, or a concurrent `code.coupling` call against the
    same `db`, is unaffected (E3).
13. **`edges_by_relation` completeness for an untraversed relation.** A
    fixture database containing an edge relation that neither
    `code.coupling` nor `code.cycles` ever visits in their own traversals
    (for example, an `implements` edge) still contributes to
    `code.health`'s `edges_by_relation` count for that relation, because
    the field is a grouped aggregate over every non-deleted edge row in
    the same reader, not limited to the relations the other two verbs
    traverse (E3).
14. **Deterministic ordering and top-N prefix extension.** Two consecutive
    calls to the same verb with the same `db` and the same other
    parameters, with no intervening write, return identical,
    identically-ordered results (idempotent reads); and for
    `code.coupling`, a call with a larger `top_n` returns, as a strict
    prefix, exactly the ordered rows a call with a smaller `top_n`
    returned over the same rows, up to the number of rows actually
    available, verifying the total order defined in E2 together with its
    min(requested, available) bound. A fixture with fewer modules than the
    smaller `top_n` returns every available row from both calls, with no
    invented rows and no error.
15. **Truncated giant component.** `code.cycles` run against a fixture whose
    cyclic component exceeds `max_members` returns that component with
    `truncated: true`, a member list capped at `max_members`, and a
    `member_count` equal to the component's true, untruncated size.

### E9: Interface note — verb count delta

The MCP verb table in ADR-023 and the verb counts in `AGENTS.md` and this
repository's `CLAUDE.md` change when the implementation PR for this
amendment lands, not in this docs-only PR. The expected delta is **+3**
verbs (`code.coupling`, `code.health`, `code.cycles`) added to the pack's
existing one (`code.ingest`), against the count current as of Amendment 2's
acceptance (79 verbs on the default pack set) — the implementation PR
should cite the then-current count at merge time, since intervening PRs may
have changed it.
