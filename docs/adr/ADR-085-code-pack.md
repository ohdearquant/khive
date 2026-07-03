# ADR-085: Code Pack — Source-Code Ontology and Audit-Finding Vocabulary

**Status**: Proposed\
**Date**: 2026-07-03\
**Authors**: Ocean, lambda:khive (advisor-drafted)\
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
corpus this fleet works in daily — has no equivalent. Every session re-derives the same
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
test on all three ingredients, and the fleet's two concrete, recurring pains (comprehension
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

## Open Questions (for Ocean / lambda:leo)

1. Is the audit-finding lane (D4) in scope for v1, or should this ADR ship code-ontology-only
   (D1-D3, D5, D6) and defer `finding` to a follow-up amendment? Advisor recommendation:
   include it — it rides shared CRUD at near-zero marginal mechanism cost and immediately
   makes the existing `audit_crate.py` harvest step's output graph-queryable.
2. Default-pack-set promotion timing (D5's opt-in-at-v1 posture) — confirm gated on one
   validated real ingest, or set an explicit earlier promotion trigger.
3. Confirm this ADR's reading of the original one-line ask ("we need a pack-code") as
   domain-ontology scope (Fork A: code-as-graph vocabulary) rather than an execution surface
   (Fork C, rejected here as a different risk class colliding with issue #373).

## Implementation

This ADR authorizes design, not code; implementation follows SPEC-GATE sign-off.

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
- `.khive/scripts/audit_crate.py` + `.khive/audits/**/findings.json` — the audit lane's
  existing record shape mapped by D4
